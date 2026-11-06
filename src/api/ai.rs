//! AI 运动。
//! 列表：GET /api/v1/sport/ai/list {}；提交：POST /api/v65/sport/ai/record/upload；
//! 记录：GET /api/v66/sport/ai/record/infos?sportId=&pageSize=&pageNum=。

use super::client::{get_field, parse_data_field, ApiClient};
use serde_json::{json, Value};

pub const AI_LIST_PATH: &str = "/api/v1/sport/ai/list";
pub const AI_UPLOAD_PATH: &str = "/api/v65/sport/ai/record/upload";
#[allow(dead_code)]
pub const AI_RECORDS_PATH: &str = "/api/v66/sport/ai/record/infos";

/// AI 运动项目。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct AiSport {
    pub id: i64,
    pub name: String,
}

/// 拉取项目列表；成功即落盘永久缓存，失败时回退缓存。
pub fn fetch_list(client: &mut ApiClient) -> Result<Vec<AiSport>, String> {
    match fetch_list_remote(client) {
        Ok(list) => {
            let _ = crate::api::model::save_ai_sports(&list);
            Ok(list)
        }
        Err(e) => match crate::api::model::load_ai_sports() {
            Some(cached) if !cached.is_empty() => Ok(cached),
            _ => Err(e),
        },
    }
}

fn fetch_list_remote(client: &mut ApiClient) -> Result<Vec<AiSport>, String> {
    let biz = client.call("GET", AI_LIST_PATH, "{}", &[])?;
    let arr = get_field(&biz, "list")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    Ok(arr
        .iter()
        .filter_map(|it| {
            Some(AiSport {
                id: it.get("id")?.as_i64()?,
                name: it.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string(),
            })
        })
        .collect())
}

/// 单条 AI 记录全量信息（record/info：含 speed/consume/sportId/reason 等列表没有的字段）。
pub fn fetch_record_detail(client: &mut ApiClient, id: i64) -> Result<Value, String> {
    let path = "/api/v66/sport/ai/record/info";
    let biz = client.call("GET", path, &json!({ "id": id }).to_string(), &[])?;
    let data = parse_data_field(&biz);
    if data.is_null() {
        return Err(format!("记录 {id} 无详情数据"));
    }
    Ok(data)
}

/// 提交意图（具体字段按项目自身类型换算，见 upload）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum AiMode {
    /// 按分钟：1-30
    Minutes { minutes: i64 },
    /// 按次数：个数
    Count { reps: i64 },
}

/// 项目详情（GET ai/info）：type 决定成绩语义，number 为定次类目标个数。
#[derive(Debug, Clone, Copy)]
pub struct SportInfo {
    /// 1=计次类（score=个数）；2=计时类（score=用时毫秒）
    pub sport_type: i64,
    #[allow(dead_code)]
    pub number: i64,
}

/// 查询项目详情；失败按计次类处理。
pub fn fetch_info(client: &mut ApiClient, sport_id: i64) -> Result<SportInfo, String> {
    let path = "/api/v1/sport/ai/info";
    let biz = client.call("GET", path, &json!({ "sportId": sport_id }).to_string(), &[])?;
    let data = parse_data_field(&biz);
    Ok(SportInfo {
        sport_type: data.get("type").and_then(|v| v.as_i64()).unwrap_or(1),
        number: data.get("number").and_then(|v| v.as_i64()).unwrap_or(0),
    })
}

/// 提交 AI 运动记录。字段语义对照真人记录：
/// 计次类（type=1）score=个数、speed=0、消耗=个数×0.07；
/// 计时类（type=2）score=用时毫秒、speed=每分钟个数、消耗=秒×0.2；坐位体前屈消耗为 0。
pub fn upload(client: &mut ApiClient, sport_id: i64, mode: AiMode, at: Option<i64>) -> Result<Value, String> {
    let now = crate::crypto::envelope::now_ms();
    // `/api/v1/sport/ai/info` 在 7.4.0 二进制里**没有任何调用点**（selector 只有 selref +
    // method-list 两个引用）。仍保留调用是因为它还能给出 score 单位（计次/计时），
    // 但失败时必须显式告警——静默按计次类处理会把计时类项目写成错误单位。
    let info = match fetch_info(client, sport_id) {
        Ok(i) => i,
        Err(e) => {
            eprintln!(
                "[ai] 拉取项目 {sport_id} 类型失败（{e}），按计次类(type=1)提交；\
                 若该项目是计时类，本条成绩单位会错"
            );
            SportInfo { sport_type: 1, number: 0 }
        }
    };
    let jitter = 0.9 + rand::random::<f64>() * 0.2; // ±10%

    // 计次类持续频率（个/分）与计时类单次耗时（毫秒）——取自真人记录区间
    const REPS_PER_MIN: f64 = 90.0;
    const MS_PER_REP: f64 = 240.0;

    let (body_type, score, time_consume, speed, consume) = match (info.sport_type, mode) {
        // 计时类：score=用时；个数仅用于推算 speed/时长
        (2, AiMode::Minutes { minutes }) => {
            let ms = minutes * 60_000;
            let reps = (REPS_PER_MIN * minutes as f64 * jitter).round() as i64;
            (2, ms.to_string(), ms, (reps as f64 / (ms as f64 / 1000.0) * 60.0).round() as i64, (ms / 1000) as f64 * 0.2)
        }
        (2, AiMode::Count { reps }) => {
            let ms = ((reps as f64 * MS_PER_REP * jitter) as i64).max(30_000);
            (2, ms.to_string(), ms, (reps as f64 / (ms as f64 / 1000.0) * 60.0).round() as i64, (ms / 1000) as f64 * 0.2)
        }
        // 计次类：score=个数；speed 固定 0
        (_, AiMode::Minutes { minutes }) => {
            let reps = (REPS_PER_MIN * minutes as f64 * jitter).round() as i64;
            let ms = minutes * 60_000;
            (1, reps.to_string(), ms, 0, reps as f64 * 0.07)
        }
        (_, AiMode::Count { reps }) => {
            let ms = (((reps as f64 / REPS_PER_MIN) * 60_000.0) as i64).max(30_000);
            (1, reps.to_string(), ms, 0, reps as f64 * 0.07)
        }
    };
    let consume = if sport_id == 16 {
        "0".to_string() // 坐位体前屈：无消耗
    } else {
        format!("{consume:.1}")
    };
    let score_date = at.unwrap_or(now - time_consume);
    // 7.4.0 实机 **9 键** body（builder `sub_1006751EC` 的 swift_arrayDestroy(count: 9) 印证）：
    // sportId / type / score / timeConsume / speed / consume / 上传视频地址 / uuid / scoreDate。
    // `taskId` 在二进制里只被个推 SDK 使用，与本接口无关，已移除。
    let body = json!({
        "sportId": sport_id,
        "type": body_type,
        "score": score,
        "timeConsume": time_consume,
        "speed": speed.to_string(),
        "consume": consume,
        "上传视频地址": "",
        "uuid": uuid::Uuid::new_v4().to_string(),
        "scoreDate": score_date,
    });
    client.call("POST", AI_UPLOAD_PATH, &body.to_string(), &[])
}

/// AI 记录（v66 按天分组：list[]{scoreDate, frequency, recordInfos[]}）。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct AiRecordItem {
    pub id: i64,
    pub name: String,
    pub score: String,
    #[serde(rename = "type")]
    pub rtype: i64,
    pub upload_time: i64,
    pub score_date: i64,
    pub status: i64,
    pub has_video: bool,
    pub time_consume: i64,
    pub speed: String,
    pub consume: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct AiRecordGroup {
    pub score_date: i64,
    pub frequency: i64,
    pub records: Vec<AiRecordItem>,
}

/// 项目记录分页总数。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct AiRecordPage {
    pub groups: Vec<AiRecordGroup>,
    pub total_count: i64,
}

/// 拉取某项目的 AI 记录（pageSize 条，按天分组）。
pub fn fetch_records(client: &mut ApiClient, sport_id: i64, page_size: i64) -> Result<AiRecordPage, String> {
    let path = AI_RECORDS_PATH;
    let biz = client.call(
        "GET",
        path,
        &json!({ "sportId": sport_id, "pageSize": page_size, "pageNum": 1 }).to_string(),
        &[],
    )?;
    let data = parse_data_field(&biz);
    let arr = data
        .get("list")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let groups = arr
        .iter()
        .map(|g| AiRecordGroup {
            score_date: g.get("scoreDate").and_then(|v| v.as_i64()).unwrap_or(0),
            frequency: g.get("frequency").and_then(|v| v.as_i64()).unwrap_or(0),
            records: g
                .get("recordInfos")
                .and_then(|v| v.as_array())
                .map(|items| {
                    items
                        .iter()
                        .map(|r| AiRecordItem {
                            id: r.get("id").and_then(|v| v.as_i64()).unwrap_or(0),
                            name: r.get("name").and_then(|v| v.as_str()).unwrap_or("").into(),
                            score: match r.get("score") {
                                Some(Value::String(s)) => s.clone(),
                                Some(Value::Number(n)) => n.to_string(),
                                _ => String::new(),
                            },
                            rtype: r.get("type").and_then(|v| v.as_i64()).unwrap_or(0),
                            upload_time: r.get("uploadTime").and_then(|v| v.as_i64()).unwrap_or(0),
                            score_date: r.get("scoreDate").and_then(|v| v.as_i64()).unwrap_or(0),
                            status: r.get("status").and_then(|v| v.as_i64()).unwrap_or(0),
                            has_video: r
                                .get("mediaUrl")
                                .or_else(|| r.get("exerciseMediaUrl"))
                                .map(|v| !v.is_null() && v.as_str().map(|s| !s.is_empty()).unwrap_or(false))
                                .unwrap_or(false),
                            time_consume: r.get("timeConsume").and_then(|v| v.as_i64()).unwrap_or(0),
                            speed: match r.get("speed") {
                                Some(Value::String(s)) => s.clone(),
                                Some(Value::Number(n)) => n.to_string(),
                                _ => String::new(),
                            },
                            consume: match r.get("consume") {
                                Some(Value::String(s)) => s.clone(),
                                Some(Value::Number(n)) => n.to_string(),
                                _ => String::new(),
                            },
                        })
                        .collect()
                })
                .unwrap_or_default(),
        })
        .collect();
    let total_count = data
        .get("totalCount")
        .and_then(|v| v.as_str().and_then(|s| s.parse().ok()).or_else(|| v.as_i64()))
        .unwrap_or(0);
    Ok(AiRecordPage { groups, total_count })
}
