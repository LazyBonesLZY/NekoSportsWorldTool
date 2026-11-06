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

/// `runec` 的明文：`"%@%.6f%.6f%@"` = uid + **经度** + **纬度** + 毫秒时间戳（拼接，非 JSON）。
///
/// 汇编取证 `+[SWRunConfigDataManager runecWithLocation:]` @0x1000a3ae4：
/// ```text
/// BL coordinate        → FMOV D8, D1      ; D1 = longitude
/// BL coordinate        → FMOV D9, D0      ; D0 = latitude
/// STP  D8, D9, [SP, …]                    ; 变参顺序 (D8, D9) = (lng, lat)
/// ADRL X2, "%@%.6f%.6f%@"
/// ```
/// 第 4 个 `%@` 传的是 `NSNumber numberWithLongLong:`，描述即整数毫秒。
pub fn runec_plaintext(uid: i64, lon: f64, lat: f64, ts_ms: i64) -> String {
    format!("{uid}{}{}{}", six_digit(lon), six_digit(lat), ts_ms)
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
    let runec_input = runec_plaintext(uid, lon, lat, start_ms);
    // runec：共用会话 Four，外层 observed 序
    let runec_env = build_envelope(&mut client.session, &runec_input, OuterOrder::Observed);
    let runec = runec_env.json;

    let mut body = json!({
        "sportType": 4,
        "longitude": lon,
        "latitude": lat,
        "sign": md5_url_sign(&url),
        // 真机 `-[SWSpilePointSequenceStrategy UUID]` 返回 `NSUUID.UUIDString`，**大写**。
        "uuid": uuid::Uuid::new_v4().to_string().to_uppercase(),
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

/// 点位的高德（GCJ-02）坐标。
///
/// 真机样例响应（IPA 内 `FivePointResp.json`）里 `pointsResModels` 的元素只有
/// `lat`/`lon`/`isFixed`/`pointName`，**没有** `glat`/`glon`。旧实现要求两者都齐全，
/// 遇到这种响应会把所有点位过滤掉，直接导致「点位接口失败且无缓存」。
/// 这里改为：优先取 `glat/glon`，缺失时由 BD 坐标本地换算成 GCJ（与轨迹同一套转换）。
pub(crate) fn point_gcj(point: &Value) -> Option<(f64, f64)> {
    if let Some(v) = point_coordinate(point, "glat", "glon") {
        return Some(v);
    }
    // 7.4.0 不做坐标换算（见 `wire::conv_point` 的取证），所以 lat/lon 就是目标体系，
    // 原样使用；此前这里多做了一次 BD→GCJ。
    let (lat, lng) = point_coordinate(point, "lat", "lon")?;
    crate::location::Coordinate::new(lat, lng, 0.0).ok().map(|_| (lat, lng))
}

// 接口和缓存中的点必须能同时给出路线所需 BD 坐标与五点所需 GCJ 坐标。
fn valid_points(points: Vec<Value>) -> Vec<Value> {
    points
        .into_iter()
        .filter(|p| point_coordinate(p, "lat", "lon").is_some() && point_gcj(p).is_some())
        .collect()
}

/// 点位中心（BD 系），仅使用完整有效的坐标对。
#[allow(dead_code)]
pub fn center_bd(points: &[Value]) -> (f64, f64) {
    let points = points_bd(points);
    let n = points.len().max(1) as f64;
    (points.iter().map(|p| p.0).sum::<f64>() / n,
     points.iter().map(|p| p.1).sum::<f64>() / n)
}

/// 点位 → (lat, lon) BD 系数组。
///
/// **不再用于轨迹**：7.4.0 的上传体系是 GCJ（见 `points_gcj`）。保留它只给围栏/锚点
/// 诊断这类「按 BD 说话」的地方用。
pub fn points_bd(points: &[Value]) -> Vec<(f64, f64)> {
    points.iter().filter_map(|p| point_coordinate(p, "lat", "lon")).collect()
}

/// 点位 → (lat, lon) GCJ-02 数组（轨迹生成 + 上传体系）。
///
/// 依据（IDA，7.4.0）：
/// - `+[SWCoordinate currentType]` @0x1001cf794 恒返回 1，`stringForType:(1)` 返回 `"gcj02"`；
/// - `_buildLocationPathInfo` 的 map 块把 `SWMyLocationEntity` 的坐标经
///   `SWCoordinate coordinateWithLatitude:longitude:`（只 alloc+set）写进 `gLat/gLng`。
///
/// 也就是说 `gLat/gLng` 是 GCJ，服务端点位响应里的 `glat/glon` 也是 GCJ，两者必须同系。
/// 拿 BD 的 `lat/lon` 去生成轨迹，再原样当 GCJ 上传，整条轨迹会被整体平移约 900m
/// （实测：江门校区跑到了校外 936.4m 处）。
pub fn points_gcj(points: &[Value]) -> Vec<(f64, f64)> {
    points.iter().filter_map(point_gcj).collect()
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
        // 只有 BD 坐标、没有 GCJ 坐标的点也必须保留（真机样例响应就是这种形状）。
        let bd_only = json!({"lat": 22.5, "lon": 112.5, "isFixed": 0, "pointName": "4号点"});
        let bad = vec![json!({}), json!({"lat": 0, "lon": 0}),
            json!({"lat": "NaN", "lon": 112}), json!({"lat": 91, "lon": 112}),
            json!({"lat": 22, "lon": 181})];
        assert!(valid_points(bad.clone()).is_empty());
        let mut mixed = bad;
        mixed.push(valid.clone());
        mixed.push(bd_only.clone());
        assert_eq!(valid_points(mixed), vec![valid, bd_only]);
        assert!(point_coordinate(&json!({"lat": 0, "lon": 1}), "lat", "lon").is_some());
    }

    /// 真机点位响应只有 lat/lon 时，坐标**原样使用**，不做换算。
    ///
    /// 7.4.0 的上传层不做坐标转换（`SWCoordinate coordinateWithLatitude:longitude:`
    /// 只 alloc+set、`-latitude` 直接返回 ivar），所以响应里的经纬度就是最终上送值。
    #[test]
    fn point_gcj_is_verbatim_when_response_only_has_lat_lon() {
        let p = json!({"lat": 30.214898, "lon": 120.204429, "isFixed": 1, "pointName": "12号点"});
        assert!(point_coordinate(&p, "glat", "glon").is_none());
        let (glat, glon) = point_gcj(&p).expect("有 lat/lon 就必须可用");
        assert!((glat - 30.214898).abs() < 1e-12, "不得做任何换算: {glat}");
        assert!((glon - 120.204429).abs() < 1e-12, "不得做任何换算: {glon}");
        // 若响应同时带 glat/glon，则优先用它们
        let both = json!({"lat": 30.214898, "lon": 120.204429,
                          "glat": 30.21, "glon": 120.20});
        assert_eq!(point_gcj(&both), Some((30.21, 120.20)));
    }
}

#[cfg(test)]
mod runec_tests {
    use super::*;

    /// 顺序必须是 uid → 经度 → 纬度 → 毫秒；坐标各 6 位小数；不取整、无分隔符。
    #[test]
    fn runec_plaintext_matches_ipa_format() {
        let got = runec_plaintext(17837509, 112.9472, 22.4169, 1_793_840_000_000);
        assert_eq!(got, "17837509112.94720022.4169001793840000000");
        // 六位小数补零
        let got2 = runec_plaintext(1, 112.5, 22.5, 1);
        assert_eq!(got2, "1112.50000022.5000001");
        // 负坐标保留负号
        assert!(runec_plaintext(1, -1.5, -2.5, 1).starts_with("1-1.500000-2.500000"));
    }

}

#[cfg(test)]
mod coordinate_frame_tests {
    use super::*;
    use serde_json::json;

    /// 回归：轨迹必须用 GCJ（`glat/glon`）生成。
    ///
    /// 事故背景：11-06 的三条记录用 `points_bd`（BD 的 `lat/lon`）生成轨迹、
    /// 却原样当 `gLat/gLng` 上传，整条轨迹平移 936m 落到校外。
    /// 这里用实测点位（服务端返回 lat/lon=BD、glat/glon=GCJ）做双向断言。
    #[test]
    fn track_points_must_be_gcj_or_the_stadium_reference_never_matches() {
        // 实测：2026-11-06 runMode=1 点位响应（截取 5 个点）
        let pts = vec![
            json!({"pointName":"点位2","lat":22.417494,"lon":112.947451,"glat":22.411344,"glon":112.941001}),
            json!({"pointName":"点位3","lat":22.417015,"lon":112.947550,"glat":22.410867,"glon":112.941100}),
            json!({"pointName":"点位6","lat":22.416447,"lon":112.946970,"glat":22.410290,"glon":112.940522}),
            json!({"pointName":"点位4","lat":22.416729,"lon":112.947666,"glat":22.410582,"glon":112.941215}),
            json!({"pointName":"点位5","lat":22.416387,"lon":112.947398,"glat":22.410236,"glon":112.940948}),
        ];
        let gcj = points_gcj(&pts);
        assert_eq!(gcj.len(), 5);
        assert!((gcj[0].0 - 22.411344).abs() < 1e-12, "GCJ 必须是 glat，不得换算");
        // GCJ 点位能匹配到卫星描线（= 真的落在跑道上）
        assert!(
            crate::track::reference::matching_ring(&gcj).is_some(),
            "GCJ 点位必须匹配到江门校区跑道描线"
        );
        // BD 点位匹配不上（这正是当初轨迹跑到校外 900m 的原因）
        let bd = points_bd(&pts);
        assert!(
            crate::track::reference::matching_ring(&bd).is_none(),
            "BD 点位不应该匹配跑道描线——匹配上了说明坐标系又混了"
        );
    }
}
