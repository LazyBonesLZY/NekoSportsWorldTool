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

/// 7.4.0 真实请求头常量。
///
/// 取值来自 `+[SWCHttpTool getHeadRequestSerializer]` @0x10027f674：
/// `Content-Type: application/json`、`Accept: application/json`、
/// `Accept-Encoding: gzip`、`Content-Encoding: gzip`、`appVersion`，超时 15s。
///
/// **`Content-Encoding: gzip` 有意不发**：AFHTTPRequestSerializer 并不会真的压缩请求体，
/// 真机是「明明没压却声明 gzip」。服务端显然容忍这一点（否则 App 自己就跑不通），
/// 但我们这边一旦被严格按 gzip 解析，整条链会直接不可用，且本轮无法登录验证。
/// 这是一个显式记录的偏离项，等有抓包/实测再决定是否对齐。
/// `Accept`：**App 侧不设置**。`+[SWCHttpTool setRequestSeriaHeader:]` @0x10027ed88 只显式设
/// `headerSign` 与 `appVersion`；`Accept` 这个 cstring 在全二进制里只被第三方 SDK
/// （0x102c2xxxx 区域）引用。
///
/// 注意：**ureq 会自动注入 `Accept: */*`，且没有移除 API**（已实测）。因此我们发出去的
/// 头里仍会带一个 `Accept: */*`——这一条是与真机的**已知客户端级差异**，
/// 值本身是中性的（不声明格式偏好），不像 `application/json` 那样属于主动表态。
pub const IOS_ACCEPT_LANGUAGE: &str = "zh-Hans-CN;q=1.0";
/// 真机是 NSURLSession 自动加的 `gzip, deflate, br`；我们只能解 gzip
/// （实测 ureq 收到 `Content-Encoding: deflate` 会返回乱码），
/// 因此只声明 `gzip`——宁可少声明，也不能声明解不了的编码。
pub const IOS_ACCEPT_ENCODING: &str = "gzip";
pub const IOS_CONTENT_TYPE: &str = "application/json";
/// AFHTTPRequestSerializer 的 timeoutInterval 是 15s。
pub const IOS_TIMEOUT_SECS: u64 = 15;

/// 给请求挂上 iOS 通用头（UA / appVersion / headerSign + AFN 默认头）。
/// `json_body` 为 false 时（GET）不设 Content-Type —— 实机 GET 不带该头。
pub fn apply_ios_headers(req: ureq::Request, ua: &str, header_sign: &str, json_body: bool) -> ureq::Request {
    let mut req = req
        .set("Accept-Language", IOS_ACCEPT_LANGUAGE)
        .set("Accept-Encoding", IOS_ACCEPT_ENCODING)
        .set("User-Agent", ua)
        .set("appVersion", crate::crypto::header::IOS_APP_VERSION)
        .set("headerSign", header_sign);
    if json_body {
        req = req.set("Content-Type", IOS_CONTENT_TYPE);
    }
    req
}

pub fn make_agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout(std::time::Duration::from_secs(IOS_TIMEOUT_SECS))
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

    /// iOS 7.4.0 UA（与 headerSign / appVersion 严格同源，杜绝跨平台矛盾指纹）。
    pub fn ua(&self) -> String {
        crate::crypto::header::ua_ios_for(&self.identity.os_version)
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
        let (header_plain, header_extra) = build_header_for(&self.identity, uid, &token, None);
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

        // 通用头由 apply_ios_headers 统一提供，提交链复用同一实现，避免两条链漂移。
        let mut req = apply_ios_headers(
            self.agent.request(method, &final_url),
            ua,
            &header_env.json,
            !is_get,
        );
        // 与 submit.rs 保持一致：iOS 明文头附带的 nonce/timeStamp/tokenSign 也随请求发送，
        // 否则两条链的请求头集合不同，服务端可据此区分。
        for (k, v) in &header_extra {
            req = req.set(k, v);
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

/// 服务器时间端点（run 域名，`+[SWAPI serverTime]` @0x100285ff0）。
pub const SERVER_TIME_PATH: &str = "/api/v3/getserver/gmttime";

/// 允许的本机/服务器时间偏差（秒）。
///
/// 真机 `-[SWCHttpTool isVaildTimeRange:]` @0x10028323c 在**每次提交前**先拉这个端点，
/// 若 `|server - local| > 300` 就返回 `status=0`，`uploadGrade` 的完成块随即用
/// 错误码 `1090090909` 终止上传（`sub_100047DA8`）。也就是说本机时钟偏 5 分钟以上，
/// 真机自己就不会上传。
pub const MAX_CLOCK_SKEW_SECS: i64 = 300;

/// 拉取服务器时间（秒）。响应形如 `{error, message, data:{data: <毫秒>}}`。
pub fn fetch_server_time_secs(client: &mut ApiClient) -> Result<i64, String> {
    let biz = client.call("GET", SERVER_TIME_PATH, "{}", &[])?;
    let data = parse_data_field(&biz);
    let raw = data
        .get("data")
        .and_then(|v| v.as_i64())
        .or_else(|| data.as_i64())
        .ok_or("serverTime 响应缺 data.data")?;
    // 真机：`(unsigned)(v+999) > 0x7CE` → `v/1000`；否则**不用这个值**，
    // 回退到 `+[ServerDate seconds]` 的缓存时间。我们没有那个缓存，
    // 因此小值时直接报错跳过对时，而不是拿一个明显不是时间戳的数字去比。
    if (raw + 999) as u64 > 0x7CE {
        Ok(raw / 1000)
    } else {
        Err(format!("serverTime 值异常（{raw}），跳过对时"))
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

/// 按 UTF-8 字符边界截断，避免在汉字/emoji 中间切片 panic。
pub fn safe_truncate(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

pub fn ureq_err(e: ureq::Error) -> String {
    match e {
        ureq::Error::Status(code, resp) => {
            let body = resp.into_string().unwrap_or_default();
            format!("HTTP {code}: {}", safe_truncate(&body, 300))
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

#[cfg(test)]
mod server_time_tests {
    use super::*;

    /// 毫秒值除以 1000；明显不是时间戳的值返回 Err（真机此时回退缓存，我们没有缓存）。
    #[test]
    fn server_time_ms_conversion_and_guard() {
        // 直接验证换算逻辑（不联网，故复制同样的分支）
        let conv = |raw: i64| -> Option<i64> {
            if (raw + 999) as u64 > 0x7CE { Some(raw / 1000) } else { None }
        };
        assert_eq!(conv(1_700_000_009_000), Some(1_700_000_009));
        assert_eq!(conv(500), None);
        assert_eq!(conv(0), None);
        assert_eq!(MAX_CLOCK_SKEW_SECS, 300);
        assert_eq!(SERVER_TIME_PATH, "/api/v3/getserver/gmttime");
    }
}
