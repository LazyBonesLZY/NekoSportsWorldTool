//! HTTP 客户端封装。
//!
//! 通用请求封装：body = 信封(明文, insert)，headerSign = 信封(header, observed)，
//! 响应两层解密到业务 JSON；同一实例复用同一 keyDataFour。

use crate::api::model::Session;
use crate::crypto::decrypt::{decrypt_response, derive_paes_key, Decrypted};
use crate::crypto::envelope::{
    build_envelope, build_envelope_ts, rsa_public_key, EnvelopeSession, OuterOrder,
};
use crate::crypto::header::{build_header_for, HeaderIdentity};

pub fn make_agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout(std::time::Duration::from_secs(30))
        .build()
}

pub struct ApiClient {
    pub agent: ureq::Agent,
    pub session: EnvelopeSession,
    pub identity: HeaderIdentity,
    pub login: Option<Session>,
}

pub struct RequestOutcome {
    pub http_status: u16,
    pub decrypted: Option<Decrypted>,
    pub raw_len: usize,
    /// 解密失败时的服务端原始返回摘要（诊断用）。
    pub raw_head: String,
}

impl ApiClient {
    pub fn new(identity: HeaderIdentity, login: Option<Session>) -> Self {
        Self {
            agent: make_agent(),
            session: EnvelopeSession::new(),
            identity,
            login,
        }
    }

    fn uid(&self) -> i64 {
        self.login.as_ref().map(|s| s.uid).unwrap_or(-1)
    }

    fn token(&self) -> String {
        self.login.as_ref().map(|s| s.token.clone()).unwrap_or_default()
    }

    /// 按设备身份平台返回对应 UA（与 headerSign 平台严格一致，杜绝跨平台矛盾指纹）。
    pub fn ua(&self) -> String {
        if self.identity.platform == "android" {
            crate::crypto::header::UA_ANDROID.to_string()
        } else {
            crate::crypto::header::ua_ios_for(&self.identity.os_version)
        }
    }

    /// 发送信封请求（header/headerSign 用 observed 序，body 用 insert 序）。
    /// POST：body = 信封(body_plain)。GET（7.4.0 AFN 语义）：参数进 URL 查询串，
    /// body_plain 解析为对象后逐键附加到 URL，**不携带信封 body**（实机 GET 无请求体）。
    pub fn envelope_request(
        &mut self,
        method: &str,
        url: &str,
        body_plain: &str,
        ua: &str,
        extra_headers: &[(String, String)],
    ) -> Result<RequestOutcome, String> {
        let uid = self.uid();
        let token = self.token();
        let (header_plain, _) = build_header_for(&self.identity, uid, &token, None);
        let header_env = build_envelope(&mut self.session, &header_plain, OuterOrder::Observed);
        // body 时间戳 = header ts + 0..=2ms（原生两次独立取时，固定 +1 是可聚类特征）
        let delta = (rand::Rng::gen::<u32>(&mut rand::thread_rng()) % 3) as i64;
        let body_env =
            build_envelope_ts(&mut self.session, body_plain, OuterOrder::Insert, header_env.ts_ms + delta);

        let is_get = method.eq_ignore_ascii_case("GET");
        // GET：body_plain（JSON 对象）→ URL 查询参数（AFNetworking GET:parameters: 行为）
        let final_url = if is_get {
            append_query(url, body_plain)
        } else {
            url.to_string()
        };

        let mut req = self.agent.request(method, &final_url);
        // Alamofire/AFN 真实请求头集合：缺失 Accept/Accept-Encoding 是协议端指纹
        req = req
            .set("Accept", "*/*")
            .set("Accept-Language", "zh-Hans-CN;q=1.0")
            .set("Accept-Encoding", "gzip;q=1.0, compress;q=0.5")
            .set("User-Agent", ua)
            .set("appVersion", crate::crypto::header::IOS_APP_VERSION)
            .set("headerSign", &header_env.json);
        if !is_get {
            req = req.set("Content-Type", "application/json; charset=utf-8");
        }
        for (k, v) in extra_headers {
            req = req.set(k, v);
        }
        // GET 无请求体（信封 body 仅作 PAES 派生源，不出网）
        let resp = if is_get {
            req.call().map_err(ureq_err)?
        } else {
            req.send_string(&body_env.json).map_err(ureq_err)?
        };
        let status = resp.status();
        let raw = resp.into_string().unwrap_or_default();
        let raw_len = raw.len();
        let key = derive_paes_key(
            &body_env.key_data[0],
            &body_env.key_data[1],
            &body_env.key_data[2],
            &body_env.key_data[3],
        );
        let (decrypted, raw_head) = match decrypt_response(raw.as_bytes(), &key, &rsa_public_key()) {
            Ok(d) => (Some(d), String::new()),
            Err(e) => (None, format!("{e} | 原文: {}", crate::textlog::truncate(&raw, 160))),
        };
        Ok(RequestOutcome { http_status: status, decrypted, raw_len, raw_head })
    }

    /// 标准业务请求（RUN 域名）：返回业务 JSON；error != 10000 时 Err。
    pub fn call(
        &mut self,
        method: &str,
        path: &str,
        body_plain: &str,
        extra_headers: &[(String, String)],
    ) -> Result<serde_json::Value, String> {
        self.call_host(crate::api::model::HOST, method, path, body_plain, extra_headers)
    }

    /// 指定域名业务请求（排行榜/违规名单走 DISCOVERY，信封链相同）。
    /// 每次调用打印敏感字段打码后的解密响应日志。
    pub fn call_host(
        &mut self,
        host: &str,
        method: &str,
        path: &str,
        body_plain: &str,
        extra_headers: &[(String, String)],
    ) -> Result<serde_json::Value, String> {
        let url = format!("{}{}", host, path);
        let out = self.envelope_request(method, &url, body_plain, &self.ua(), extra_headers)?;
        let dec = out.decrypted.ok_or_else(|| {
            format!(
                "HTTP {} 响应解密失败（len={}）原文: {}",
                out.http_status, out.raw_len, out.raw_head
            )
        })?;
        let business = match check_business(&dec.business) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("[resp] {} -> Err: {}", path, e);
                return Err(e);
            }
        };
        let biz_str = serde_json::to_string(&crate::textlog::redact_json(&business)).unwrap_or_default();
        eprintln!("[resp] {} -> {} ({}B) {}", path, out.http_status, out.raw_len, biz_str);
        Ok(business)
    }
}

/// 业务结果检查：error == 10000 → Ok(business)；否则 Err(描述)。
pub fn check_business(v: &serde_json::Value) -> Result<serde_json::Value, String> {
    let err = v.get("error").and_then(|e| e.as_i64()).unwrap_or(0);
    if err == 10000 {
        Ok(v.clone())
    } else {
        let msg = v
            .get("message")
            .or_else(|| v.get("msg"))
            .and_then(|m| m.as_str())
            .unwrap_or("(无消息)");
        Err(format!("业务错误 {err}: {msg}"))
    }
}

/// 业务字段查找：先顶层，再 data 子对象。
pub fn get_field<'a>(biz: &'a serde_json::Value, name: &str) -> Option<&'a serde_json::Value> {
    if let Some(v) = biz.get(name) {
        if !v.is_null() {
            return Some(v);
        }
    }
    biz.get("data").and_then(|d| d.get(name))
}

/// data 字段如果是 JSON 字符串则解析（Obs 一层包裹语义）。
pub fn parse_data_field(biz: &serde_json::Value) -> serde_json::Value {
    match biz.get("data") {
        Some(serde_json::Value::String(s)) => serde_json::from_str(s).unwrap_or(serde_json::Value::Null),
        Some(v) => v.clone(),
        None => serde_json::Value::Null,
    }
}

/// GET 请求的 URL 查询参数拼接（AFNetworking GET:parameters: 语义）。
/// body_plain 为 JSON 对象时逐键附加；空对象/非对象则原样返回 URL。
fn append_query(url: &str, body_plain: &str) -> String {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(body_plain) else {
        return url.to_string();
    };
    let Some(obj) = v.as_object() else {
        return url.to_string();
    };
    if obj.is_empty() {
        return url.to_string();
    }
    let mut out = url.to_string();
    for (i, (k, val)) in obj.iter().enumerate() {
        out.push(if i == 0 && !url.contains('?') { '?' } else { '&' });
        out.push_str(&urlencoding_compat(k));
        out.push('=');
        out.push_str(&urlencoding_compat(&value_to_query_string(val)));
    }
    out
}

/// AFN 查询值字符串化：字符串原样，数字/布尔字面量，null → 空。
fn value_to_query_string(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Null => String::new(),
        serde_json::Value::Bool(b) => if *b { "1".into() } else { "0".into() },
        other => other.to_string(),
    }
}

/// 最小百分号编码（查询串安全字符以外的转义），对齐 AFN 的查询编码。
fn urlencoding_compat(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

pub fn ureq_err(e: ureq::Error) -> String {
    match e {
        ureq::Error::Status(code, resp) => {
            let body = resp.into_string().unwrap_or_default();
            format!("HTTP {code}: {}", &body[..body.len().min(300)])
        }
        ureq::Error::Transport(t) => format!("网络错误: {t}"),
    }
}

#[cfg(test)]
mod query_tests {
    use super::*;

    /// GET 查询串拼接（AFN GET:parameters: 语义）。
    #[test]
    fn test_append_query() {
        assert_eq!(append_query("https://h/api/v1", "{}"), "https://h/api/v1");
        assert_eq!(
            append_query("https://h/api/v1", r#"{"uid":123}"#),
            "https://h/api/v1?uid=123"
        );
        assert_eq!(
            append_query("https://h/api/v1?x=1", r#"{"uid":123,"s":"a b"}"#),
            "https://h/api/v1?x=1&uid=123&s=a%20b"
        );
        // 中文与空格转义
        assert_eq!(
            append_query("https://h", r#"{"q":"城市"}"#),
            "https://h?q=%E5%9F%8E%E5%B8%82"
        );
        // null → 空值；bool → 1/0
        assert_eq!(append_query("https://h", r#"{"a":null,"b":true,"c":false}"#), "https://h?a=&b=1&c=0");
        // 非 JSON 原样返回
        assert_eq!(append_query("https://h", "not json"), "https://h");
    }

    #[test]
    fn test_query_value_stringification() {
        assert_eq!(value_to_query_string(&serde_json::json!("x")), "x");
        assert_eq!(value_to_query_string(&serde_json::json!(42)), "42");
        assert_eq!(value_to_query_string(&serde_json::json!(null)), "");
        assert_eq!(value_to_query_string(&serde_json::json!(true)), "1");
    }
}
