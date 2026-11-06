//! OBS 上传：POST /api/obs/temporary/url 换签名 URL → PUT JSON。

use super::client::{get_field, parse_data_field, ApiClient};
use serde_json::{json, Value};

pub const OBS_SIGN_PATH: &str = "/api/obs/temporary/url";

/// 跑步 JSON 数据的桶。`-[UploadClient init]` @0x1004b0124 用 Swift 小串立即数写入：
/// `0x62682D6A73647969` = "iydsj-hb"，`0xEF746F682D657361` = "ase-hot"（0xEF 为长度 15）。
/// 图片/视频走的是另外两个桶（gxapp-images / gxapp-vedio），与跑步数据不同桶。
pub const OBS_JSON_BUCKET: &str = "iydsj-hbase-hot";

/// 签名与上传都用 text/plain（真机从不发 application/json）。
pub const OBS_CONTENT_TYPE: &str = "text/plain";

/// 签名请求里的 `method`。
///
/// **必须是实际 HTTP 动词的大写形式**：OSS/对象存储的预签名把 HTTP 方法一并签进去，
/// 若这里声明 `"Put"` 而真正发出的是 `PUT`，签名与实际动词不一致，服务端会拒。
/// 二进制里 `"PUT"` 与 `"binary/octet-stream"` 相邻出现在同一字面量池
/// （`0x103ffd3c3` / `0x103ffd3c7`），正是对象存储相关的字面量簇。
/// 早前用 `"Put"` 属大小写未定，这里改为 `"PUT"`：服务端若做了大小写归一化，两者等价；
/// 若原样透传进签名，只有 `"PUT"` 能对上。
pub const OBS_METHOD: &str = "PUT";

/// OBS 临时授权结果。
pub struct SignedObs {
    pub url: String,
    /// 签名时约定的请求头（OBS 预签名 URL 的一部分，缺了或写错会 403）。
    pub headers: Vec<(String, String)>,
}

/// 换取签名 URL + 约定的请求头。
///
/// 响应形如 `{"error":10000,"data":"{\"signedUrl\":\"…\",\"actualSignedRequestHeaders\":{…}}"}` ——
/// `data` 是一段 **JSON 字符串**，需要二次解析（`sub_1004AFCBC` 里先取 `data.stringValue`
/// 再做 `SWJSON initWithString:`）。PUT 时必须把 `actualSignedRequestHeaders` **整份**
/// 逐个设成请求头（`+[SWCHttpTool putUploadData:withUrlStr:header:finish:]` @0x100283524），
/// 不能自己硬编码 `Content-Type`。
pub fn sign_url(client: &mut ApiClient, method: &str, key: &str) -> Result<SignedObs, String> {
    // contentType 真机是 **text/plain**（`sub_1004ACED4` 的小串立即数 "text/plain"）。
    let body = json!({
        "bucketName": OBS_JSON_BUCKET,
        "objectKey": key,
        "method": method,
        "contentType": OBS_CONTENT_TYPE,
    })
    .to_string();
    let biz = client.call("POST", OBS_SIGN_PATH, &body, &[])?;
    // business.data 是 JSON 字符串，里面才有 signedUrl 与 actualSignedRequestHeaders
    let data = parse_data_field(&biz);
    let url = data
        .get("signedUrl")
        .and_then(|v| v.as_str())
        .or_else(|| get_field(&biz, "signedUrl").and_then(|v| v.as_str()))
        .ok_or("OBS 签名响应缺 signedUrl")?
        .to_string();
    let mut headers: Vec<(String, String)> = Vec::new();
    if let Some(obj) = data.get("actualSignedRequestHeaders").and_then(|v| v.as_object()) {
        for (k, v) in obj {
            if let Some(s) = v.as_str() {
                headers.push((k.clone(), s.to_string()));
            }
        }
    }
    Ok(SignedObs { url, headers })
}

/// PUT 上传 OBS 对象。
///
/// 请求头**只**来自签名响应里的 `actualSignedRequestHeaders`；
/// 若签名响应没给（旧服务端），退回到签名时声明的 `text/plain`。
pub fn put_object(signed: &SignedObs, payload: &[u8], log: &mut dyn FnMut(&str)) -> Result<(), String> {
    let agent = super::client::make_agent();
    let mut req = agent.put(&signed.url);
    if signed.headers.is_empty() {
        req = req.set("Content-Type", OBS_CONTENT_TYPE);
    } else {
        for (k, v) in &signed.headers {
            req = req.set(k, v);
        }
    }
    let resp = req.send_bytes(payload).map_err(super::client::ureq_err)?;
    log(&format!("[obs] PUT {} -> {}", shorten(&signed.url), resp.status()));
    Ok(())
}

/// 依次上传给定的 objectKey，返回成功个数。
///
/// 真机只有**一个** OBS 键（`run_data/{yyyyMMddHH}/{uuid}.json`，
/// 见 `track::wire::obs_keys` 的取证），因此这里的 keys 长度通常为 1。
pub fn upload_keys(
    client: &mut ApiClient,
    keys: &[String],
    payload: &[u8],
    log: &mut dyn FnMut(&str),
) -> usize {
    // 真机在 OBS 失败时不是立刻重试，而是 `reSaveObsDataWithRRid:` 把数据以
    // `isUpload=0` 落本地表，留给后续补传。本工具没有常驻补传队列，
    // 因此改用**即时重试**近似：每个 key 最多 3 次、带抖动退避。
    // 记录本身已入库，OBS 只是明细；重试失败也不影响成绩。
    const ATTEMPTS: usize = 3;
    let mut ok = 0;
    for key in keys {
        let mut done = false;
        for attempt in 1..=ATTEMPTS {
            match sign_url(client, OBS_METHOD, key).and_then(|signed| put_object(&signed, payload, log)) {
                Ok(()) => {
                    if attempt > 1 {
                        log(&format!("[obs] PUT {key} 第 {attempt} 次尝试成功"));
                    }
                    ok += 1;
                    done = true;
                    break;
                }
                Err(e) => {
                    log(&format!("[obs] PUT {key} 第 {attempt}/{ATTEMPTS} 次失败: {e}"));
                    if attempt < ATTEMPTS {
                        let ms = 400u64 * attempt as u64 + rand::random::<u64>() % 400;
                        std::thread::sleep(std::time::Duration::from_millis(ms));
                    }
                }
            }
        }
        if !done {
            log(&format!(
                "[obs] PUT {key} 放弃（记录已入库，轨迹明细缺失；真机此时是落本地待补传）"
            ));
        }
    }
    ok
}

fn shorten(url: &str) -> &str {
    // 只显示 objectKey 部分，避免日志过长。
    // 必须按 UTF-8 字符边界截断：签名 URL 一般是 ASCII，但参数里出现中文时
    // 直接切片会 panic。
    let start = url.find("run_data").unwrap_or(0);
    let tail = &url[start..];
    super::client::safe_truncate(tail, 60)
}

/// 回读验证（GET 签名）。
#[allow(dead_code)]
pub fn fetch_object(client: &mut ApiClient, key: &str, log: &mut dyn FnMut(&str)) -> Result<Value, String> {
    let signed = sign_url(client, "Get", key)?;
    let agent = super::client::make_agent();
    let mut req = agent.get(&signed.url);
    for (k, v) in &signed.headers {
        req = req.set(k, v);
    }
    let resp = req.call().map_err(super::client::ureq_err)?;
    let text = resp.into_string().unwrap_or_default();
    log(&format!("[obs] GET 回读 {} 字节", text.len()));
    serde_json::from_str(&text).map_err(|e| format!("OBS 对象解析失败: {e}"))
}

