//! iOS 7.4.0 跑步提交签名。
//!
//! 语义来自 IPA `SWEncryption.signObjectWithoutEncoding:suffix:` 与
//! `NSDictionary sortedQueryStringWithoutEncoding`：
//! 键按 lowercaseString 比较排序、值取 `[obj description]`、
//! signature = MD5(query + 盐)，originalSign = 同一 query 明文。

use super::envelope::md5_hex;

pub const SALT: &str = "2slhe02lsfiwowlcixisla_sls-_slaor";

/// iOS 7.4.0 签名排除集：buildGrade 对 body 副本 removeObjectsForKeys
/// (@[allLocJson, fivePointJson]) 后签名。segmentJson/speedPerTenSec/
/// stepsPerTenSec/signature 在签名之后才插入，天然不在输入里。
pub const IOS_SIGN_EXCLUDED: [&str; 2] = ["allLocJson", "fivePointJson"];

/// iOS 7.4.0 值语义（_queryStringWithSortedKeys:percentEncode:）：
/// NSNull → ""、其余取 [obj description]（字符串原样，NSNumber Bool→"1"/"0"）。
fn ios_value(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::Null => String::new(),
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Bool(b) => if *b { "1".into() } else { "0".into() },
        other => other.to_string(),
    }
}

/// iOS 签名输入：body 全部键 − 排除集。
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

/// iOS 7.4.0 sortedQueryStringWithoutEncoding：键按 lowercaseString compare
/// （sortedArrayUsingComparator 块实证为先小写化再 compare）。
fn join_query_ios(m: &[(String, String)]) -> String {
    // 键按小写化后比较（IPA 的 sortedArrayUsingComparator 块）；先算一次小写键再排序，
    // 避免每个比较对都重新分配字符串。
    let mut items: Vec<(String, &(String, String))> = m
        .iter()
        .map(|kv| (kv.0.to_lowercase(), kv))
        .collect();
    items.sort_by(|a, b| a.0.cmp(&b.0));
    items
        .iter()
        .map(|(_, (k, v))| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("&")
}

/// iOS 7.4.0 signature：sortedQueryStringWithoutEncoding + MD5(串 + 盐)。
pub fn signature_ios(values: &serde_json::Value) -> String {
    let query = join_query_ios(&build_ios_sign_map(values));
    md5_hex(format!("{query}{}", SALT).as_bytes())
}

/// iOS 7.4.0 originalSign：同一 sortedQueryStringWithoutEncoding 明文。
pub fn original_sign_ios(values: &serde_json::Value) -> String {
    join_query_ios(&build_ios_sign_map(values))
}

/// 室内跑的签名：`+[SWEncryption signObject:suffix:]` —— 用**编码版** query。
///
/// 与室外不同：室外走 `signObjectWithoutEncoding:`（`percentEncode:NO`），
/// 室内 `buildGrade` @0x1002011f0 走的是 `signObject:suffix:`，
/// 即 `_queryStringWithSortedKeys:YES percentEncode:YES`。
/// `URLQueryAllowedCharacterSet` 覆盖字母数字与 `-._~!$&'()*+,;=:@/?`，
/// 因此数字/经纬度字符串不会被编码；只有中文、空格等才会变成 %XX（大写）。
pub fn indoor_signature(values: &serde_json::Value, salt: &str) -> String {
    let q = join_query_ios_encoded(&build_ios_sign_map(values));
    md5_hex(format!("{q}{salt}").as_bytes())
}

/// 与 [`join_query_ios`] 相同，但按 `URLQueryAllowedCharacterSet` 做百分号编码。
pub fn join_query_ios_encoded(m: &[(String, String)]) -> String {
    let mut items: Vec<(String, &(String, String))> =
        m.iter().map(|kv| (kv.0.to_lowercase(), kv)).collect();
    items.sort_by(|a, b| a.0.cmp(&b.0));
    items
        .iter()
        .map(|(_, (k, v))| format!("{}={}", percent_encode_url_query(k), percent_encode_url_query(v)))
        .collect::<Vec<_>>()
        .join("&")
}

/// Foundation `URLQueryAllowedCharacterSet` 的近似：字母数字 + `-._~!$&'()*+,;=:@/?`
/// 不编码，其余按 UTF-8 逐字节编码为大写 %XX。
fn percent_encode_url_query(s: &str) -> String {
    const EXTRA: &[u8] = b"-._~!$&'()*+,;=:@/?";
    let mut out = String::with_capacity(s.len());
    for b in s.as_bytes() {
        let keep = b.is_ascii_alphanumeric() || EXTRA.contains(b);
        if keep {
            out.push(*b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
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

    /// iOS 7.4.0 签名语义（IPA 实证 sortedArrayUsingComparator 块）：
    /// 键按 lowercaseString compare（忽略大小写）；Bool→"1"/"0"；Null→""。
    #[test]
    fn test_ios_signature_semantics() {
        let sample = json!({
            "sportType": 1, "totalTime": 1200, "totalDis": 2100, "speed": 43008,
            "startTime": 1700000000000i64, "stopTime": 1700000001200i64,
            "complete": "true", "selDistance": 2000, "unCompleteReason": 0,
            "getPrize": "false", "status": 0, "uuid": "TEST-UUID", "uid": 13056447,
            "sessionId": "436cb925-0840-8fd1-524f-7c43e44a8e73",
            "avgStepFreq": 165, "totalSteps": 3300, "selectedUnid": 57501,
            "calorie": 132, "policy": 0, "selRunTime": 1200, "validDis": 2050,
            "validTime": 1180, "useMobilityTools": 0, "faceCheck": 1,
        });
        let orig = original_sign_ios(&sample);
        // 键序：小写化后有序（selectedUnid→selectedunid 在 selRunTime→selruntime 后）
        let keys: Vec<&str> = orig.split('&').map(|kv| kv.split('=').next().unwrap()).collect();
        let lower_keys: Vec<String> = keys.iter().map(|k| k.to_lowercase()).collect();
        let sorted = {
            let mut v = lower_keys.clone();
            v.sort();
            v
        };
        assert_eq!(lower_keys, sorted, "iOS 签名键必须小写化有序");
        assert_eq!(
            keys.iter().position(|k| *k == "selectedUnid").unwrap() + 1,
            keys.iter().position(|k| *k == "selRunTime").unwrap(),
            "小写比较：selruntime(selectedunid) 中 'l' < 'u'，selectedUnid 在 selRunTime 前"
        );
        // 排除集只有 allLocJson/fivePointJson
        assert!(!orig.contains("fivePointJson"));
        // sessionId 参与签名
        assert!(orig.contains("sessionId=436cb925"));
        // signature = MD5(同串 + 盐)
        let sig = signature_ios(&sample);
        assert_eq!(sig, md5_hex(format!("{orig}{SALT}").as_bytes()));

        // 黄金向量：把上面那条"自洽"断言升级成字面量，防止实现与测试一起被改歪。
        // 排序依据：`+[NSDictionary _queryStringWithSortedKeys:percentEncode:]` @0x1002bd900
        // 用 `sortedArrayUsingComparator:`，其 comparator @0x1002bdba0 是
        // `[a2.lowercaseString compare:a3.lowercaseString]` —— **忽略大小写**。
        // 因此 `selectedUnid`（selectedunid）排在 `selRunTime`（selruntime）**之前**；
        // 若哪天改成按原始键做字节序，这两者会互换，下面的字面量会立刻失败。
        assert_eq!(
            orig,
            "avgStepFreq=165&calorie=132&complete=true&faceCheck=1&getPrize=false&policy=0\
             &selDistance=2000&selectedUnid=57501&selRunTime=1200\
             &sessionId=436cb925-0840-8fd1-524f-7c43e44a8e73&speed=43008&sportType=1\
             &startTime=1700000000000&status=0&stopTime=1700000001200&totalDis=2100\
             &totalSteps=3300&totalTime=1200&uid=13056447&unCompleteReason=0\
             &useMobilityTools=0&uuid=TEST-UUID&validDis=2050&validTime=1180"
        );
        assert_eq!(sig, "6f23fae3a0333f577e84dfd9202c2659");
    }

    /// iOS 签名语义：键按 lowercaseString 排序，signature = MD5(orig + 盐)。
    #[test]
    fn test_ios_signature_vectors() {
        let sample = json!({
            "sportType": 1, "totalTime": 540, "totalDis": 1200, "speed": 7500,
            "startTime": 1_793_851_519_462i64, "stopTime": 1_793_852_059_462i64,
            "complete": "true", "getPrize": "false", "status": 0,
            "uuid": "TEST-UUID", "uid": 17837509, "unauthorized": "0",
            "themeId": 0, "selectedUnid": 47203, "selRunTime": 300, "selDistance": 1000,
            "validDis": 1200, "validTime": 540, "unCompleteReason": 0,
            "useMobilityTools": 0, "calorie": 87, "address": "江门市", "goalId": 0,
            "totalAscent": 5, "avgPower": 166, "totalSteps": 1200,
            "avgStepFreq": 133, "faceCheck": 1, "policy": 1,
        });
        let orig = original_sign_ios(&sample);
        // 键序必须是小写化有序
        let keys: Vec<String> = orig.split('&')
            .map(|kv| kv.split('=').next().unwrap().to_lowercase()).collect();
        let mut sorted = keys.clone();
        sorted.sort();
        assert_eq!(keys, sorted, "iOS 签名键必须小写化有序");
        // 字符串型字段与数字型字段按各自 description 输出
        assert!(orig.contains("complete=true"));
        assert!(orig.contains("getPrize=false"));
        assert!(orig.contains("unauthorized=0"));
        assert!(orig.contains("speed=7500"));
        // signature = MD5(orig + 盐)
        assert_eq!(signature_ios(&sample), md5_hex(format!("{orig}{SALT}").as_bytes()));
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
