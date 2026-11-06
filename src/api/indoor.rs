//! 室内跑（sportType 由 policy 查表得出，通常为 1）。
//!
//! 权威来源：`-[SWIndoorSportGradeBuilder buildGrade]` @0x1002011f0
//! 与 `-[SWRunGradeManager _uploadIndoorGradeWithUserInfo:]` @0x100049c90。
//!
//! 与室外自由跑（`SWRunGradeBuilder`）是**两条完全独立的链**：
//!
//! | 项 | 室外 | 室内 |
//! |---|---|---|
//! | builder | `SWRunGradeBuilder` | `SWIndoorSportGradeBuilder` |
//! | 入口 | `uploadGrade` → `buildGradeWithCompletion:` | `uploadIndoorGrade` → `buildGrade`（同步版） |
//! | 端点 | `/api/v70260/runnings/save/record` | **`/api/v68/indoor/save/record`** |
//! | body | 38 键 | **18 键**，无轨迹/距离 |
//! | 计量 | 距离 + 时间 + 步数 | **纯步数** |
//! | 签名 | `signObjectWithoutEncoding:` | **`signObject:`（percent-encoded）** |
//!
//! 室内上传**没有** `isVaildTimeRange:` 对时闸门（`uploadIndoorGrade` 里不存在该调用）。

use serde_json::{json, Map, Value};

use super::client::{ApiClient, IOS_TIMEOUT_SECS};

pub const INDOOR_RECORD_PATH: &str = "/api/v68/indoor/save/record";

/// `record.policy` → 提交体 `policy`（`dword_1045C6368`）。
const POLICY_TABLE: [i64; 7] = [0, 1, 2, 0, 0, 0, 0];
/// `record.policy` → 提交体 `sportType`（`qword_1045C6388`）。
const SPORT_TYPE_TABLE: [i64; 7] = [1, 1, 1, 3, 4, 5, 6];

/// 表外索引一律取 -1（真机 `if (policy > 6) → -1`）。
fn table_lookup(table: &[i64; 7], policy: i64) -> i64 {
    if (0..7).contains(&policy) {
        table[policy as usize]
    } else {
        -1
    }
}

pub fn policy_for(record_policy: i64) -> i64 {
    table_lookup(&POLICY_TABLE, record_policy)
}

pub fn sport_type_for(record_policy: i64) -> i64 {
    table_lookup(&SPORT_TYPE_TABLE, record_policy)
}

/// `sub_1002017EC`：室内未达标原因。`0` 达标 / `10` 时长不足 / `11` 步数不足。
///
/// ```text
/// if (minRunTime < 1 || totalTime >= minRunTime) {
///     if (minSteps < 1 || totalSteps >= minSteps) return 0;
///     else return 11;
/// } else return 10;
/// ```
pub fn uncomplete_reason(
    total_time: i64,
    total_steps: i64,
    min_run_time: i64,
    min_steps: i64,
) -> i64 {
    if min_run_time < 1 || total_time >= min_run_time {
        if min_steps < 1 || total_steps >= min_steps {
            0
        } else {
            11
        }
    } else {
        10
    }
}

/// `avgStepFreq` = `totalSteps / (totalTime / 60.0)`，向零取整。
/// `totalTime <= 0` 时真机不做除零保护，这里给 0 以免产生 inf 转整的未定义结果。
pub fn avg_step_freq(total_steps: i64, total_time: i64) -> i64 {
    if total_time <= 0 {
        return 0;
    }
    (total_steps as f64 / (total_time as f64 / 60.0)) as i64
}

pub struct IndoorBodyInput {
    pub uuid: String,
    pub selected_unid: String,
    /// `SWIndoorSportRecord.policy`（**不是**提交体里的 policy，需经表映射）。
    pub record_policy: i64,
    pub start_ms: i64,
    pub stop_ms: i64,
    pub total_time: i64,
    pub total_steps: i64,
    /// 定位，真机用 `"%.6f"` 字符串。
    pub latitude: f64,
    pub longitude: f64,
    /// `+[SWRunConfigDataManager indoorRunRule]` 的三个字段。
    pub min_steps: i64,
    pub min_run_time: i64,
    pub max_run_time: i64,
}

/// 室内 body 的键集（18 键）。签名时 `signature`/`stepsPerTenSec` 尚未写入，
/// 因此**参与签名的只有前 16 键**。
pub const INDOOR_BODY_KEYS: [&str; 18] = [
    "avgStepFreq", "complete", "latitude", "longitude", "maxRunTime", "minSteps", "policy",
    "selectedUnid", "selRunTime", "sportType", "startTime", "stopTime", "totalSteps",
    "totalTime", "unCompleteReason", "uuid", "signature", "stepsPerTenSec",
];

/// 构造室内提交体。
///
/// 插入顺序与真机一致；`signature` 用**编码版** query 计算，
/// `stepsPerTenSec` 在签名**之后**才以空数组写入（真机就是这样）。
pub fn build_indoor_body(i: &IndoorBodyInput, salt: &str) -> Map<String, Value> {
    let reason = uncomplete_reason(i.total_time, i.total_steps, i.min_run_time, i.min_steps);
    let mut body = Map::new();
    body.insert(
        "avgStepFreq".into(),
        Value::from(avg_step_freq(i.total_steps, i.total_time)),
    );
    body.insert(
        "complete".into(),
        Value::String(if reason == 0 { "true" } else { "false" }.into()),
    );
    body.insert("latitude".into(), Value::String(format!("{:.6}", i.latitude)));
    body.insert(
        "longitude".into(),
        Value::String(format!("{:.6}", i.longitude)),
    );
    body.insert("maxRunTime".into(), Value::from(i.max_run_time));
    body.insert("minSteps".into(), Value::from(i.min_steps));
    body.insert("policy".into(), Value::from(policy_for(i.record_policy)));
    body.insert(
        "selectedUnid".into(),
        Value::String(i.selected_unid.clone()),
    );
    body.insert("selRunTime".into(), Value::from(i.min_run_time));
    body.insert(
        "sportType".into(),
        Value::from(sport_type_for(i.record_policy)),
    );
    body.insert("startTime".into(), Value::from(i.start_ms));
    body.insert("stopTime".into(), Value::from(i.stop_ms));
    body.insert("totalSteps".into(), Value::from(i.total_steps));
    body.insert("totalTime".into(), Value::from(i.total_time));
    body.insert("unCompleteReason".into(), Value::from(reason));
    body.insert("uuid".into(), Value::String(i.uuid.clone()));

    // 签名：`signObject:suffix:` 作用在**尚未含 signature/stepsPerTenSec** 的字典上。
    let sig = crate::crypto::sign::indoor_signature(&Value::Object(body.clone()), salt);
    body.insert("signature".into(), Value::String(sig));
    body.insert("stepsPerTenSec".into(), Value::Array(Vec::new()));
    assert_indoor_key_set(&body);
    body
}

/// 运行期护栏：键集偏离 IPA 立即硬失败（与室外 body 同策略）。
fn assert_indoor_key_set(body: &Map<String, Value>) {
    debug_assert_eq!(body.len(), INDOOR_BODY_KEYS.len());
    for k in INDOOR_BODY_KEYS {
        assert!(body.contains_key(k), "室内 body 缺少 IPA 键 {k}");
    }
}

/// 提交室内记录。返回业务 `data`。
pub fn submit_indoor_record(
    client: &mut ApiClient,
    body_plain: &str,
    log: &mut dyn FnMut(&str),
) -> Result<Value, String> {
    let env = crate::crypto::envelope::build_envelope(
        &mut client.session,
        body_plain,
        crate::crypto::envelope::OuterOrder::Insert,
    );
    let (hp, _extra) = crate::crypto::header::build_native_ios_header(
        &client.identity,
        client.login.as_ref().map(|s| s.uid).unwrap_or(0),
        client.login.as_ref().map(|s| s.token.as_str()).unwrap_or(""),
        None,
    );
    let header_env = crate::crypto::envelope::build_envelope(
        &mut client.session,
        &hp,
        crate::crypto::envelope::OuterOrder::Observed,
    );
    let ua = crate::crypto::header::ua_ios_for(&client.identity.os_version);
    let req = super::client::apply_ios_headers(
        client.agent.post(&format!("{}{}", super::model::HOST, INDOOR_RECORD_PATH)),
        &ua,
        &header_env.json,
        true,
    );
    let _ = IOS_TIMEOUT_SECS;
    let resp = req
        .send_string(&env.json)
        .map_err(|e| format!("室内提交失败: {e}"))?;
    let status = resp.status();
    let raw = resp.into_string().unwrap_or_default();
    log(&format!("[indoor] HTTP {status} len={}", raw.len()));
    let biz: Value = serde_json::from_str(&raw).map_err(|e| format!("室内响应非 JSON: {e}"))?;
    Ok(json!({ "http": status, "business": biz }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> IndoorBodyInput {
        IndoorBodyInput {
            uuid: "INDOOR-UUID".into(),
            selected_unid: "47203".into(),
            record_policy: 0,
            start_ms: 1_700_000_000_000,
            stop_ms: 1_700_001_200_000,
            total_time: 1200,
            total_steps: 2100,
            latitude: 22.4169,
            longitude: 112.9472,
            min_steps: 1500,
            min_run_time: 600,
            max_run_time: 7200,
        }
    }

    /// 18 键、顺序无关但集合必须一致；`stepsPerTenSec` 恒为空数组。
    #[test]
    fn body_has_ipa_key_set() {
        let b = build_indoor_body(&sample(), "SALT");
        assert_eq!(b.len(), 18);
        for k in INDOOR_BODY_KEYS {
            assert!(b.contains_key(k), "缺 {k}");
        }
        assert_eq!(b["stepsPerTenSec"].as_array().unwrap().len(), 0);
        // 绝不能出现室外的字段
        for k in ["allLocJson", "fivePointJson", "totalDis", "calorie", "avgPower", "address"] {
            assert!(!b.contains_key(k), "室内 body 不得含 {k}");
        }
    }

    /// 类型：经纬度是 `"%.6f"` 字符串；selectedUnid/uuid 字符串；其余数字。
    #[test]
    fn body_field_types_match_ipa() {
        let b = build_indoor_body(&sample(), "SALT");
        assert!(b["latitude"].is_string(), "latitude 必须是字符串");
        assert_eq!(b["latitude"], "22.416900");
        assert_eq!(b["longitude"], "112.947200");
        assert!(b["selectedUnid"].is_string());
        assert!(b["uuid"].is_string());
        assert!(b["signature"].is_string());
        assert!(b["complete"].is_string());
        for k in ["avgStepFreq", "maxRunTime", "minSteps", "policy", "selRunTime",
                  "sportType", "startTime", "stopTime", "totalSteps", "totalTime",
                  "unCompleteReason"] {
            assert!(b[k].is_number(), "{k} 必须是数字");
        }
    }

    /// policy/sportType 来自查表，表外索引取 -1。
    #[test]
    fn policy_and_sport_type_come_from_tables() {
        assert_eq!(policy_for(0), 0);
        assert_eq!(policy_for(1), 1);
        assert_eq!(policy_for(2), 2);
        assert_eq!(policy_for(3), 0);
        assert_eq!(sport_type_for(0), 1);
        assert_eq!(sport_type_for(2), 1);
        assert_eq!(sport_type_for(3), 3);
        assert_eq!(sport_type_for(6), 6);
        assert_eq!(policy_for(7), -1);
        assert_eq!(sport_type_for(99), -1);

        let mut i = sample();
        i.record_policy = 2;
        let b = build_indoor_body(&i, "S");
        assert_eq!(b["policy"], 2);
        assert_eq!(b["sportType"], 1);
    }

    /// 未达标原因：0 达标 / 10 时长不足 / 11 步数不足。
    #[test]
    fn uncomplete_reason_matches_ipa_branches() {
        // 都达标
        assert_eq!(uncomplete_reason(1200, 2100, 600, 1500), 0);
        // 时长不足优先
        assert_eq!(uncomplete_reason(300, 2100, 600, 1500), 10);
        // 时长够、步数不足
        assert_eq!(uncomplete_reason(1200, 100, 600, 1500), 11);
        // 阈值为 0/负数视为不限制
        assert_eq!(uncomplete_reason(10, 0, 0, 0), 0);
        assert_eq!(uncomplete_reason(1200, 0, 0, 1500), 11);

        let mut i = sample();
        i.total_steps = 10;
        let b = build_indoor_body(&i, "S");
        assert_eq!(b["complete"], "false");
        assert_eq!(b["unCompleteReason"], 11);
    }

    /// 签名只覆盖前 16 键（`signature`/`stepsPerTenSec` 在签名之后写入）。
    #[test]
    fn signature_covers_exactly_the_sixteen_pre_sign_keys() {
        let b = build_indoor_body(&sample(), "SALT");
        // 复算：去掉 signature 与 stepsPerTenSec 后按编码版 query 签名
        let mut pre = b.clone();
        pre.remove("signature");
        pre.remove("stepsPerTenSec");
        assert_eq!(pre.len(), 16, "参与签名的必须是 16 键");
        let expect = crate::crypto::sign::indoor_signature(
            &serde_json::Value::Object(pre),
            "SALT",
        );
        assert_eq!(b["signature"].as_str().unwrap(), expect);
        // 盐不同则签名不同
        let other = build_indoor_body(&sample(), "OTHER");
        assert_ne!(b["signature"], other["signature"]);
    }

    /// 百分号编码：数字与经纬度串不受影响；中文/空格会变成大写 %XX。
    #[test]
    fn url_query_encoding_matches_foundation_allowed_set() {
        use crate::crypto::sign::join_query_ios_encoded;
        let kv = vec![
            ("latitude".to_string(), "22.416900".to_string()),
            ("selectedUnid".to_string(), "47203".to_string()),
        ];
        let q = join_query_ios_encoded(&kv);
        assert!(q.contains("latitude=22.416900"), "{q}");
        assert!(!q.contains('%'), "数字与点号不应被编码: {q}");

        let kv2 = vec![("k".to_string(), "中文 a".to_string())];
        let q2 = join_query_ios_encoded(&kv2);
        assert_eq!(q2, "k=%E4%B8%AD%E6%96%87%20a");
    }

    /// avgStepFreq = trunc(totalSteps / (totalTime/60))；totalTime<=0 时给 0。
    #[test]
    fn avg_step_freq_truncates() {
        assert_eq!(avg_step_freq(2100, 1200), 105); // 2100/20 = 105
        assert_eq!(avg_step_freq(2000, 1200), 100);
        assert_eq!(avg_step_freq(2101, 1200), 105); // 105.05 → 105（截断）
        assert_eq!(avg_step_freq(2100, 0), 0);
    }
}
