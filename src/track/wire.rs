//! OBS 对象组装。
//!
//! 10 键对象，每值 gzip+base64；BD→GCJ 单次转换；27 键协议点集；
//! 10s 窗（speed/step_freq）；每 1000m 一圈；五点 fixed_point_json。
#![allow(non_snake_case)]

use chrono::{Local, TimeZone};
use flate2::write::GzEncoder;
use flate2::Compression;
use serde_json::{json, Value};
use std::io::Write;

use super::geom::round_to;
use super::model::{GenPoint, Track};

const X_PI: f64 = std::f64::consts::PI * 3000.0 / 180.0;

/// 百度 BD-09 → 高德 GCJ-02。
pub fn bd09_to_gcj02(bd_lat: f64, bd_lng: f64) -> (f64, f64) {
    let x = bd_lng - 0.0065;
    let y = bd_lat - 0.006;
    let z = (x * x + y * y).sqrt() - 0.00002 * (y * X_PI).sin();
    let theta = y.atan2(x) - 0.000003 * (x * X_PI).cos();
    (z * theta.sin(), z * theta.cos())
}

/// gzip + base64。
pub fn gz(data: &[u8]) -> String {
    let mut enc = GzEncoder::new(Vec::new(), Compression::default());
    enc.write_all(data).expect("gzip 写入内存失败");
    let out = enc.finish().expect("gzip 收尾失败");
    super::super::crypto::envelope::b64_encode(&out)
}

fn gz_json(v: &Value) -> String {
    gz(v.to_string().as_bytes())
}

fn gz_str(v: &str) -> String {
    gz(v.as_bytes())
}

/// iOS 7.4.0 实机 22 键点集（_buildLocationPathInfo map 块）：数值字段为
/// "%.2f"/"%.1f"/"%.6f" 格式化字符串（实机 stringWithFormat），gainTime 为
/// "yyyy-MM-dd HH:mm:ss"；lat/lng 恒 0；locType 恒 0。
pub fn conv_point(p: &GenPoint, start_ms: i64) -> Value {
    let (glat, glng) = bd09_to_gcj02(p.gLat, p.gLng);
    json!({
        "id": p.id,
        "flag": start_ms,
        "speed": format!("{:.2}", p.speed),
        "avgSpeed": format!("{:.2}", p.avgSpeed),
        "radius": format!("{:.1}", p.radius),
        "gainTime": p.gainTime,
        "type": p.ptype,
        "totalDis": format!("{:.2}", p.totalDis),
        "totalTime": p.totalTime,
        "validDis": format!("{:.2}", p.validDis),
        "validTime": p.validTime,
        "lat": 0,
        "lng": 0,
        "gLat": format!("{:.6}", glat),
        "gLng": format!("{:.6}", glng),
        "coorType": "gcj02",
        "bdA": format!("{:.2}", p.bdA),
        "bdD": format!("{:.2}", p.bdD),
        "bdS": format!("{:.2}", p.bdS),
        "locType": 0,
        "stepDistance": format!("{:.2}", p.stepDistance),
        "queueNum": p.queueNum,
    })
}

/// 五点实体（iOS 7.4.0 _buildFivePointInfo map 块，10 键）：
/// lat/lon 恒 0；glat/glon 为 "%.6f" 字符串（GCJ 系）；isPass/isFixed 为 NSNumber bool。
pub fn five_point_payload(points: &[Value], start_ms: i64) -> Vec<Value> {
    points
        .iter()
        .enumerate()
        .map(|(i, p)| {
            let gcj = crate::api::points::point_coordinate(p, "glat", "glon");
            json!({
                "id": i as i64 + 1,
                "flag": start_ms,
                "isPass": true,
                "lat": 0,
                "lon": 0,
                "glat": gcj.map(|(lat, _)| format!("{lat:.6}")),
                "glon": gcj.map(|(_, lon)| format!("{lon:.6}")),
                "position": p["position"].as_i64().unwrap_or(999),
                "isFixed": p["isFixed"].as_bool().unwrap_or_else(|| p["isFixed"].as_f64().map(|v| v != 0.0).unwrap_or(false)),
                "pointName": p["pointName"].as_str().unwrap_or(""),
            })
        })
        .collect()
}

/// 提交 body 的 fivePointJson 包装串。
pub fn five_point_wrapper(points: &[Value], start_ms: i64) -> String {
    let five = five_point_payload(points, start_ms);
    json!({
        "useZip": false,
        "fivePointJson": Value::Array(five).to_string(),
        "runAreaId": -1,
        "geoFencesJson": "[]",
        "freedomShowFence": false,
    })
    .to_string()
}

/// 校验提交用五点轨迹仍是同一次点位请求产生的有效数据。
/// iOS 7.4.0 五点实体 lat/lon 恒 0（实机行为），真实坐标在 glat/glon（GCJ 系）。
pub fn validate_five_point_wrapper(wrapper: &str) -> Result<(), String> {
    let outer: Value = serde_json::from_str(wrapper).map_err(|e| format!("五点轨迹 JSON 无效: {e}"))?;
    let raw = outer.get("fivePointJson").and_then(Value::as_str).ok_or("五点轨迹缺少 fivePointJson")?;
    let points: Vec<Value> = serde_json::from_str(raw).map_err(|e| format!("五点轨迹数组无效: {e}"))?;
    if points.is_empty() { return Err("五点轨迹不能为空".into()); }
    for point in points {
        crate::api::points::point_coordinate(&point, "glat", "glon")
            .ok_or("五点轨迹包含缺失或无效坐标")?;
    }
    Ok(())
}

#[cfg(test)]
mod validation_tests {
    use super::*;
    #[test] fn rejects_empty_or_malformed_five_point_payload() { assert!(validate_five_point_wrapper("{}").is_err()); assert!(validate_five_point_wrapper(r#"{"fivePointJson":"[]"}"#).is_err()); }

    /// iOS 7.4.0 五点实体 lat/lon 恒 0，真实坐标在 glat/glon —— 校验必须放行。
    #[test]
    fn accepts_ios_five_point_with_zero_latlon() {
        let pts = r#"[{"id":1,"flag":1,"isPass":true,"lat":0,"lon":0,"glat":"22.411344","glon":"112.941001","position":999,"isFixed":0,"pointName":"p1"}]"#;
        let wrapper = json!({"useZip": false, "fivePointJson": pts, "runAreaId": -1, "geoFencesJson": "[]", "freedomShowFence": false}).to_string();
        assert!(validate_five_point_wrapper(&wrapper).is_ok());
        // glat/glon 双零仍拒绝
        let bad = r#"[{"id":1,"lat":0,"lon":0,"glat":"0","glon":"0"}]"#;
        let wrapper2 = json!({"useZip": false, "fivePointJson": bad, "runAreaId": -1}).to_string();
        assert!(validate_five_point_wrapper(&wrapper2).is_err());
    }

    #[test]
    fn five_point_payload_handles_string_number_and_fixed_flags() {
        let input = vec![
            json!({"glat": "22.411344", "glon": 112.941001, "isFixed": true}),
            json!({"glat": 22.5, "glon": "112.5", "isFixed": false}),
            json!({"glat": 22.5, "glon": 112.5, "isFixed": 1}),
            json!({"glat": 22.5, "glon": 112.5, "isFixed": 0}),
        ];
        let payload = five_point_payload(&input, 123);
        assert_eq!(payload[0]["glat"], "22.411344");
        assert_eq!(payload[0]["glon"], "112.941001");
        assert_eq!(payload.iter().map(|p| p["isFixed"].as_bool().unwrap()).collect::<Vec<_>>(),
                   vec![true, false, true, false]);
        assert!(validate_five_point_wrapper(&five_point_wrapper(&input, 123)).is_ok());
        let numeric = json!({"fivePointJson": json!([{"glat": 22.5, "glon": 112.5}]).to_string()}).to_string();
        assert!(validate_five_point_wrapper(&numeric).is_ok());
    }

    #[test]
    fn rejects_missing_malformed_nonfinite_out_of_range_and_zero_coordinates() {
        let invalid = vec![json!({}), json!({"glat": "bad", "glon": 112}),
            json!({"glat": "NaN", "glon": 112}), json!({"glat": "inf", "glon": 112}),
            json!({"glat": 91, "glon": 112}), json!({"glat": 22, "glon": -181}),
            json!({"glat": 0, "glon": 0}), json!({"glat": null, "glon": 112})];
        for p in invalid {
            let direct = json!({"fivePointJson": json!([p.clone()]).to_string()}).to_string();
            assert!(validate_five_point_wrapper(&direct).is_err(), "{p}");
            assert!(validate_five_point_wrapper(&five_point_wrapper(&[p.clone()], 123)).is_err(), "{p}");
        }
        let good = json!({"glat": 0, "glon": 112});
        assert!(validate_five_point_wrapper(&five_point_wrapper(&[good], 123)).is_ok());
    }

    #[test]
    fn laps_are_rebuilt_from_overridden_altitude() {
        let points = vec![(38.901678, 121.540241), (38.902564, 121.541233)];
        let mut track = crate::track::generator::build(
            1200.0,
            600,
            7,
            (38.9, 121.54),
            1_700_000_000_000,
            &points,
        );
        crate::track::altitude::override_bd_a(&mut track, 36.75).unwrap();
        let laps = build_laps(&track, track.startTime);
        assert!(!laps.is_empty());
        assert!(laps.iter().all(|lap| lap["elevationGain"] == 0.0));
        assert!(laps.iter().all(|lap| lap["endAltAbs"] == 36.75));
        assert!(laps.iter().all(|lap| lap["endAltRel"] == 0.0));
    }
}

/// 10s 时间窗，id=(rrid%100000)*1000+窗口序秒（6 个真人样本跨 9 月记录验证一致；
/// 旧版 App 样本为全局序号，不适用当前版本）。
/// 10s 时间窗（iOS 7.4.0 _buildStepAndSpeedInfo map 块，各 5 键）：
/// speed={beginTime,endTime,distance:"%.2f",queueNum:0,state:0}
/// step={beginTime,endTime,stepsNum,queueNum:0,state:0}
fn build_windows(track: &Track, _rrid: i64) -> (Vec<Value>, Vec<Value>) {
    let start_ms = track.startTime;
    let total_time = track.totalTime;
    let mut sp = Vec::new();
    let mut stf = Vec::new();
    let mut lo = 0i64;
    for (a, b) in track.speedPerTenSec.iter().zip(&track.stepsPerTenSec) {
        let hi = (lo + a.time).min(total_time);
        sp.push(json!({
            "beginTime": start_ms + lo * 1000,
            "endTime": start_ms + hi * 1000,
            "distance": format!("{:.2}", a.value),
            "queueNum": 0,
            "state": 0,
        }));
        stf.push(json!({
            "beginTime": start_ms + lo * 1000,
            "endTime": start_ms + hi * 1000,
            "stepsNum": b.value as i64,
            "queueNum": 0,
            "state": 0,
        }));
        lo = hi;
    }
    (sp, stf)
}

/// 圈（每 1000m 一圈，末圈 isFullLap=false；avgStride 单位厘米）。
fn build_laps(track: &Track, _start_ms: i64) -> Vec<Value> {
    let mut laps = Vec::new();
    let locs = &track.locations;
    let (mut prev_d, mut prev_t, mut prev_steps, mut gain) = (0.0f64, 0i64, 0i64, 0.0f64);
    let alt0 = locs.first().map(|p| p.bdA).unwrap_or(0.0);
    for (i, pt) in locs.iter().enumerate() {
        if i > 0 {
            let dd = pt.bdA - locs[i - 1].bdA;
            if dd > 0.0 {
                gain += dd;
            }
        }
        let d_now = pt.totalDis;
        let t_now = pt.totalTime;
        let last = i == locs.len() - 1;
        if d_now - prev_d >= 1000.0 || last {
            let lap_d = d_now - prev_d;
            let lap_t = 1i64.max(t_now - prev_t);
            let lap_steps = pt.steps - prev_steps;
            // iOS 7.4.0 _buildLapsInfo：11 键，无 flag/id/step；数值为 NSNumber（double/int）
            laps.push(json!({
                "lapIndex": laps.len() as i64 + 1,
                "distance": round_to(lap_d, 4),
                "duration": lap_t,
                "cumulativeDuration": t_now,
                "avgPace": round_to((lap_t as f64 / 60.0) / (lap_d / 1000.0).max(0.001), 2),
                "avgCadence": round_to(lap_steps as f64 / (lap_t as f64 / 60.0), 2),
                "avgStride": round_to(lap_d / 1.max(lap_steps) as f64 * 100.0, 2),
                "endAltAbs": round_to(pt.bdA, 2),
                "endAltRel": round_to(pt.bdA - alt0, 2),
                "elevationGain": round_to(gain, 2),
                "isFullLap": lap_d >= 1000.0,
            }));
            prev_d = d_now;
            prev_t = t_now;
            prev_steps = pt.steps;
            gain = 0.0;
        }
    }
    laps
}

/// 组装 10 键 OBS 对象（值均 gzip+base64；segment_json/runFaceCheck 为空串 gzip）。
pub fn build_obs_object(
    track: &Track,
    rrid: i64,
    uuid: &str,
    uid: i64,
    live_points: &[Value],
) -> Value {
    let start_ms = track.startTime;
    let pts: Vec<Value> = track.locations.iter().map(|p| conv_point(p, start_ms)).collect();
    let run_wrap = json!({ "allLocJson": Value::Array(pts).to_string(), "useZip": false });
    let (sp, stf) = build_windows(track, rrid);
    let laps = build_laps(track, start_ms);
    let five = five_point_payload(live_points, start_ms);
    let fx = json!({
        "fivePointJson": Value::Array(five).to_string(),
        "freedomShowFence": false,
        "geoFencesJson": "[]",
        "runAreaId": -1,
        "useZip": false,
    });
    json!({
        "rrid": gz_str(&rrid.to_string()),
        "uuid": gz_str(uuid),
        "uid": gz_str(&uid.to_string()),
        "run_data": gz_json(&run_wrap),
        "fixed_point_json": gz_json(&fx),
        "segment_json": gz_str(""),
        "speed_json": gz_json(&Value::Array(sp)),
        "step_freq_json": gz_json(&Value::Array(stf)),
        "laps_json": gz_json(&Value::Array(laps)),
        "runFaceCheck": gz_str(""),
        // 7.4.0 新增：无照片时为空串；有照片时 {"hasRejectedPhoto":bool}
        "extension_json": gz_str(""),
    })
}

/// OBS objectKey（两个都要传）。
pub fn obs_keys(track: &Track, rrid: i64, uuid: &str) -> Vec<String> {
    let t0 = Local
        .timestamp_millis_opt(track.startTime)
        .single()
        .map(|t| t.format("%Y%m%d%H").to_string())
        .unwrap_or_default();
    vec![
        format!("run_data/{t0}/{uuid}.json"),
        format!("run_data/{}/{}.json", rrid / 1000000, rrid),
    ]
}
