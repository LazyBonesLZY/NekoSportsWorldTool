//! 排行榜，全部走 {DISCOVERY}：
//! 主榜 /api/v41/rank；历史榜 /api/v41/historyRank；室内榜 /api/v43/runnings/indoor/studentRank。

use super::client::ApiClient;
use crate::api::model::DISCOVERY;
use serde_json::{json, Value};

pub const RANK_PATH: &str = "/api/v41/rank";
pub const HISTORY_PATH: &str = "/api/v41/historyRank";
pub const INDOOR_PATH: &str = "/api/v43/runnings/indoor/studentRank";

/// 榜单行（解析失败的行保留原始 JSON）。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RankRow {
    pub sort: i64,
    pub name: String,
    /// 米。
    pub length: f64,
    #[serde(default)]
    pub gender: i64,
}

/// 主榜参数：rtype=1个人 2班级 3院系；sort=1日榜 2月榜；
/// 主榜（`/api/v41/rank`）。
///
/// IPA 的构造块 `sub_1000F10D8` 写的是：`unid`（NSString）+ `type` + `sortType`，
/// `date` **仅当传入时间戳 ≥ 1 时**才以 `yyyy-MM-dd` 附加。**没有 `gender`**。
/// 早前实现多发了一个 `gender`，并在缺省时强行塞当天 `date`，两处都已按 IPA 改掉。
pub fn main_rank(
    client: &mut ApiClient,
    rtype: i64,
    sort_type: i64,
    date: Option<String>,
) -> Result<Vec<RankRow>, String> {
    let mut body = json!({
        "unid": unid_of(client),
        "type": rtype,
        "sortType": sort_type,
    });
    if let Some(d) = date {
        if !d.is_empty() {
            body["date"] = json!(d);
        }
    }
    let biz = client.call_host(DISCOVERY, "POST", RANK_PATH, &body.to_string(), &[])?;
    dump_unknown(&biz, "rank_main");
    Ok(rows_from(parse_data(&biz)))
}

/// 历史榜：sort=1日 2月。
pub fn history_rank(client: &mut ApiClient, sort_type: i64, gender: i64) -> Result<Vec<RankRow>, String> {
    let body = json!({
        "unid": unid_of(client),
        "sortType": sort_type,
        "gender": gender,
        "pageNo": 1,
        "pageSize": 20,
    });
    let biz = client.call_host(DISCOVERY, "POST", HISTORY_PATH, &body.to_string(), &[])?;
    dump_unknown(&biz, "rank_history");
    Ok(rows_from(parse_data(&biz)))
}

/// 室内榜：range=1日 2周 3月。
///
/// IPA 的 builder `sub_1003B4DC8` 只 setURL + setParameters，**没有** setHTTPMethod:，
/// 而 `SWRequest._HTTPMethod` 默认 0 → GET（参数进查询串、不做信封加密）。
pub fn indoor_rank(client: &mut ApiClient, date_range: i64, gender: i64) -> Result<Vec<RankRow>, String> {
    let body = json!({
        "pageNum": 1,
        "pageSize": 20,
        "gender": gender,
        "dateRange": date_range,
    });
    let biz = client.call_host(DISCOVERY, "GET", INDOOR_PATH, &body.to_string(), &[])?;
    dump_unknown(&biz, "rank_indoor");
    Ok(rows_from(parse_data(&biz)))
}

/// IPA 的 `getUNID` 返回 NSString，三个榜单 body 里的 unid 都必须是字符串。
fn unid_of(client: &ApiClient) -> String {
    client.login.as_ref().map(|s| s.unid.clone()).unwrap_or_default()
}

/// data 可能是数组或 {list:[...]} 包裹。
fn parse_data(biz: &Value) -> Vec<Value> {
    let data = super::client::parse_data_field(biz);
    if let Some(a) = data.as_array() {
        return a.clone();
    }
    for key in ["list", "records", "rankList", "ranks"] {
        if let Some(a) = data.get(key).and_then(|v| v.as_array()) {
            return a.clone();
        }
    }
    Vec::new()
}

/// 行字段按实际响应宽容解析；无 name/length 的行丢弃（原始 JSON 已落地）。
fn rows_from(arr: Vec<Value>) -> Vec<RankRow> {
    arr.iter()
        .enumerate()
        .filter_map(|(i, r)| {
            let name = r
                .get("name")
                .or_else(|| r.get("userName"))
                .or_else(|| r.get("nickname"))
                .and_then(|v| v.as_str())?;
            let length = r
                .get("length")
                .or_else(|| r.get("totalDis"))
                .or_else(|| r.get("dis"))
                .and_then(|v| v.as_f64())
                .unwrap_or(0.0);
            let sort = r
                .get("sort")
                .or_else(|| r.get("rank"))
                .and_then(|v| v.as_i64())
                .unwrap_or(i as i64 + 1);
            Some(RankRow {
                sort,
                name: name.to_string(),
                length,
                gender: r.get("gender").and_then(|v| v.as_i64()).unwrap_or(-1),
            })
        })
        .collect()
}

/// 首跑未知结构落地（平台数据目录）供字段核对。
fn dump_unknown(biz: &Value, name: &str) {
    let path = crate::platform::data_dir().join(name);
    if !path.exists() {
        let _ = std::fs::write(&path, biz.to_string());
    }
}
