//! 跑步策略：POST /api/v70103/runModePolicy。
//! 返回 data.runRuleModel.minDistance（提交时 selDistance 用它）与 data.policy。

use super::client::{get_field, ApiClient};
use serde_json::{json, Value};

pub const POLICY_PATH: &str = "/api/v70103/runModePolicy";

pub struct PolicyInfo {
    pub timestamp: i64,
    pub policy: i64,
    pub min_distance: i64,
    /// runRuleModel.minRunTime：iOS buildGrade 的 selRunTime 直接用它（不是 totalTime）。
    pub min_run_time: i64,
    /// runRuleModel.faceVerify：本场地是否启用真人核验。
    /// iOS buildGrade 在无 faceCheckTypes 时发 faceCheck=0，故此处为 false 时应发 0。
    pub face_verify: bool,
    /// `runRuleModel.validTime` 的有效时间窗（服务端唯一权威来源）。
    ///
    /// IDA 实证：`+[SWRunRule mj_objectClassInArray]` @0x10015de80 把 `validTime`
    /// 声明成 `SWRunValidTime` 对象数组映射；`SWRunValidTime` 只有 `start` / `end`
    /// 两个属性。**它不是整数**，旧实现按 `as_i64()` 解析只会拿到 0。
    /// 窗口外的记录会被服务端拒绝（用户遇到的 11016 就是它），
    /// 而 UI 里一直写死 7:00–20:00 —— 现在以这里为准。
    pub valid_start: Option<String>,
    pub valid_end: Option<String>,
    /// `runRuleModel.speedBottom` / `speedTop`：服务端认可的速度窗口。
    /// `SWRunRule` 上是 `speedTop` / `speedBottom` 两个属性（@0x10015e008 / 0x10015e018）。
    pub speed_bottom: Option<f64>,
    pub speed_top: Option<f64>,
    /// `runRuleModel.stepBottom` / `stepTop`：认可的步频窗口。
    pub step_bottom: Option<f64>,
    pub step_top: Option<f64>,
    /// 必经点（**GCJ-02 系**，与上传体 `gLat/gLng`、轨迹生成同一坐标系）。
    /// **字段名是猜的**（见 `extract_must_points`），命中时 `must_points_key`
    /// 会带上实际字段名，便于核对。
    pub must_points: Vec<(f64, f64)>,
    /// 命中的响应字段名；None 表示没有匹配到任何候选键。
    pub must_points_key: Option<&'static str>,
    /// 本次**用了兜底值**的字段名。这些值会原样进提交体
    /// （`selDistance`/`selRunTime`/`faceCheck` 等），服务端没下发时必须让用户看见。
    pub defaults_used: Vec<&'static str>,
}

/// 数值/布尔字段的宽容解析：服务端同一个字段可能发 1 / 1.0 / "1" / true。
fn num_i64(v: Option<&Value>) -> Option<i64> {
    let v = v?;
    v.as_i64()
        .or_else(|| v.as_f64().map(|f| f as i64))
        .or_else(|| v.as_str().and_then(|s| s.trim().parse::<f64>().ok()).map(|f| f as i64))
}

fn num_f64(v: Option<&Value>) -> Option<f64> {
    let v = v?;
    v.as_f64()
        .or_else(|| v.as_str().and_then(|s| s.trim().parse::<f64>().ok()))
}

fn num_bool(v: Option<&Value>) -> Option<bool> {
    let v = v?;
    v.as_bool()
        .or_else(|| v.as_i64().map(|n| n != 0))
        .or_else(|| v.as_str().map(|s| matches!(s.trim(), "1" | "true" | "TRUE")))
}

/// policy 请求体（`+[SWRunConfigDataManager fetchRunPolicyInMode:handler:]` @0x1000a4888）。
///
/// 五个键的取值全部来自反编译：
/// - `runMode`      = 入参 mode（`numberWithInteger:`）
/// - `ruleUpdateTime`     = `[runRule updateTime]`，无缓存时 0（`numberWithLong:`）
/// - `geofenceUpdateTime` = `[geofenceConfig updateTime]`，无缓存时 0（`numberWithLong:`）
/// - `selectUnid`  = `[self selectedUnid]`，nil 时回退 `@""`（**字符串**）
/// - `operateType` = `numberWithInt:0`（**硬编码 0**）
pub fn policy_body(run_mode: i64, rule_update: i64, geofence_update: i64, select_unid: &str) -> Value {
    json!({
        "runMode": run_mode,
        "ruleUpdateTime": rule_update,
        "geofenceUpdateTime": geofence_update,
        "selectUnid": select_unid,
        "operateType": 0,
    })
}

/// 室内跑策略。
///
/// `+[SWRunConfigDataManager fetchIndoorRunConfigDatasWithHandler:]` @0x1000a4568
/// 走的是**同一个** `/api/v70103/runModePolicy`，只是 `runMode = 5`；
/// 响应解析 `sub_1000A4B7C` 里 `if (mode == 5) resetIndoorRunRule(rule) else resetRunRule(rule)`，
/// 两者都取 `data.runRuleModel`。
#[derive(Debug, Clone)]
pub struct IndoorPolicy {
    pub timestamp: i64,
    pub min_steps: i64,
    pub min_run_time: i64,
    pub max_run_time: i64,
    /// 缺失时用了兜底的字段名（会进提交体，必须让用户看见）。
    pub defaults_used: Vec<&'static str>,
}

/// 拉取室内策略（`runMode = 5`）。
pub fn fetch_indoor_policy(client: &mut ApiClient, unid: &str) -> Result<IndoorPolicy, String> {
    let body = policy_body(5, 0, 0, unid).to_string();
    let biz = client.call("POST", POLICY_PATH, &body, &[])?;
    let timestamp = num_i64(get_field(&biz, "timestamp")).ok_or("室内策略响应缺 timestamp")?;
    let rule = get_field(&biz, "runRuleModel").cloned().unwrap_or(Value::Null);
    let mut defaults: Vec<&'static str> = Vec::new();
    let min_steps = match num_i64(rule.get("minSteps")) {
        Some(v) => v,
        None => {
            defaults.push("minSteps=1500");
            1500
        }
    };
    let min_run_time = match num_i64(rule.get("minRunTime")) {
        Some(v) => v,
        None => {
            defaults.push("minRunTime=600");
            600
        }
    };
    let max_run_time = match num_i64(rule.get("maxRunTime")) {
        Some(v) => v,
        None => {
            defaults.push("maxRunTime=7200");
            7200
        }
    };
    Ok(IndoorPolicy {
        timestamp,
        min_steps,
        min_run_time,
        max_run_time,
        defaults_used: defaults,
    })
}

/// 解析 `runRuleModel` 里会**原样写回提交体**的几个字段，并记录用了兜底的项。
///
/// `selDistance` / `selRunTime` / `faceCheck` 都直接来自这里，服务端没下发时
/// 兜底值会被当成真值上传，所以必须把"用了兜底"这件事暴露给用户。
fn parse_rule_numbers(rule: &Value) -> (i64, i64, bool, Vec<&'static str>) {
    let mut defaults: Vec<&'static str> = Vec::new();
    let min_distance = match num_i64(rule.get("minDistance")) {
        Some(v) => v,
        None => {
            defaults.push("minDistance=1000");
            1000
        }
    };
    let min_run_time = match num_i64(rule.get("minRunTime")) {
        Some(v) => v,
        None => {
            defaults.push("minRunTime=300");
            300
        }
    };
    let face_verify = match num_bool(rule.get("faceVerify")) {
        Some(v) => v,
        None => {
            defaults.push("faceVerify=false");
            false
        }
    };
    if rule.get("validTime").is_none() {
        defaults.push("validTime=未下发");
    }
    (min_distance, min_run_time, face_verify, defaults)
}

/// 从 `runRuleModel.validTime` 取 `start` / `end`。
/// 兼容三种形态：`{start,end}` 对象、字符串、以及旧的整数（当作无窗口）。
/// 取 `runRuleModel.validTime` 的 `start` / `end`。
///
/// 真实服务端下发的是**数组**（实测 2026-11-06）：
/// ```json
/// "validTime":[{"start":"06:30:00","end":"22:00:00"}]
/// ```
/// 不是 `{start,end}` 对象。旧实现只按对象读，于是恒得 `None`、时间窗从不生效。
fn valid_time_field(rule: &Value, key: &str) -> Option<String> {
    let vt = rule.get("validTime")?;
    // 数组形态：取第一个元素（真机只下发一段）
    let vt = match vt {
        Value::Array(arr) => arr.first()?,
        other => other,
    };
    if let Some(s) = vt.get(key).and_then(|v| v.as_str()) {
        let t = s.trim();
        return if t.is_empty() { None } else { Some(t.to_string()) };
    }
    if let Some(n) = vt.get(key).and_then(|v| v.as_i64()) {
        // 少数环境下发分钟数，转成 HH:mm
        let (h, m) = ((n / 60).rem_euclid(24), n.rem_euclid(60));
        return Some(format!("{h:02}:{m:02}"));
    }
    None
}

/// 把 `HH:mm` / `HH:mm:ss` / `YYYY-MM-DD HH:mm:ss` 解析成「当天第几分钟」。
pub fn time_of_day_minutes(text: &str) -> Option<i64> {
    let time_part = text.rsplit(' ').next().unwrap_or(text);
    let mut it = time_part.split(':');
    let h: i64 = it.next()?.trim().parse().ok()?;
    let m: i64 = it.next()?.trim().parse().ok()?;
    if !(0..=23).contains(&h) || !(0..=59).contains(&m) {
        return None;
    }
    Some(h * 60 + m)
}

/// 必经点坐标 → GCJ-02。
///
/// 与 `fence::point_xy`（BD 优先）相反：轨迹和上传体都在 GCJ 系，
/// 优先取 `glat/glon`；只有 BD 字段时才换算。全 0 视为缺失。
fn must_point_gcj(p: &Value) -> Option<(f64, f64)> {
    let get = |keys: &[&str]| -> Option<f64> {
        keys.iter().find_map(|k| {
            let v = p.get(*k)?;
            v.as_f64()
                .or_else(|| v.as_str().and_then(|s| s.trim().parse::<f64>().ok()))
        })
    };
    let (ga, go) = (get(&["glat"]), get(&["glon"]));
    if let (Some(a), Some(o)) = (ga, go) {
        if !(a == 0.0 && o == 0.0) {
            return Some((a, o));
        }
    }
    let (lat, lon) = (get(&["lat", "latitude"]), get(&["lon", "lng", "longitude"]));
    let (lat, lon) = (lat?, lon?);
    if lat == 0.0 && lon == 0.0 {
        return None;
    }
    Some(crate::track::wire::bd09_to_gcj02(lat, lon))
}

/// 从 policy 响应 `data` 中防御式提取必经点列表（字段名不确定，逐个尝试）。
///
/// 注意：真机的"必经点"其实是 `/api/v560/get/1/distance/1` 下发点位里 `isFixed=1` 的那些，
/// policy 响应里并没有一个语义明确的必经点数组。这里的候选键是**推测**，
/// 因此把命中键名一并返回，让上层能把它打进日志核对。
fn extract_must_points(v: &serde_json::Value) -> (Vec<(f64, f64)>, Option<&'static str>) {
    let data = v.get("data").unwrap_or(v);
    for name in [
        "pointList",
        "runPointList",
        "mustPointList",
        "passPointList",
        "mustPoints",
        "passPoints",
        "runPoints",
        "points",
        "nodeList",
        "checkPointList",
    ] {
        let Some(arr) = data.get(name).and_then(|x| x.as_array()) else {
            continue;
        };
        // 轨迹/上传统一在 GCJ 系里做，所以必经点也必须取 GCJ（见 `must_point_gcj`）。
        let pts: Vec<(f64, f64)> = arr.iter().filter_map(must_point_gcj).collect();
        if !pts.is_empty() {
            return (pts, Some(name));
        }
    }
    (Vec::new(), None)
}

/// 只读：按任意 `runMode` 拉策略原文。
///
/// 用来核对「户外跑/自由跑/随机跑」等模式与计分跑是否共用同一套策略字段
/// （`+[SWRunConfigDataManager _fetchRunPolicyInMode:params:handler:]` 只在
/// `mode == 5` 时切到 `/api/v59/indoorRunModePolicy`，其余模式全走同一个
/// `/api/v70103/runModePolicy`）。
pub fn fetch_policy_raw(client: &mut ApiClient, mode: i64) -> Result<Value, String> {
    let unid = client
        .login
        .as_ref()
        .map(|s| s.unid.clone())
        .unwrap_or_default();
    let body = policy_body(mode, 0, 0, &unid).to_string();
    client.call("POST", POLICY_PATH, &body, &[])
}

/// body：{"runMode":1,"ruleUpdateTime":0,"geofenceUpdateTime":0,"selectUnid":"<unid>","operateType":0}
///
/// 注意键名是 **`geofenceUpdateTime`**（小写 f）——`geoFenceUpdateTime`（大写 F）属于
/// 围栏接口 `/api/v1/getGeoFenceForRun`，两个接口用的是不同的字面量。
/// `selectUnid` 在 IPA 里是 NSString，不能发数字。
pub fn fetch_policy(client: &mut ApiClient) -> Result<PolicyInfo, String> {
    fetch_policy_mode(client, 1)
}

/// 按 `runMode` 拉取策略。
///
/// `runMode` 的语义由 `+[SWRunPrepareImpl runPrepareImplWithRunPolicy:]` @0x10018a678
/// 的类表 `off_104CA3798` 给出：
///
/// | runMode | 实现类 | 跑法 |
/// |---|---|---|
/// | 0 | SWRunPrepareRandomImpl | 随机跑 |
/// | 1 | SWRunPrepareSequenceImpl | 顺序跑（校园跑/计分跑） |
/// | 2 | SWRunPrepareDestinationImpl | 定向跑 |
/// | 3 | SWRunPrepareMeetingImpl | 约跑 |
/// | 4 | SWRunPrepareFreeImpl | 自由跑 |
/// | 5 | SWRunPrepareFreeImpl | 室内跑（走室内策略端点） |
/// | 6 | SWRunPrepareOutdoorImpl | 户外跑 |
///
/// 记录体的 `sportType`/`policy` 由同一张索引表决定（见 `indoor::sport_type_for` /
/// `indoor::policy_for`）：runMode 1 → (1,1)，runMode 6 → (6,0)，runMode 4 → (4,0)。
pub fn fetch_policy_mode(client: &mut ApiClient, mode: i64) -> Result<PolicyInfo, String> {
    let unid = client
        .login
        .as_ref()
        .map(|s| s.unid.clone())
        .unwrap_or_default();
    let body = policy_body(mode, 0, 0, &unid).to_string();
    let biz = client.call("POST", POLICY_PATH, &body, &[])?;
    let timestamp = num_i64(get_field(&biz, "timestamp")).ok_or("policy 响应缺 timestamp")?;
    let policy = num_i64(get_field(&biz, "policy")).unwrap_or(0);
    let rule = get_field(&biz, "runRuleModel")
        .cloned()
        .unwrap_or(Value::Null);
    let (must_points, must_points_key) = extract_must_points(&biz);
    let (min_distance, min_run_time, face_verify, defaults_used) = parse_rule_numbers(&rule);
    Ok(PolicyInfo {
        timestamp,
        policy,
        min_distance,
        min_run_time,
        // faceVerify 若发 0/1 而不是布尔，`as_bool()` 会返回 None 并把启用核验的场地
        // 误判成不启用——那会让本工具在该场地上传一条必然无效的记录。
        face_verify,
        valid_start: valid_time_field(&rule, "start"),
        valid_end: valid_time_field(&rule, "end"),
        speed_bottom: num_f64(rule.get("speedBottom")),
        speed_top: num_f64(rule.get("speedTop")),
        step_bottom: num_f64(rule.get("stepBottom")),
        step_top: num_f64(rule.get("stepTop")),
        must_points,
        must_points_key,
        defaults_used,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_time_of_day_in_several_shapes() {
        assert_eq!(time_of_day_minutes("06:30"), Some(390));
        assert_eq!(time_of_day_minutes("22:00"), Some(1320));
        assert_eq!(time_of_day_minutes("06:30:00"), Some(390));
        assert_eq!(time_of_day_minutes("2026-11-06 06:30:00"), Some(390));
        assert_eq!(time_of_day_minutes("24:00"), None);
        assert_eq!(time_of_day_minutes("06:99"), None);
        assert_eq!(time_of_day_minutes(""), None);
        assert_eq!(time_of_day_minutes("abc"), None);
    }

    /// validTime 是 `{start,end}` 对象，不是整数 —— 旧的 as_i64() 只会拿到 0。
    #[test]
    fn valid_time_is_object_not_integer() {
        let rule = serde_json::json!({
            "validTime": { "start": "06:30", "end": "22:00" },
            "minDistance": 1000,
        });
        assert_eq!(valid_time_field(&rule, "start").as_deref(), Some("06:30"));
        assert_eq!(valid_time_field(&rule, "end").as_deref(), Some("22:00"));
        // 整数形态兼容成 HH:mm
        let rule2 = serde_json::json!({ "validTime": { "start": 390, "end": 1320 } });
        assert_eq!(valid_time_field(&rule2, "start").as_deref(), Some("06:30"));
        assert_eq!(valid_time_field(&rule2, "end").as_deref(), Some("22:00"));
        // 缺失 / 空串
        assert!(valid_time_field(&serde_json::json!({}), "start").is_none());
        assert!(valid_time_field(&serde_json::json!({ "validTime": {"start": ""} }), "start").is_none());
    }
}

#[cfg(test)]
mod parse_tests {
    use super::*;

    /// faceVerify 发 0/1 时必须被识别为布尔 —— 否则启用核验的场地会被误判成不启用。
    #[test]
    fn numeric_booleans_are_recognised() {
        assert_eq!(num_bool(Some(&serde_json::json!(1))), Some(true));
        assert_eq!(num_bool(Some(&serde_json::json!(0))), Some(false));
        assert_eq!(num_bool(Some(&serde_json::json!(true))), Some(true));
        assert_eq!(num_bool(Some(&serde_json::json!("1"))), Some(true));
        assert_eq!(num_bool(Some(&serde_json::json!("true"))), Some(true));
        assert_eq!(num_bool(None), None);
    }

    /// 服务端完整下发时不应报兜底；缺字段时要逐项列出。
    #[test]
    fn rule_defaults_are_reported() {
        let full = serde_json::json!({
            "minDistance": 1500, "minRunTime": 600, "faceVerify": 1,
            "validTime": { "start": "06:30", "end": "22:00" }
        });
        let (d, t, f, used) = parse_rule_numbers(&full);
        assert_eq!((d, t, f), (1500, 600, true));
        assert!(used.is_empty(), "完整下发时不应有兜底项: {used:?}");

        let empty = serde_json::json!({});
        let (d, t, f, used) = parse_rule_numbers(&empty);
        assert_eq!((d, t, f), (1000, 300, false));
        assert_eq!(used.len(), 4, "缺 4 项都要报: {used:?}");
        assert!(used.iter().any(|s| s.starts_with("minDistance")));
        assert!(used.iter().any(|s| s.starts_with("faceVerify")));
    }

    /// 五个键的类型与取值必须与真机一致：4 个 number + 1 个 string，operateType 恒 0。
    #[test]
    fn policy_body_matches_ipa() {
        let b = policy_body(1, 0, 0, "47203");
        assert_eq!(b.as_object().unwrap().len(), 5);
        assert_eq!(b["runMode"], 1);
        assert_eq!(b["ruleUpdateTime"], 0);
        assert_eq!(b["geofenceUpdateTime"], 0);
        assert_eq!(b["selectUnid"], "47203");
        assert!(b["selectUnid"].is_string(), "selectUnid 必须是字符串");
        assert_eq!(b["operateType"], 0);
        assert!(b["operateType"].is_i64(), "operateType 必须是数字");
    }

    /// 服务端实测下发的是**数组** `[{start,end}]`（"HH:mm:ss"），不是对象。
    /// 旧实现按对象读 → 恒 None → 时间窗从不生效。
    #[test]
    fn valid_time_is_parsed_from_array() {
        let rule = serde_json::json!({
            "validTime": [{"start": "06:30:00", "end": "22:00:00"}]
        });
        assert_eq!(valid_time_field(&rule, "start").as_deref(), Some("06:30:00"));
        assert_eq!(valid_time_field(&rule, "end").as_deref(), Some("22:00:00"));
        // 兼容旧的单对象形态
        let obj = serde_json::json!({"validTime": {"start": "07:00", "end": "21:00"}});
        assert_eq!(valid_time_field(&obj, "start").as_deref(), Some("07:00"));
        // 空数组/缺失 → None
        assert!(valid_time_field(&serde_json::json!({"validTime": []}), "start").is_none());
        assert!(valid_time_field(&serde_json::json!({}), "start").is_none());
        // 分钟数与 HH:mm:ss 都能解析成当日分钟
        assert_eq!(time_of_day_minutes("06:30:00"), Some(390));
        assert_eq!(time_of_day_minutes("22:00:00"), Some(1320));
    }

    #[test]
    fn numeric_fields_accept_strings_and_floats() {
        assert_eq!(num_i64(Some(&serde_json::json!(1000))), Some(1000));
        assert_eq!(num_i64(Some(&serde_json::json!(1000.0))), Some(1000));
        assert_eq!(num_i64(Some(&serde_json::json!("1000"))), Some(1000));
        assert_eq!(num_i64(None), None);
        assert_eq!(num_f64(Some(&serde_json::json!("6.5"))), Some(6.5));
    }
}
