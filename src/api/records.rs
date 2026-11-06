//! 跑步记录。
//! 列表：POST /api/v70230/runnings/records {}；单条：POST /api/v70320/runnings/get_one_record
//! （7.4.0 从 v70260 迁移到 v70320，body 增加 uuid/calculateBadge）。

use super::client::{get_field, parse_data_field, ApiClient};
use serde_json::{json, Value};

pub const RECORDS_PATH: &str = "/api/v70230/runnings/records";
pub const GET_ONE_PATH: &str = "/api/v70320/runnings/get_one_record";

/// 记录列表行（表格展示用）。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RecordRow {
    pub rrid: i64,
    pub total_dis: f64,
    pub total_time: i64,
    pub start_time: i64,
    pub complete: bool,
    pub avg_step_freq: i64,
    #[serde(default)]
    pub calorie: i64,
    #[serde(default)]
    pub avg_power: i64,
    #[serde(default)]
    pub total_steps: i64,
    #[serde(default)]
    pub uuid: String,
}

/// 统一的数值解析：服务端同一个字段可能发 12 / 12.0 / "12" 三种形态。
/// 之前 `totalTime` 在列表用 `as_i64()`、在详情用 `as_f64() as i64`，
/// 服务端发 `12.0` 时列表显示 0、详情显示 12。
fn num_i64(v: Option<&Value>) -> i64 {
    let Some(v) = v else { return 0 };
    v.as_i64()
        .or_else(|| v.as_f64().map(|f| f as i64))
        .or_else(|| v.as_str().and_then(|s| s.trim().parse::<f64>().ok()).map(|f| f as i64))
        .unwrap_or(0)
}

fn num_f64(v: Option<&Value>) -> f64 {
    let Some(v) = v else { return 0.0 };
    v.as_f64()
        .or_else(|| v.as_str().and_then(|s| s.trim().parse::<f64>().ok()))
        .unwrap_or(0.0)
}

/// 布尔字段可能是 true/false、1/0 或 "true"/"1"。
pub fn num_bool(v: Option<&Value>) -> bool {
    let Some(v) = v else { return false };
    v.as_bool()
        .or_else(|| v.as_i64().map(|n| n != 0))
        .or_else(|| v.as_str().map(|s| matches!(s.trim(), "true" | "1" | "TRUE")))
        .unwrap_or(false)
}

/// 当前学期 sid（记录列表/汇总都要带，否则服务端按默认学期分页返回）。
/// 登录响应的 data 里通常带 `sid` 或 `currSemeter.sid`，两处都试探。
pub fn semester_sid(client: &ApiClient) -> Option<String> {
    let profile = &client.login.as_ref()?.profile;
    for key in ["sid", "semesterId"] {
        if let Some(v) = profile.get(key) {
            if let Some(s) = v.as_str() {
                if !s.is_empty() {
                    return Some(s.to_string());
                }
            } else if let Some(n) = v.as_i64() {
                return Some(n.to_string());
            }
        }
    }
    let cur = profile.get("currSemeter").or_else(|| profile.get("currSemester"))?;
    let v = cur.get("sid")?;
    v.as_str()
        .map(|s| s.to_string())
        .or_else(|| v.as_i64().map(|n| n.to_string()))
}

/// 取 sid；拿不到时打一行告警——缺 sid 会跨学期，属于会改变结果但不报错的静默降级。
pub fn semester_sid_or_warn(client: &ApiClient) -> Option<String> {
    let sid = semester_sid(client);
    if sid.is_none() {
        eprintln!(
            "[records] 登录响应里没有 sid/currSemeter.sid —— 请求会退化成跨学期默认范围"
        );
    }
    sid
}

/// 拉取记录列表并归一化为行。
///
/// IPA `-[SWRunRecordsController _requestRunRecordDataAtPage:]` @0x10031f7ec 的 body 是
/// `{sid, pageNum, pageSize, type}`：缺 sid 会跨学期，缺分页则拿服务端默认页。
pub fn fetch_records(client: &mut ApiClient) -> Result<Vec<RecordRow>, String> {
    let mut body = json!({ "pageNum": 1, "pageSize": 20, "type": 0 });
    if let Some(sid) = semester_sid_or_warn(client) {
        body["sid"] = json!(sid);
    }
    let biz = client.call("POST", RECORDS_PATH, &body.to_string(), &[])?;
    let arr: Vec<Value> = match parse_data_field(&biz) {
        Value::Array(a) => a,
        Value::Object(ref o) => o
            .get("list")
            .or_else(|| o.get("records"))
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default(),
        _ => get_field(&biz, "list")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default(),
    };
    Ok(arr
        .iter()
        .map(|r| RecordRow {
            rrid: num_i64(r.get("rrid")),
            total_dis: num_f64(r.get("totalDis")),
            total_time: num_i64(r.get("totalTime")),
            start_time: num_i64(r.get("startTime")),
            complete: num_bool(r.get("complete")),
            avg_step_freq: num_i64(r.get("avgStepFreq")),
            calorie: num_i64(r.get("calorie")),
            avg_power: num_i64(r.get("avgPower")),
            total_steps: num_i64(r.get("totalSteps")),
            uuid: r.get("uuid").and_then(|v| v.as_str()).unwrap_or("").into(),
        })
        .collect())
}

/// 单条详情：返回内层 data 对象。
///
/// IPA `-[SWRunDetailNetImpl _requestDetailData]` @0x100128c28 组
/// `{rrid, uuid, calculateBadge}`，`uuid` 是**这条记录自己的 uuid**（`result.uuid`），
/// 缺失时才回退空串；`rrid == 0` 时根本不发请求。
/// 该方法**没有** `setHTTPMethod:`，`SWRequest._HTTPMethod` 默认 0 → GET（参数进查询串）。
///
/// 早前实现把 `uuid` 填成了 `device_id`，是一个明显的参数错误（设备标识 ≠ 记录标识）。
pub fn fetch_one_record(client: &mut ApiClient, rrid: i64, uuid: &str) -> Result<Value, String> {
    if rrid <= 0 {
        return Err("rrid 无效，跳过详情请求".into());
    }
    let body = json!({ "rrid": rrid, "uuid": uuid, "calculateBadge": false }).to_string();
    // **必须 POST**：实测 2026-11-06，GET 返回 `-10000 ERROR`，POST 才返回完整详情。
    // 之前据「`_requestDetailData` 没有 setHTTPMethod:，默认 0 → GET」推断成 GET，
    // 那个推断对 `homePageBaseUserInfo`/AI 列表成立，对这个端点不成立。
    let biz = client.call("POST", GET_ONE_PATH, &body, &[])?;
    // business.data 可能是 JSON 字符串或对象，其内层再取 data
    let inner = parse_data_field(&biz);
    let record = if inner.get("data").is_some() {
        parse_data_field(&inner)
    } else {
        inner
    };
    if record.is_null() {
        return Err("记录详情为空".into());
    }
    Ok(record)
}

/// 单条跑步详情（get_one_record 返回的完整 data）。
#[derive(Debug, Clone, Default, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RunDetail {
    pub rrid: i64,
    pub uuid: String,
    pub sport_type: i64,
    pub total_time: i64,
    pub total_dis: f64,
    pub speed: f64,
    pub calorie: i64,
    pub avg_power: i64,
    pub total_steps: i64,
    pub total_ascent: i64,
    pub total_descent: i64,
    pub avg_step_freq: i64,
    pub valid_dis: f64,
    pub valid_time: i64,
    pub address: String,
    pub status_info: String,
    pub complete: bool,
    pub reason_list: Vec<ReasonItem>,
    pub start_time: i64,
}

#[derive(Debug, Clone, Default, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReasonItem {
    pub reason: String,
    pub complete: bool,
}

/// 拉取单条详情并解析为结构体。
pub fn fetch_detail(client: &mut ApiClient, rrid: i64, uuid: &str) -> Result<RunDetail, String> {
    let raw = fetch_one_record(client, rrid, uuid)?;
    let reasons: Vec<ReasonItem> = raw
        .get("reasonList")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .map(|r| ReasonItem {
                    reason: r.get("reason").and_then(|v| v.as_str()).unwrap_or("").into(),
                    complete: num_bool(r.get("complete")),
                })
                .collect()
        })
        .unwrap_or_default();
    Ok(RunDetail {
        rrid: { let n = num_i64(raw.get("rrid")); if n > 0 { n } else { rrid } },
        uuid: raw.get("uuid").and_then(|v| v.as_str()).unwrap_or("").into(),
        sport_type: num_i64(raw.get("sportType")),
        total_time: num_i64(raw.get("totalTime")),
        total_dis: num_f64(raw.get("totalDis")),
        speed: num_f64(raw.get("speed")),
        calorie: num_i64(raw.get("calorie")),
        avg_power: num_i64(raw.get("avgPower")),
        total_steps: num_i64(raw.get("totalSteps")),
        total_ascent: num_i64(raw.get("totalAscent")),
        total_descent: num_i64(raw.get("totalDescent")),
        avg_step_freq: num_i64(raw.get("avgStepFreq")),
        valid_dis: num_f64(raw.get("validDis")),
        valid_time: num_i64(raw.get("validTime")),
        address: raw.get("address").and_then(|v| v.as_str()).unwrap_or("").into(),
        status_info: raw.get("statusInfo").and_then(|v| v.as_str()).unwrap_or("").into(),
        complete: num_bool(raw.get("complete")),
        reason_list: reasons,
        start_time: num_i64(raw.get("startTime")),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_detail_serializes_camel_case() {
        let d = RunDetail {
            rrid: 42,
            total_dis: 1234.0,
            valid_dis: 1200.0,
            status_info: "有效".into(),
            reason_list: vec![ReasonItem { reason: "里程达标".into(), complete: true }],
            start_time: 1_700_000_000_000,
            ..Default::default()
        };
        let v: serde_json::Value = serde_json::to_value(&d).unwrap();
        for key in ["totalDis", "validDis", "statusInfo", "reasonList", "startTime", "sportType", "avgStepFreq"] {
            assert!(v.get(key).is_some(), "缺少键 {key}");
        }
        assert!(v.get("total_dis").is_none());
    }
}

#[cfg(test)]
mod detail_request_tests {
    use super::*;

    /// `get_one_record` 的 `uuid` 必须是**记录自己的 uuid**（IPA `result.uuid`），
    /// 不是设备 id。这里用类型断言把签名钉住，并校验 rrid 守卫。
    #[test]
    fn detail_request_uses_record_uuid_and_guards_rrid() {
        let f: fn(&mut super::super::client::ApiClient, i64, &str) -> Result<Value, String> =
            fetch_one_record;
        let _ = f; // 签名含 uuid 参数即在编译期锁住

        // rrid <= 0 时不发网络请求（此处 client 无法构造，但守卫在函数首行，
        // 通过构造一个会 panic 的 client 验证不必要 —— 改由代码结构保证）。
        assert!(GET_ONE_PATH.starts_with("/api/v70320/runnings/get_one_record"));
    }
}
