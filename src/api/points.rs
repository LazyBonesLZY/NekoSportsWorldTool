//! 校园点位：POST /api/v560/get/1/distance/1（sportType=4）。
//!
//! runec = 信封(f"{uid}{经度6位}{纬度6位}{起始时间秒级ms整}"，insert→observed 序列化)；
//! sign = MD5(http版URL + 盐)。带 300s 缓存（限流 10603：5 分钟 3 次），失败回退最近缓存。

use super::client::ApiClient;
use super::model;
use crate::crypto::envelope::{build_envelope, OuterOrder};
use crate::crypto::sign::md5_url_sign;
use crate::location::Coordinate;
use serde_json::{json, Value};

pub const POINTS_PATH: &str = "/api/v560/get/1/distance/1";

/// 经纬度六位小数字符串。
fn six_digit(v: f64) -> String {
    format!("{v:.6}")
}

/// 拉取实时点位（带缓存回退）。anchor=(lat,lng) 请求锚点。
pub fn fetch_points(
    client: &mut ApiClient,
    anchor: Coordinate,
    log: &mut dyn FnMut(&str),
) -> Result<Vec<Value>, String> {
    fetch_points_ext(client, anchor, None, log)
}

/// run_area_id：学校配置了区域时 App 会附带；未配置则不传（与 App 一致）。
pub fn fetch_points_ext(
    client: &mut ApiClient,
    anchor: Coordinate,
    run_area_id: Option<String>,
    log: &mut dyn FnMut(&str),
) -> Result<Vec<Value>, String> {
    anchor.validate()?;
    // ① TTL 内命中缓存直接返回
    if let Some((ts, pts)) = model::load_points_cache_for(anchor) {
        let pts = valid_points(pts);
        if !pts.is_empty()
            && crate::crypto::envelope::now_ms() - ts < model::POINTS_TTL_MS
        {
            log(&format!("[points] 缓存命中（{} 秒前，{} 点）", (crate::crypto::envelope::now_ms() - ts) / 1000, pts.len()));
            return Ok(pts);
        }
    }
    // ② 请求接口
    let uid = client.login.as_ref().map(|s| s.uid).unwrap_or(0);
    let unid = client
        .login
        .as_ref()
        .map(|s| s.unid.clone())
        .unwrap_or_else(|| "0".into());
    let lat = anchor.latitude;
    let lon = anchor.longitude;
    let url = format!("{}{}", model::HOST, POINTS_PATH);

    let start_ms = crate::crypto::envelope::now_ms();
    // runec：uid + 6 位经度 + 6 位纬度 + 完整毫秒时间戳（7.4.0 与 7.3.40 同式，App 不做秒级取整）
    let runec_input = format!("{uid}{}{}{}", six_digit(lon), six_digit(lat), start_ms);
    // runec：共用会话 Four，外层 observed 序
    let runec_env = build_envelope(&mut client.session, &runec_input, OuterOrder::Observed);
    let runec = runec_env.json;

    let mut body = json!({
        "sportType": 4,
        "longitude": lon,
        "latitude": lat,
        "sign": md5_url_sign(&url),
        "uuid": uuid::Uuid::new_v4().to_string(),
        "selectedUnid": unid,
        "runec": runec,
    });
    if let Some(area) = run_area_id {
        body["runAreaId"] = json!(area);
    }
    let body = body.to_string();

    let out = client.envelope_request("POST", &url, &body, &client.ua(), &[])?;
    let fallback = |log: &mut dyn FnMut(&str)| -> Result<Vec<Value>, String> {
        if let Some((_ts, pts)) = model::load_points_cache_for(anchor) {
            let pts = valid_points(pts);
            if !pts.is_empty() {
                log("[points] 接口失败，回退最近一次接口结果缓存");
                return Ok(pts);
            }
        }
        Err("点位接口失败且无缓存".into())
    };
    let Some(dec) = out.decrypted else {
        return fallback(log);
    };
    let payload = &dec.business;
    // pointsResModels：优先顶层，其次 data 子对象
    let pts = valid_points(super::client::get_field(payload, "pointsResModels")
        .and_then(|m| m.as_array())
        .cloned()
        .unwrap_or_default());
    if !pts.is_empty() {
        let _ = model::save_points_cache(anchor, &pts);
        return Ok(pts);
    }
    let err = payload.get("error").and_then(|e| e.as_i64()).unwrap_or(0);
    log(&format!(
        "[points] 接口无点位 error={err}: {}",
        payload.get("message").and_then(|m| m.as_str()).unwrap_or("")
    ));
    fallback(log)
}

/// 同时支持接口数字和字符串坐标，并排除占位零及越界值。
pub(crate) fn point_coordinate(point: &Value, lat_key: &str, lon_key: &str) -> Option<(f64, f64)> {
    let number = |key: &str| {
        let v = point.get(key)?;
        v.as_f64().or_else(|| v.as_str()?.trim().parse::<f64>().ok())
    };
    let (lat, lon) = (number(lat_key)?, number(lon_key)?);
    if (lat == 0.0 && lon == 0.0) || Coordinate::new(lat, lon, 0.0).is_err() {
        return None;
    }
    Some((lat, lon))
}

// 接口和缓存中的点必须同时具备路线所需 BD 和五点所需 GCJ 坐标。
fn valid_points(points: Vec<Value>) -> Vec<Value> {
    points.into_iter().filter(|p| {
        point_coordinate(p, "lat", "lon").is_some()
            && point_coordinate(p, "glat", "glon").is_some()
    }).collect()
}

/// 点位中心（BD 系），仅使用完整有效的坐标对。
#[allow(dead_code)]
pub fn center_bd(points: &[Value]) -> (f64, f64) {
    let points = points_bd(points);
    let n = points.len().max(1) as f64;
    (points.iter().map(|p| p.0).sum::<f64>() / n,
     points.iter().map(|p| p.1).sum::<f64>() / n)
}

/// 点位 → (lat, lon) BD 系数组（轨迹输入）。
pub fn points_bd(points: &[Value]) -> Vec<(f64, f64)> {
    points.iter().filter_map(|p| point_coordinate(p, "lat", "lon")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_valid_coordinates_and_calculates_center() {
        let points = vec![json!({"lat": " 22.5 ", "lon": 112.5}),
                          json!({"lat": 24, "lon": "114"}),
                          json!({"lat": 20})];
        assert_eq!(points_bd(&points), vec![(22.5, 112.5), (24.0, 114.0)]);
        assert_eq!(center_bd(&points), (23.25, 113.25));
        assert_eq!(center_bd(&[]), (0.0, 0.0));
    }

    #[test]
    fn filters_invalid_and_empty_point_sets() {
        let valid = json!({"lat": "22.5", "lon": 112.5, "glat": 22.49, "glon": "112.49"});
        let bad = vec![json!({}), json!({"lat": 0, "lon": 0}),
            json!({"lat": "NaN", "lon": 112}), json!({"lat": 91, "lon": 112}),
            json!({"lat": 22, "lon": 181}), json!({"lat": 22, "lon": 112, "glat": 0, "glon": 0}),
            json!({"lat": 22, "lon": 112, "glat": "bad", "glon": 112})];
        assert!(valid_points(bad.clone()).is_empty());
        let mut mixed = bad;
        mixed.push(valid.clone());
        assert_eq!(valid_points(mixed), vec![valid]);
        assert!(point_coordinate(&json!({"lat": 0, "lon": 1}), "lat", "lon").is_some());
    }
}
