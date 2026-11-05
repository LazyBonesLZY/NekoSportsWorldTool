//! Android 跑步提交签名。
//!
//! UploadSignEntity 31 字段固定顺序（+sportType=3 的 roomId）；
//! Java String.valueOf 字符串化；signature=MD5(忽略大小写排序拼接 + 盐)、
//! originalSign=自然序拼接（无 MD5 无盐）。

use super::envelope::md5_hex;

pub const SALT: &str = "2slhe02lsfiwowlcixisla_sls-_slaor";

/// UploadSignEntity 字段声明顺序（31 个）。
pub const UPLOAD_SIGN_FIELD_ORDER: [&str; 31] = [
    "sportType",
    "totalTime",
    "totalDis",
    "speed",
    "startTime",
    "stopTime",
    "complete",
    "selDistance",
    "unCompleteReason",
    "getPrize",
    "status",
    "uuid",
    "uid",
    "avgStepFreq",
    "totalSteps",
    "selectedUnid",
    "calorie",
    "policy",
    "selRunTime",
    "validDis",
    "validTime",
    "useMobilityTools",
    "errorCode",
    "geeToken",
    "unauthorized",
    "themeId",
    "faceCheck",
    "goalId",
    "address",
    "avgPower",
    "totalAscent",
];

/// UploadSignEntity.OooO00o 反射语义：String 原值，其余 String.valueOf。
fn android_value(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Null => "null".into(),
        serde_json::Value::Bool(b) => if *b { "true".into() } else { "false".into() },
        serde_json::Value::Number(n) => n.to_string(),
        other => other.to_string(),
    }
}

/// UploadSignEntity(entity) → OooO00o() 反射 Map + 可选 roomId。
fn build_sign_map(values: &serde_json::Value, has_room_id: bool) -> Vec<(String, String)> {
    let mut m = Vec::new();
    for key in UPLOAD_SIGN_FIELD_ORDER {
        if let Some(v) = values.get(key) {
            m.push((key.to_string(), android_value(v)));
        }
    }
    if has_room_id {
        if let Some(v) = values.get("roomId") {
            m.push(("roomId".to_string(), android_value(v)));
        }
    }
    m
}

/// OooOO0：移除 key 忽略大小写等于 "signature" 的项。
fn filter_signature(m: Vec<(String, String)>) -> Vec<(String, String)> {
    m.into_iter()
        .filter(|(k, _)| k.to_lowercase() != "signature")
        .collect()
}

/// OooO00o（自然序）/ OooO0O0（compareToIgnoreCase）拼接。
fn join_query(m: &[(String, String)], ignore_case: bool) -> String {
    let mut items: Vec<&(String, String)> = m.iter().collect();
    if ignore_case {
        items.sort_by(|a, b| a.0.to_lowercase().cmp(&b.0.to_lowercase()));
    } else {
        items.sort_by(|a, b| a.0.cmp(&b.0));
    }
    items
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("&")
}

/// OooO0Oo(map)：过滤后自然序拼接（无 MD5，无盐）。
pub fn original_sign(values: &serde_json::Value, has_room_id: bool) -> String {
    let m = filter_signature(build_sign_map(values, has_room_id));
    join_query(&m, false)
}

/// OooO0oO(map)：过滤 → 忽略大小写排序拼接 → MD5(串 + 盐)。
pub fn signature(values: &serde_json::Value, has_room_id: bool) -> String {
    let m = filter_signature(build_sign_map(values, has_room_id));
    let query = join_query(&m, true);
    md5_hex(format!("{query}{}", SALT).as_bytes())
}

/// iOS 7.4.0 签名排除集：removeObjectsForKeys(@[allLocJson, fivePointJson]) 后再签，
/// segmentJson/speedPerTenSec/stepsPerTenSec/signature/originalSign 在签名之后才插入。
pub const IOS_SIGN_EXCLUDED: [&str; 7] = [
    "allLocJson",
    "fivePointJson",
    "segmentJson",
    "speedPerTenSec",
    "stepsPerTenSec",
    "signature",
    "originalSign",
];

/// iOS 7.4.0 值语义（_queryStringWithSortedKeys:percentEncode:）：字符串原样、
/// NSNull → ""、NSNumber description（Bool→"1"/"0"，整数十进制）。
fn ios_value(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::Null => String::new(),
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Bool(b) => if *b { "1".into() } else { "0".into() },
        other => android_value(other),
    }
}

/// iOS 签名输入：body 全部键 − 排除集（非 Android 的 31 字段白名单）。
fn build_ios_sign_map(values: &serde_json::Value) -> Vec<(String, String)> {
    values
        .as_object()
        .map(|o| {
            o.iter()
                .filter(|(k, _)| !IOS_SIGN_EXCLUDED.contains(&k.as_str()))
                .map(|(k, v)| (k.clone(), ios_value(v)))
                .collect()
        })
        .unwrap_or_default()
}

/// iOS 7.4.0 signature：sortedQueryStringWithoutEncoding（大小写敏感排序）+ MD5(串 + 盐)。
pub fn signature_ios(values: &serde_json::Value) -> String {
    let query = join_query(&build_ios_sign_map(values), false);
    md5_hex(format!("{query}{}", SALT).as_bytes())
}

/// iOS 7.4.0 originalSign：同一 sortedQueryStringWithoutEncoding 明文（无 MD5 无盐）。
pub fn original_sign_ios(values: &serde_json::Value) -> String {
    join_query(&build_ios_sign_map(values), false)
}

/// 五点/点位请求的 sign：HTTP 版 URL + 固定 suffix 的 MD5（body 不参与）。
pub fn md5_url_sign(url: &str) -> String {
    let http_url = url.replacen("https://", "http://", 1);
    md5_hex(format!("{http_url}{}", SALT).as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// iOS 7.4.0 签名语义：大小写敏感排序（unCompleteReason 在 unauthorized 前，
    /// 因 'C'(0x43) < 'a'(0x61)）；Bool→"1"/"0"；Null→""。
    #[test]
    fn test_ios_signature_semantics() {
        let sample = json!({
            "sportType": 1, "totalTime": 1200, "totalDis": 2100, "speed": 43008,
            "startTime": 1700000000000i64, "stopTime": 1700000001200i64,
            "complete": true, "selDistance": 2000, "unCompleteReason": 0,
            "getPrize": false, "status": 0, "uuid": "TEST-UUID", "uid": 13056447,
            "avgStepFreq": 165, "totalSteps": 3300, "selectedUnid": 57501,
            "calorie": 132, "policy": 0, "selRunTime": 1200, "validDis": 2050,
            "validTime": 1180, "useMobilityTools": 0, "errorCode": 0,
            "geeToken": "", "unauthorized": 0, "themeId": 0, "faceCheck": 1,
            "goalId": null, "address": "大连市", "avgPower": 210, "totalAscent": 12,
        });
        // originalSign 明文：验证键序（大小写敏感）与值语义
        let orig = original_sign_ios(&sample);
        let keys: Vec<&str> = orig.split('&').map(|kv| kv.split('=').next().unwrap()).collect();
        let _expect_order = [
            "address", "avgPower", "avgStepFreq", "calorie", "complete", "errorCode",
            "faceCheck", "geeToken", "getPrize", "goalId", "selDistance", "selRunTime",
            "selectedUnid", "speed", "sportType", "startTime", "status", "stopTime",
            "themeId", "totalAscent", "totalDis", "totalSteps", "totalTime", "uid",
            "unCompleteReason", "unauthorized", "useMobilityTools", "uuid", "validDis",
            "validTime",
        ];
        // 抽取排序后的键序必须整体有序（大小写敏感语义）
        let mut sorted_keys = keys.clone();
        sorted_keys.sort();
        assert_eq!(keys, sorted_keys, "iOS 签名键必须大小写敏感有序");
        // iOS 值语义：bool→1/0，null→空
        assert!(orig.contains("complete=1"));
        assert!(orig.contains("getPrize=0"));
        assert!(orig.contains("goalId="));
        // signature = MD5(同串 + 盐)
        let sig = signature_ios(&sample);
        assert_eq!(sig.len(), 32);
        assert_eq!(sig, md5_hex(format!("{orig}{SALT}").as_bytes()));
        // 与 Android 形态（忽略大小写排序）在 unCompleteReason/unauthorized 对上必然不同
        let android_orig = original_sign(&sample, false);
        assert_ne!(
            orig.split('&').position(|kv| kv.starts_with("unCompleteReason")),
            android_orig.split('&').position(|kv| kv.starts_with("unauthorized")).map(|p| p.saturating_sub(0)),
            "两种形态键序语义不同"
        );
    }

    /// 实测向量（31 字段 + roomId 样例）。
    #[test]
    fn test_signature_vectors() {
        let sample = json!({
            "sportType": 3, "totalTime": 1000, "totalDis": 1200, "speed": 1200,
            "startTime": 1700000000000i64, "stopTime": 1700000001000i64,
            "complete": true, "selDistance": 1500, "unCompleteReason": 0,
            "getPrize": false, "status": 1, "uuid": "test-uuid", "uid": 13056447,
            "avgStepFreq": 134, "totalSteps": 1000, "selectedUnid": 57501,
            "calorie": 0, "policy": 0, "selRunTime": 0, "validDis": 1100,
            "validTime": 900, "useMobilityTools": 0, "errorCode": 0,
            "geeToken": "", "unauthorized": 0, "themeId": 0, "faceCheck": 1,
            "goalId": null, "address": "", "avgPower": 0, "totalAscent": 0,
            "roomId": 1001
        });
        assert_eq!(signature(&sample, true), "f2b958b2b9c8e99c4156076fbc72aabe");
        assert_eq!(signature(&sample, false), "6187185669bbd60d0c9ff4148f33e16d");
        let orig = original_sign(&sample, true);
        assert!(orig.starts_with("address=&avgPower=0&avgStepFreq=134&calorie=0&complete=true&errorCode=0&faceChec"));
        // goalId=null → "null"
        assert!(orig.contains("goalId=null"));
    }

    /// URL sign 向量。
    #[test]
    fn test_url_sign() {
        let url = "https://run.gxapp.iydsj.com/api/v560/get/1/distance/1";
        let expect = md5_hex(
            format!("http://run.gxapp.iydsj.com/api/v560/get/1/distance/1{}", SALT).as_bytes(),
        );
        assert_eq!(md5_url_sign(url), expect);
    }
}
