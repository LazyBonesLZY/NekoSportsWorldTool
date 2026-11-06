//! 日志文本处理：着色前缀剥离与关键词归类，GUI/CLI 共用。

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum LogKind {
    Plain,
    Ok,
    Err,
    Warn,
}

/// 归类并剥离前缀：入站 √/×/⚠ 仅用于着色，不进入展示文本。
pub fn classify(t: &str) -> (LogKind, &str) {
    if let Some(rest) = t.strip_prefix('×') {
        return (LogKind::Err, rest.trim_start());
    }
    if let Some(rest) = t.strip_prefix('⚠') {
        return (LogKind::Warn, rest.trim_start());
    }
    if let Some(rest) = t.strip_prefix('√') {
        return (LogKind::Ok, rest.trim_start());
    }
    if t.contains("失败") || t.contains("错误") {
        return (LogKind::Err, t);
    }
    if t.contains("成功") {
        return (LogKind::Ok, t);
    }
    (LogKind::Plain, t)
}

/// 只取展示文本（CLI 输出用）。
pub fn clean(t: &str) -> &str {
    classify(t).1
}

/// 按字符截断（字节切片会劈开多字节字符导致 panic）。
pub fn truncate(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

/// 敏感值打码。
///
/// - 11 位纯数字（手机号）只保留前 3 后 2：保留前 4 后 4 时 11 位里只剩 3 位未知，
///   等于没打码；
/// - 其它值保留前后各 4 个字符；不超过 8 个字符时全部掩码。
pub fn mask(v: &str) -> String {
    let chars: Vec<char> = v.chars().collect();
    if chars.len() == 11 && chars.iter().all(|c| c.is_ascii_digit()) {
        let head: String = chars[..3].iter().collect();
        let tail: String = chars[9..].iter().collect();
        return format!("{head}****{tail}");
    }
    if chars.len() <= 8 {
        return "****".into();
    }
    let head: String = chars[..4].iter().collect();
    let tail: String = chars[chars.len() - 4..].iter().collect();
    format!("{head}****{tail}")
}

/// 需要打码的字段（不含大小写差异）。
const SENSITIVE_KEYS: &[&str] = &[
    "token",
    "password",
    "passwd",
    "pwd",
    "passToken",
    "access_token",
    "refresh_token",
    "authorization",
    "cookie",
    "set-cookie",
    "secret",
    // 账号即手机号；学校接口还会单独返回 telephone/phone/mobile
    "username",
    "telephone",
    "phone",
    "mobile",
    // 设备指纹：日志里出现完整 DeviceId/IDFA 等于把设备身份写进日志
    "idfa",
    "device_id",
    "deviceId",
    // GT4 滑块凭证
    "lotNumber",
    "lot_number",
    "captchaOutput",
];

fn is_sensitive_key(key: &str) -> bool {
    SENSITIVE_KEYS.iter().any(|k| key.eq_ignore_ascii_case(k))
}

/// 单个敏感值：字符串与数字都打码（数字 token 同样会泄露），其余结构继续递归。
fn mask_value(value: &serde_json::Value) -> serde_json::Value {
    use serde_json::Value;
    match value {
        Value::String(s) => Value::String(mask(s)),
        Value::Number(n) => Value::String(mask(&n.to_string())),
        other => redact_json(other),
    }
}

/// 递归掩码敏感键的值（字符串与数字），键名忽略大小写；其余业务数据原样保留。
pub fn redact_json(v: &serde_json::Value) -> serde_json::Value {
    use serde_json::Value;
    match v {
        Value::Object(obj) => Value::Object(
            obj.iter()
                .map(|(key, value)| {
                    let redacted = if is_sensitive_key(key) {
                        mask_value(value)
                    } else {
                        redact_json(value)
                    };
                    (key.clone(), redacted)
                })
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.iter().map(redact_json).collect()),
        _ => v.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_mask_long_and_multibyte() {
        assert_eq!(mask("abcdefghijkl"), "abcd****ijkl");
        assert_eq!(mask("123456789"), "1234****6789");
        assert_eq!(mask("中文字符测试多字节"), "中文字符****试多字节");
        assert_eq!(mask("🦀🐱😀🎉abcdef"), "🦀🐱😀🎉****cdef");
    }

    #[test]
    fn test_mask_short_and_empty() {
        for value in ["", "a", "1234567", "12345678", "中文字符测试"] {
            assert_eq!(mask(value), "****");
        }
    }

    /// 11 位手机号只留前 3 后 2，否则 11 位里仍有 8 位可见。
    #[test]
    fn test_mask_phone_number_keeps_three_and_two() {
        assert_eq!(mask("13812345678"), "138****78");
        // 非纯数字的 11 位串仍按通用规则
        assert_eq!(mask("abcdefghijk"), "abcd****hijk");
    }

    #[test]
    fn test_redact_json_sensitive_keys_case_insensitive() {
        for key in [
            "Token",
            "PASSWORD",
            "PassToken",
            "ACCESS_TOKEN",
            "refresh_token",
            "Authorization",
            "secret",
        ] {
            let value = json!({(key): "abcdefghijkl"});
            assert_eq!(redact_json(&value), json!({(key): "abcd****ijkl"}));
            assert_eq!(value[key], "abcdefghijkl");
        }
    }

    #[test]
    fn test_redact_json_nested_objects_and_arrays() {
        let value = json!({"data": {"token": "short", "items": [
            {"password": "abcdefghijkl"}, [{"secret": "12345678"}]],
            "authorization": {"token": "short"}}});
        let expected = json!({"data": {"token": "****", "items": [
            {"password": "abcd****ijkl"}, [{"secret": "****"}]],
            "authorization": {"token": "****"}}});
        assert_eq!(redact_json(&value), expected);
    }

    #[test]
    fn test_redact_json_preserves_other_values() {
        // 非敏感键原样保留；敏感键即使是非字符串也要打码
        let value = json!({"message": "成功", "uid": 123, "token": 42,
            "password": null, "secret": false, "items": ["plain", 7, null, true]});
        let expected = json!({"message": "成功", "uid": 123, "token": "****",
            "password": null, "secret": false, "items": ["plain", 7, null, true]});
        assert_eq!(redact_json(&value), expected);
        for value in [json!("plain"), json!(42), json!(null), json!(true)] {
            assert_eq!(redact_json(&value), value);
        }
    }

    /// 手机号/设备指纹/GT4 凭证也必须打码。
    #[test]
    fn test_redact_json_covers_account_and_device_fields() {
        let value = json!({
            "username": "13812345678",
            "telephone": "13812345678",
            "device_id": "0FA3EA16-46F0-4E44-9A97-1B63F0D9B11A",
            "idfa": "66147D92-F160-4A5E-937B-B8B36FCEE21F",
            "lotNumber": "8f3c2b1a0d9e8f7a6b5c4d3e2f1a0b9c",
            "nested": { "mobile": "13900001111" },
        });
        let out = redact_json(&value);
        assert_eq!(out["username"], "138****78");
        assert_eq!(out["telephone"], "138****78");
        assert!(out["device_id"].as_str().unwrap().contains("****"));
        assert!(out["idfa"].as_str().unwrap().contains("****"));
        assert!(out["lotNumber"].as_str().unwrap().contains("****"));
        assert_eq!(out["nested"]["mobile"], "139****11");
    }

    /// 多字节字符中间截断不允许 panic。
    #[test]
    fn test_truncate_multibyte() {
        let s = "中文字符串测试";
        for n in 0..=s.len() + 4 {
            let out = truncate(s, n);
            assert!(out.chars().count() <= n);
        }
        assert_eq!(truncate("中文abc", 4), "中文ab");
    }
}
