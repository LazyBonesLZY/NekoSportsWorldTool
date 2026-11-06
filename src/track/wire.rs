//! OBS 对象组装。
//!
//! 11 键对象，每值 gzip+base64；BD→GCJ 单次转换；22 键协议点集；
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
///
/// **上传路径不再使用**：7.4.0 的 `_buildLocationPathInfo` 把实体里的坐标原样写入
/// `gLat/gLng`（`SWCoordinate coordinateWithLatitude:longitude:` 只 alloc+set），
/// 因此服务端下发什么就回传什么。本函数保留给坐标换算工具与测试使用。
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

pub(crate) fn gz_json(v: &Value) -> String {
    gz(v.to_string().as_bytes())
}

pub(crate) fn gz_str(v: &str) -> String {
    gz(v.as_bytes())
}

/// iOS 7.4.0 实机 22 键点集（_buildLocationPathInfo map 块）：数值字段为
/// "%.2f"/"%.1f"/"%.6f" 格式化字符串（实机 stringWithFormat），gainTime 为
/// "yyyy-MM-dd HH:mm:ss"；lat/lng 恒 0；locType 恒 0。
///
/// `flag` 是 `[SWRunGradeBuilder recordID]`（本机记录 id，全点相同），不是时间戳；
/// 由调用方传入 `record_id`（见 `HeaderIdentity::record_seq`）。
pub fn conv_point(p: &GenPoint, record_id: i64) -> Value {
    // 7.4.0 的上传路径**不做坐标换算**：`_buildLocationPathInfo` 的 map 块用
    // `SWCoordinate coordinateWithLatitude:[p lat] longitude:[p lng]` 取 `.latitude/.longitude`，
    // 而这两个方法只 alloc+set / 直接返回 ivar。也就是说服务端下发的坐标被原样回传，
    // `coorType` 只是固定标签。此前这里多做了 BD→GCJ，会把整条轨迹平移约 600m。
    let (glat, glng) = (p.gLat, p.gLng);
    json!({
        "id": p.id,
        "flag": record_id,
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
pub fn five_point_payload(points: &[Value], record_id: i64) -> Vec<Value> {
    // 只保留坐标有效的打卡点：早期版本对无坐标的点输出 glat/glon = null，
    // 而 validate_five_point_wrapper 会拒绝 null —— 生成器产出自己校验器拒绝的数据。
    // 这里在源头过滤；`position` 取过滤后的顺序号（真机样本里就是 0,1,2,3，跑区外的记 999）。
    points
        .iter()
        .filter_map(|p| {
            // 真实点位响应可能只有 BD 坐标，由 point_gcj 统一换取 GCJ。
            let (lat, lon) = crate::api::points::point_gcj(p)?;
            Some((lat, lon, p))
        })
        .enumerate()
        .map(|(i, (lat, lon, p))| {
            json!({
                "id": i as i64 + 1,
                "flag": record_id,
                "isPass": true,
                "lat": 0,
                "lon": 0,
                "glat": format!("{lat:.6}"),
                "glon": format!("{lon:.6}"),
                // 真机这两项都是 `numberWithInt:`，不是布尔。
                "position": i as i64,
                "isFixed": if p["isFixed"].as_bool().unwrap_or_else(|| p["isFixed"].as_f64().map(|v| v != 0.0).unwrap_or(false)) { 1 } else { 0 },
                "pointName": p["pointName"].as_str().unwrap_or(""),
            })
        })
        .collect()
}

/// 提交 body / OBS `fixed_point_json` 共用的五点包装串。
///
/// `_buildFivePointInfo` 的取值是：`record.pointData` 非空 → 回显它的 JSON，
/// 为空 → `SWJSON(@[]).rawValue` 即 `"[]"`。
///
/// **7.4.0 里 `pointData` 恒为空**：记录创建块 `sub_1000D2D14` 明确写入
/// `-[SWRunRecordEntity setPointData: &stru_104E4C978]`（空 NSString），
/// 且在 `FivePoint`/`MarkPoint` 相关函数中找不到任何后续写入方。
/// `defaultPropertyValues` 里的默认值也是 ""。因此 `fivePointJson` 恒为 `"[]"`。
///
/// **反方证据（已知但更旧）**：随包调试样本 `DebugRunTrack_fivePoints.geojson`
/// 里的 OBS 字典是 `{"fivePointJson":"[{十键实体}...]","useZip":false}` —— 那是
/// 2021 年的版本形态，当时 `pointData` 存的是实体数组。7.4.0 的创建块已改为空串。
///
/// 结论按 7.4.0 为准：发 `"[]"`。服务端要校验打卡点也有 `run_data.allLocJson`
/// 可独立重算，不依赖这一项。
pub fn five_point_wrapper(points: &[Value], record_id: i64) -> String {
    let _ = (points, record_id);
    json!({
        "useZip": false,
        "fivePointJson": "[]",
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
    // 真机发的是空数组（见 `five_point_wrapper` 的取证），因此空数组是合法值。
    // 只要非空，就仍然逐点校验坐标。
    for point in points {
        crate::api::points::point_gcj(&point).ok_or("五点轨迹包含缺失或无效坐标")?;
    }
    Ok(())
}

#[cfg(test)]
mod validation_tests {
    use super::*;
    /// 真机发的是空数组，因此 `[]` 必须放行；缺键与非法 JSON 仍要拒绝。
    #[test]
    fn accepts_empty_array_but_rejects_missing_or_malformed() {
        assert!(validate_five_point_wrapper("{}").is_err());
        assert!(validate_five_point_wrapper(r#"{"fivePointJson":"[]"}"#).is_ok());
        assert!(validate_five_point_wrapper(r#"{"fivePointJson":"not json"}"#).is_err());
    }

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
        // 真机 isFixed 是 numberWithInt:（0/1），不是布尔
        assert_eq!(payload.iter().map(|p| p["isFixed"].as_i64().unwrap()).collect::<Vec<_>>(),
                   vec![1, 0, 1, 0]);
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
            // 非空数组逐点校验；真机包装串恒为空数组，不再携带坐标。
            let direct = json!({"fivePointJson": json!([p.clone()]).to_string()}).to_string();
            assert!(validate_five_point_wrapper(&direct).is_err(), "{p}");
        }
        let good = json!({"glat": 0, "glon": 112});
        let direct_good = json!({"fivePointJson": json!([good]).to_string()}).to_string();
        assert!(validate_five_point_wrapper(&direct_good).is_ok());
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

/// 10s 时间窗（iOS 7.4.0 _buildStepAndSpeedInfo map 块，各 5 键；不含 id/flag）：
/// speed={beginTime,endTime,distance:"%.2f",queueNum:0,state:0}
/// step={beginTime,endTime,stepsNum,queueNum:0,state:0}
///
/// 时间边界唯一来源是 [`Track::window_bounds_ms`]：与 `Track::validate_consistency`
/// 的「窗口连续 + 末窗收在 startTime + totalTime*1000」用的是同一份计算，不会各算一套。
/// `stepsNum` 用四舍五入（原 `as i64` 是截断，会让 Σ stepsNum 系统性小于 totalSteps）。
fn build_windows(track: &Track, _rrid: i64) -> (Vec<Value>, Vec<Value>) {
    let bounds = track.window_bounds_ms();
    let mut sp = Vec::with_capacity(bounds.len());
    let mut stf = Vec::with_capacity(bounds.len());
    for (((begin, end), speed), steps) in bounds
        .iter()
        .zip(&track.speedPerTenSec)
        .zip(&track.stepsPerTenSec)
    {
        sp.push(json!({
            "beginTime": begin,
            "endTime": end,
            "distance": format!("{:.2}", speed.value),
            "queueNum": 0,
            "state": 0,
        }));
        stf.push(json!({
            "beginTime": begin,
            "endTime": end,
            "stepsNum": steps.value.round() as i64,
            "queueNum": 0,
            "state": 0,
        }));
    }
    (sp, stf)
}

/// 圈（每 1000m 一圈）。
///
/// 字段语义来自 `-[SWRunCoreRandom _checkAndSaveLapWithLocation:…]` @0x1000fc5ac 的
/// 调用点汇编（寄存器映射，可逐条核对）：
/// ```text
/// x3=lapIndex  d0=distance(m)  x4=duration(s)  x5=cumulativeDuration(s)
/// d1=avgPace(秒/公里)  d2=avgCadence(步/分)  d3=avgStride(米/步)
/// x6=steps  d4=endAltAbs  d5=startAlt  d6=elevationGain  w7=isFullLap(=1)
/// ```
/// 其中 `d1 = duration*1000/distance`（**秒/公里**，不是分/公里）、
/// `d3 = distance/steps`（**米/步**，不是厘米）、`d6` 是累计爬升的正增量。
pub fn build_laps(track: &Track, _start_ms: i64) -> Vec<Value> {
    let locs = &track.locations;
    if locs.len() < 2 {
        return Vec::new();
    }

    // 真机在**整公里边界上插值**建圈（`_checkAndSaveLapWithLocation:` @0x1000fc5ac）：
    //   `if (lastCompletedKm < (int)(distance / 1000))` → 每当累计距离跨过下一个 1000m 就建一圈；
    //   边界处的时长/步数/高度用当前采样区间按比例插值（`v34 = clamp((boundary - a4)/(v13 - a4))`），
    //   因此每圈 distance 恰好 1000m，末段不足一公里的部分**不建圈**，`isFullLap` 恒 true。
    let mut cum_dis = Vec::with_capacity(locs.len());
    let mut cum_gain = Vec::with_capacity(locs.len());
    let mut gain = 0.0f64;
    for (i, p) in locs.iter().enumerate() {
        if i > 0 {
            let dd = p.bdA - locs[i - 1].bdA;
            if dd > 0.0 {
                gain += dd;
            }
        }
        cum_dis.push(p.totalDis);
        cum_gain.push(gain);
    }

    // 在累计距离 `target` 处对 (时间, 步数, 高度, 累计爬升) 做线性插值。
    let sample_at = |target: f64| -> (f64, f64, f64, f64) {
        if target <= cum_dis[0] {
            let p = &locs[0];
            return (
                p.totalTime as f64,
                p.steps as f64,
                p.bdA,
                cum_gain[0],
            );
        }
        for i in 1..locs.len() {
            if cum_dis[i] >= target {
                let span = cum_dis[i] - cum_dis[i - 1];
                let u = if span > 1e-9 {
                    ((target - cum_dis[i - 1]) / span).clamp(0.0, 1.0)
                } else {
                    1.0
                };
                let a = &locs[i - 1];
                let b = &locs[i];
                let lerp = |x: f64, y: f64| x + (y - x) * u;
                return (
                    lerp(a.totalTime as f64, b.totalTime as f64),
                    lerp(a.steps as f64, b.steps as f64),
                    lerp(a.bdA, b.bdA),
                    lerp(cum_gain[i - 1], cum_gain[i]),
                );
            }
        }
        let p = locs.last().unwrap();
        (
            p.totalTime as f64,
            p.steps as f64,
            p.bdA,
            *cum_gain.last().unwrap(),
        )
    };

    let km_count = (cum_dis[cum_dis.len() - 1] / 1000.0).floor() as i64;
    let mut laps = Vec::with_capacity(km_count.max(0) as usize);
    let (mut prev_t, mut prev_steps, mut prev_alt, mut prev_gain) = (0.0f64, 0.0f64, locs[0].bdA, 0.0f64);
    for k in 1..=km_count {
        let boundary = k as f64 * 1000.0;
        let (t, steps, alt, g) = sample_at(boundary);
        // 真机先取整 `duration`（a6 是 int），`avgPace` 再由这个整数时长推出
        // （汇编：`UCVTF D1, W28` 之后 `FMUL/FDIV` 用的就是 W28）。
        let lap_t = (t - prev_t).max(1.0).round();
        let lap_steps = (steps - prev_steps).max(0.0);
        let lap_d = boundary - (k as f64 - 1.0) * 1000.0; // 恒为 1000
        laps.push(json!({
            "lapIndex": k,
            "distance": round_to(lap_d, 4),
            "duration": lap_t as i64,
            "cumulativeDuration": t.round() as i64,
            // 秒/公里：duration*1000/distance
            "avgPace": round_to(lap_t * 1000.0 / lap_d.max(0.001), 2),
            // 步/分：steps*60/duration
            "avgCadence": round_to(lap_steps * 60.0 / lap_t, 2),
            // 米/步：distance/steps
            "avgStride": round_to(lap_d / 1.0f64.max(lap_steps), 2),
            "endAltAbs": round_to(alt, 2),
            // 相对**本圈起点**的高度差（`setEndAltRel: endAltAbs - startAlt`）
            "endAltRel": round_to(alt - prev_alt, 2),
            // 累计爬升的正增量（`_lapElevationGain` 的差分）
            "elevationGain": round_to((g - prev_gain).max(0.0), 2),
            "isFullLap": true,
        }));
        prev_t = t;
        prev_steps = steps;
        prev_alt = alt;
        prev_gain = g;
    }
    laps
}

/// 组装 11 键 OBS 对象（值均 gzip+base64）。
///
/// 空数据时的取值（`buildObsDataWithRrid:` @0x100274e6 实测）：
/// `segment_json` 与 `runFaceCheck` 是**空数组的 JSON 串 `"[]"`** 再 gzip
/// （`_buildSegmentDataInfo` / `buildFaceCheckDataInfo` 都走 `sw_JSONString`）；
/// 只有 `extension_json` 是空串 gzip（`_buildExtensionJsonInfo` 无照片时返回 `@""`）。
pub fn build_obs_object(
    track: &Track,
    rrid: i64,
    uuid: &str,
    uid: i64,
    live_points: &[Value],
    record_id: i64,
) -> Value {
    let start_ms = track.startTime;
    let pts: Vec<Value> = track.locations.iter().map(|p| conv_point(p, record_id)).collect();
    let run_wrap = json!({ "allLocJson": Value::Array(pts).to_string(), "useZip": false });
    let (sp, stf) = build_windows(track, rrid);
    let laps = build_laps(track, start_ms);
    // 与提交 body 的 fivePointJson 必须完全同源（真机两处都调 `_buildFivePointInfo`），
    // 且 `flag` 与点位用的是同一个记录 id（真机两处都读 `[builder recordID]`）。
    let fx_text = five_point_wrapper(live_points, record_id);
    let fx: Value = serde_json::from_str(&fx_text).unwrap_or(Value::Null);
    json!({
        // 真机 `buildObsDataWithRrid:` 的 `rrid` 字段恒为 gzip("0") ——
        // 入参 rrid 根本没被使用（gzipBase64(CFSTR("0")) 写死）。
        "rrid": gz_str("0"),
        "uuid": gz_str(uuid),
        "uid": gz_str(&uid.to_string()),
        "run_data": gz_json(&run_wrap),
        "fixed_point_json": gz_json(&fx),
        "segment_json": gz_str("[]"),
        "speed_json": gz_json(&Value::Array(sp)),
        "step_freq_json": gz_json(&Value::Array(stf)),
        // `_buildLapsInfo` 在 `SWLapEntity` 查询结果为空时**返回 nil**，
        // 调用方随即替换成 `&stru_104E4C978`（空 NSString）再 gzip → 空串。
        // 也就是「没有圈」时发的是 gzip("")，不是 gzip("[]")。
        "laps_json": if laps.is_empty() {
            gz_str("")
        } else {
            gz_json(&Value::Array(laps))
        },
        "runFaceCheck": gz_str("[]"),
        // 7.4.0 新增：无照片时为空串；有照片时 {"hasRejectedPhoto":bool}
        "extension_json": gz_str(""),
    })
}

/// OBS objectKey（两个都要传）。
/// OBS 对象键。真机**只上传一个**（日期分支 + uuid 文件名）。
///
/// 依据：`sub_1004B026C` 的 key 生成器有两个分支 ——
/// `name` 能解析成整数走 `run_data/{n/1e6}/{n}.json`，否则走
/// `run_data/{yyyyMMddHH}/{name}.json`（本地时区，`.json` 后缀）。
/// 而实际上传路径 `uploadRunObsJsonwithParams:` 传的 `name` 是 **uuid**（非数字），
/// 因此只会产生日期分支那一个键；整数分支的唯一产生方
/// `uploadJsonWithRrid:withMoudle:withObs:withStartTime:result:` 在整个二进制里
/// 没有任何调用点（死代码）。下载侧也只按 uuid 取（`downloadObsDataCompletion:`）。
///
/// 早前实现额外上传了 `run_data/{rrid/1e6}/{rrid}.json`，属无依据的冗余写，
/// 多出的那次 PUT 本身就是一个可观测的差异。
pub fn obs_keys(track: &Track, _rrid: i64, uuid: &str) -> Vec<String> {
    let t0 = Local
        .timestamp_millis_opt(track.startTime)
        .single()
        .map(|t| t.format("%Y%m%d%H").to_string())
        .unwrap_or_default();
    vec![format!("run_data/{t0}/{uuid}.json")]
}

/// 键集合护栏：这些常量与 IPA 反编译结果一一对应。
/// 之前的 originalSign 事故就是「键集偏离却无人发现」——只断言数量会漏掉键名替换，
/// 因此这里对每个协议结构同时断言**数量与键名集合**。
/// OBS 顶层 11 键（IPA -[SWRunGradeBuilder buildObsDataWithRrid:]）。
pub const OBS_KEYS: [&str; 11] = [
        "rrid", "uuid", "uid", "run_data", "fixed_point_json", "step_freq_json",
        "speed_json", "segment_json", "runFaceCheck", "extension_json", "laps_json",
    ];
/// 点位 22 键（IPA _buildLocationPathInfo map 块 v52[0..21]）。
pub const POINT_KEYS: [&str; 22] = [
        "id", "flag", "speed", "avgSpeed", "radius", "gainTime", "type", "totalDis",
        "totalTime", "validDis", "validTime", "lat", "lng", "gLat", "gLng",
        "coorType", "bdA", "bdD", "bdS", "locType", "stepDistance", "queueNum",
    ];
/// 五点实体 10 键。
pub const FIVE_POINT_KEYS: [&str; 10] = [
        "id", "flag", "isPass", "lat", "lon", "glat", "glon", "position",
        "isFixed", "pointName",
    ];
/// 五点 wrapper 5 键。
pub const WRAPPER_KEYS: [&str; 5] = [
        "useZip", "fivePointJson", "runAreaId", "geoFencesJson", "freedomShowFence",
    ];
/// 速度窗 5 键。
pub const SPEED_WIN_KEYS: [&str; 5] = ["beginTime", "endTime", "distance", "queueNum", "state"];
/// 步数窗 5 键。
pub const STEP_WIN_KEYS: [&str; 5] = ["beginTime", "endTime", "stepsNum", "queueNum", "state"];
/// 圈 11 键（IPA _buildLapsInfo v52[0..10]）。
pub const LAP_KEYS: [&str; 11] = [
        "lapIndex", "distance", "duration", "cumulativeDuration", "avgPace",
        "avgCadence", "avgStride", "endAltAbs", "endAltRel", "elevationGain", "isFullLap",
    ];

#[cfg(test)]
mod key_set_guard_tests {
    use super::*;
    use crate::track::generator::build;









    fn keys_of(v: &Value) -> Vec<String> {
        let mut k: Vec<String> = v.as_object().unwrap().keys().cloned().collect();
        k.sort_unstable();
        k
    }
    fn expect(c: &[&str]) -> Vec<String> {
        let mut v: Vec<String> = c.iter().map(|s| s.to_string()).collect();
        v.sort_unstable();
        v
    }
    fn track() -> Track {
        let pts = [(22.411290, 112.940320), (22.411343, 112.941001),
                   (22.410866, 112.941100), (22.410582, 112.941216),
                   (22.410823, 112.940376)];
        build(1200.0, 540, 2026, (22.4169, 112.9472), 1_793_840_000_000, &pts)
    }
    fn live_points() -> Vec<Value> {
        (0..5).map(|i| serde_json::json!({
            "glat": 22.41 + i as f64 * 1e-4, "glon": 112.94 + i as f64 * 1e-4,
            "isFixed": i % 2 == 0, "pointName": format!("P{i}"),
        })).collect()
    }

    #[test]
    fn obs_top_level_key_set_is_exact() {
        let t = track();
        let obj = build_obs_object(&t, 1, "UUID", 1, &live_points(), 1);
        assert_eq!(keys_of(&obj), expect(&OBS_KEYS));
    }

    #[test]
    fn point_key_set_is_exact_22() {
        let t = track();
        let p = conv_point(&t.locations[0], 1);
        assert_eq!(keys_of(&p), expect(&POINT_KEYS));
        // 类型约束：数值字段为格式化字符串，lat/lng/locType 为数字 0
        assert!(p["gLat"].is_string() && p["speed"].is_string() && p["radius"].is_string());
        assert_eq!(p["lat"], 0);
        assert_eq!(p["lng"], 0);
        assert_eq!(p["locType"], 0);
    }

    #[test]
    fn five_point_and_wrapper_key_sets_are_exact() {
        let pts = live_points();
        let payload = five_point_payload(&pts, 123);
        assert_eq!(payload.len(), 5);
        for p in &payload {
            assert_eq!(keys_of(p), expect(&FIVE_POINT_KEYS));
        }
        let wrapper: Value = serde_json::from_str(&five_point_wrapper(&pts, 123)).unwrap();
        assert_eq!(keys_of(&wrapper), expect(&WRAPPER_KEYS));
    }

    /// 无坐标的点必须被过滤，而不是产出 null（否则与自身校验器冲突）。
    #[test]
    fn five_point_drops_points_without_coordinates() {
        let pts = vec![
            serde_json::json!({"glat": 22.41, "glon": 112.94, "pointName": "ok"}),
            serde_json::json!({"pointName": "no-coord"}),
            serde_json::json!({"glat": 0, "glon": 0, "pointName": "zero"}),
        ];
        let payload = five_point_payload(&pts, 1);
        assert_eq!(payload.len(), 1, "十键实体只保留坐标有效的点");
        assert_eq!(payload[0]["pointName"], "ok");
        assert_eq!(payload[0]["id"], 1);
        // 包装串回显的是**原始**点位数组（真机 pointData 语义），不做二次过滤；
        // 生产链路上游 `valid_points` 已经过滤过，这里只针对合法输入断言。
        let good = vec![json!({"lat": 22.41, "lon": 112.94, "pointName": "ok"})];
        assert!(validate_five_point_wrapper(&five_point_wrapper(&good, 1)).is_ok());
    }

    #[test]
    fn window_and_lap_key_sets_are_exact() {
        let t = track();
        let (sp, stf) = build_windows(&t, 1);
        assert!(!sp.is_empty() && !stf.is_empty());
        for w in &sp { assert_eq!(keys_of(w), expect(&SPEED_WIN_KEYS)); }
        for w in &stf { assert_eq!(keys_of(w), expect(&STEP_WIN_KEYS)); }

        let laps = build_laps(&t, t.startTime);
        assert!(!laps.is_empty());
        for l in &laps { assert_eq!(keys_of(l), expect(&LAP_KEYS)); }
    }
}

#[cfg(test)]
mod lap_units_tests {
    use super::*;
    use crate::track::generator::build;

    /// 圈字段单位对齐 `_checkAndSaveLapWithLocation:` 的寄存器映射：
    /// `avgPace = duration*1000/distance`（秒/公里）、`avgStride = distance/steps`（米/步）、
    /// `avgCadence = steps*60/duration`（步/分）。旧实现把前两者分别写成分/公里与厘米。
    #[test]
    fn lap_pace_is_sec_per_km_and_stride_is_metres() {
        let pts = [(22.411290, 112.940320), (22.411343, 112.941001),
                   (22.410866, 112.941100), (22.410582, 112.941216),
                   (22.410823, 112.940376)];
        let track = build(2000.0, 900, 2026, (22.4169, 112.9472), 1_793_840_000_000, &pts);
        let laps = build_laps(&track, track.startTime);
        assert!(!laps.is_empty());
        for lap in &laps {
            let d = lap["distance"].as_f64().unwrap();
            let t = lap["duration"].as_i64().unwrap() as f64;
            let pace = lap["avgPace"].as_f64().unwrap();
            let stride = lap["avgStride"].as_f64().unwrap();
            let cadence = lap["avgCadence"].as_f64().unwrap();
            // 秒/公里：6 分配速 ≈ 360，而不是 6
            assert!(
                (pace - t * 1000.0 / d).abs() < 0.02,
                "avgPace 应为秒/公里: {pace} vs {}",
                t * 1000.0 / d
            );
            assert!((150.0..=900.0).contains(&pace), "avgPace 量级不对: {pace}");
            // 米/步：真人 0.6~1.7m，而不是 60~170
            assert!((0.3..=2.5).contains(&stride), "avgStride 应为米/步: {stride}");
            // 步/分：真人 100~220
            assert!((80.0..=240.0).contains(&cadence), "avgCadence 量级不对: {cadence}");
            assert_eq!(lap["isFullLap"], true, "真机只建整公里圈，isFullLap 恒 true");
        }
        // 只按整公里建圈：2000m → 2 圈；没有末段半圈
        assert_eq!(laps.len(), (track.totalDistance / 1000.0).floor() as usize);
        // endAltRel 相对**本圈起点**：累计 duration 单调、distance 每圈恰好一千米量级
        for pair in laps.windows(2) {
            assert!(pair[1]["cumulativeDuration"].as_i64() > pair[0]["cumulativeDuration"].as_i64());
        }
        for lap in &laps {
            assert!((lap["distance"].as_f64().unwrap() - 1000.0).abs() < 40.0);
        }
    }
}

#[cfg(test)]
mod accuracy_tests {
    use crate::track::generator::build;

    /// 上传的 radius 必须是**滑动中位数**平滑后的整数精度
    /// （`-[SWValidPointDetector _smoothAccuracy:]` @0x100127300），
    /// 不是逐点独立随机——独立随机的序列在统计上明显更"白"。
    #[test]
    fn radius_is_median_smoothed_and_in_real_gps_range() {
        let pts = [(22.411290, 112.940320), (22.411343, 112.941001),
                   (22.410866, 112.941100), (22.410582, 112.941216),
                   (22.410823, 112.940376)];
        let track = build(2000.0, 720, 2026, (22.4169, 112.9472), 1_793_840_000_000, &pts);
        // 只检查正常点：起终点哨兵由 postfix 单独赋值
        let radii: Vec<f64> = track
            .locations
            .iter()
            .filter(|p| p.ptype == 0)
            .map(|p| p.radius)
            .collect();
        // 都是整数（真机对精度取整后再平滑）且在户外真实量级
        for r in &radii {
            assert_eq!(*r, r.round(), "radius 应为整数精度: {r}");
            assert!((1.0..=65.0).contains(r), "radius 超出真实 GPS 量级: {r}");
        }
        // 中位数平滑后相邻值不应频繁跳变
        let jumps = radii.windows(2).filter(|w| (w[0] - w[1]).abs() > 6.0).count();
        assert!(jumps * 10 <= radii.len(), "平滑后相邻跳变过多: {jumps}/{}", radii.len());
    }
}

#[cfg(test)]
mod window_shape_tests {
    use super::*;
    use crate::track::generator::build;

    /// 不足 1 km 的跑动没有整公里圈，`_buildLapsInfo` 此时返回 **nil**，
    /// 调用方替换成空串再 gzip —— 所以发出的是 `gzip("")` 而不是 `gzip("[]")`。
    #[test]
    fn no_lap_run_sends_empty_laps_json() {
        use std::io::Read;
        let pts = [(22.411290, 112.940320), (22.411343, 112.941001),
                   (22.410866, 112.941100), (22.410582, 112.941216),
                   (22.410823, 112.940376)];
        let track = build(900.0, 420, 2026, (22.4109, 112.9407), 1_793_840_000_000, &pts);
        assert!(track.totalDistance < 1000.0);
        let live = vec![serde_json::json!({"lat": 22.4113, "lon": 112.9403, "isFixed": 1})];
        let obj = build_obs_object(&track, 1, "U", 1, &live, 3);
        let raw = crate::crypto::envelope::b64_decode(obj["laps_json"].as_str().unwrap()).unwrap();
        let mut g = flate2::read::GzDecoder::new(&raw[..]);
        let mut out = String::new();
        g.read_to_string(&mut out).unwrap();
        assert_eq!(out, "", "无圈时应为 gzip(\"\")，不是 gzip(\"[]\")");
    }

    /// OBS 的 `rrid`/`uuid`/`uid` 都是 `gzipBase64(字符串)`，且 `rrid` 的值是
    /// **字面量 "0"** 而不是真实 rrid（`buildObsDataWithRrid:` 里
    /// `[@"0" dataUsingEncoding:]` → `gzipBase64WithUnData:`，参数 a3 完全没用）。
    #[test]
    fn obs_scalar_fields_are_gzip_of_expected_strings() {
        use std::io::Read;
        let pts = [(22.411290, 112.940320), (22.411343, 112.941001),
                   (22.410866, 112.941100), (22.410582, 112.941216),
                   (22.410823, 112.940376)];
        let track = build(1200.0, 540, 2026, (22.4109, 112.9407), 1_793_840_000_000, &pts);
        let live = vec![serde_json::json!({"lat": 22.4113, "lon": 112.9403, "isFixed": 1})];
        let obj = build_obs_object(&track, 1_234_567, "UUID-XYZ", 17837509, &live, 3);
        let un = |k: &str| -> String {
            let raw = crate::crypto::envelope::b64_decode(obj[k].as_str().unwrap()).unwrap();
            let mut g = flate2::read::GzDecoder::new(&raw[..]);
            let mut out = String::new();
            g.read_to_string(&mut out).unwrap();
            out
        };
        assert_eq!(un("rrid"), "0", "rrid 恒为字面量 0，与真实 rrid 无关");
        assert_eq!(un("uuid"), "UUID-XYZ");
        assert_eq!(un("uid"), "17837509");
        // 其余键都是 JSON 文本
        for k in ["run_data", "fixed_point_json", "laps_json", "speed_json", "step_freq_json"] {
            let t = un(k);
            assert!(t.starts_with('{') || t.starts_with('['), "{k} 应为 JSON: {t:.40}");
        }
    }

    /// `_buildStepAndSpeedInfo` 的两个数组元素是**固定 5 键**，且 `distance` 是
    /// `"%.2f"` 字符串、`beginTime`/`endTime` 是数字、`stepsNum` 是数字。
    ///
    /// 来源：`sub_100279110`（stepsPerTenSec map 块）与 `sub_1002792E0`（speedPerTenSec map 块），
    /// 两者都用 `dictionaryWithObjects:forKeys:count:5`。
    #[test]
    fn obs_window_elements_have_ipa_shape() {
        let pts = [(22.411290, 112.940320), (22.411343, 112.941001),
                   (22.410866, 112.941100), (22.410582, 112.941216),
                   (22.410823, 112.940376)];
        let track = build(1200.0, 540, 2026, (22.4109, 112.9407), 1_793_840_000_000, &pts);
        let (sp, stf) = build_windows(&track, 1);
        assert!(!sp.is_empty(), "应有 10 秒窗口");

        for w in &sp {
            let keys: Vec<&String> = w.as_object().unwrap().keys().collect();
            assert_eq!(keys.len(), 5, "speedPerTenSec 元素应为 5 键: {keys:?}");
            assert!(w["beginTime"].is_i64() || w["beginTime"].is_u64(), "beginTime 应为数字");
            assert!(w["endTime"].is_i64() || w["endTime"].is_u64(), "endTime 应为数字");
            assert!(w["distance"].is_string(), "distance 应为 \"%.2f\" 字符串");
            assert_eq!(w["queueNum"], 0);
            assert_eq!(w["state"], 0);
            let d = w["distance"].as_str().unwrap();
            assert_eq!(d.split('.').nth(1).map(|s| s.len()), Some(2), "两位小数: {d}");
        }
        for w in &stf {
            let keys: Vec<&String> = w.as_object().unwrap().keys().collect();
            assert_eq!(keys.len(), 5, "stepsPerTenSec 元素应为 5 键: {keys:?}");
            assert!(w["stepsNum"].is_number(), "stepsNum 应为数字");
        }
    }
}

#[cfg(test)]
mod lap_consistency_tests {
    use super::*;
    use crate::track::generator::build;

    fn pts() -> [(f64, f64); 5] {
        [(22.411290, 112.940320), (22.411343, 112.941001), (22.410866, 112.941100),
         (22.410582, 112.941216), (22.410823, 112.940376)]
    }

    /// 圈与点位必须互相说得通（跨层不变量）：
    /// - 圈数 = floor(totalDis / 1000)，每圈恰好 1000m
    /// - 1000m 的圈：avgPace(sec/km) 恒等于该圈 duration
    /// - stride × cadence / 60 ≈ 1000 / duration（步幅与步频推回的速度）
    /// - endAltAbs 必须等于该圈结束时点位的海拔（容差 0.5m）
    /// - endAltRel = endAltAbs − 本圈起点海拔
    #[test]
    fn laps_are_consistent_with_points() {
        let t = build(3300.0, 1200, 2026, (22.4109, 112.9407), 1_793_840_000_000, &pts());
        let laps = build_laps(&t, t.startTime);
        assert_eq!(laps.len(), (t.totalDistance / 1000.0).floor() as usize);
        let mut prev_cum = 0.0f64;
        let mut prev_alt: Option<f64> = None;
        for (i, l) in laps.iter().enumerate() {
            let d = l["distance"].as_f64().unwrap();
            assert_eq!(d, 1000.0, "lap{i} 必须恰好 1000m");
            let dur = l["duration"].as_f64().unwrap();
            assert!(dur > 0.0);
            assert_eq!(l["avgPace"].as_f64().unwrap(), dur, "lap{i} avgPace 应为 sec/km");
            let cad = l["avgCadence"].as_f64().unwrap();
            let stride = l["avgStride"].as_f64().unwrap();
            let v_ss = stride * cad / 60.0;
            let v_d = 1000.0 / dur;
            assert!(
                (v_ss - v_d).abs() / v_d < 0.01,
                "lap{i} 步幅×步频推回的速度 {v_ss:.3} 与距离/时长 {v_d:.3} 相差超过 1%"
            );
            let cum = l["cumulativeDuration"].as_f64().unwrap();
            assert!(cum > prev_cum, "lap{i} cumulativeDuration 必须递增");
            prev_cum = cum;
            // 该圈结束时刻的**插值**海拔：圈边界通常落在两个点位之间，
            // 直接取最近点会引入一个点位的噪声（现海拔模型 σ≈2.4m），必须按时间插值。
            let idx = t
                .locations
                .iter()
                .position(|p| p.totalTime as f64 >= cum)
                .unwrap_or(t.locations.len() - 1);
            let real = if idx == 0 {
                t.locations[0].bdA
            } else {
                let a = &t.locations[idx - 1];
                let b = &t.locations[idx];
                let span = (b.totalTime - a.totalTime).max(1) as f64;
                let f = ((cum - a.totalTime as f64) / span).clamp(0.0, 1.0);
                a.bdA + (b.bdA - a.bdA) * f
            };
            let end_abs = l["endAltAbs"].as_f64().unwrap();
            assert!(
                (end_abs - real).abs() < 0.5,
                "lap{i} endAltAbs={end_abs:.2} 与点位海拔 {real:.2} 不符"
            );
            if let Some(pa) = prev_alt {
                let rel = l["endAltRel"].as_f64().unwrap();
                assert!(
                    (rel - (end_abs - pa)).abs() < 0.05,
                    "lap{i} endAltRel={rel:.2} 应等于 endAltAbs−上圈海拔 {:.2}",
                    end_abs - pa
                );
            }
            prev_alt = Some(end_abs);
        }
    }
}

#[cfg(test)]
mod altitude_realism_tests {
    use crate::track::generator::build;

    /// 海拔序列必须有真实 GPS 量级的波动。
    ///
    /// 对照真实轨迹样本 `DebugRunTrack_locations.geojson`（518 点，1808m）：
    /// 标准差 2.37m、5%~95% 跨度 7.26m、全跨度 19.46m。
    /// 历史实现按 `uniform(3.8,6.2)/(0.4*n_est)` 衰减，长跑时 σ 只剩 0.18m、
    /// 跨度 1.05m —— 比真机平滑 13 倍，是可直接统计判别的特征。
    #[test]
    fn altitude_series_has_realistic_variance() {
        let pts = [(22.411290, 112.940320), (22.411343, 112.941001),
                   (22.410866, 112.941100), (22.410582, 112.941216),
                   (22.410823, 112.940376)];
        let t = build(3300.0, 1200, 2026, (22.4109, 112.9407), 1_793_840_000_000, &pts);
        let a: Vec<f64> = t.locations.iter().map(|p| p.bdA).collect();
        let n = a.len() as f64;
        let mean = a.iter().sum::<f64>() / n;
        let sd = (a.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / n).sqrt();
        let span = a.iter().cloned().fold(f64::MIN, f64::max)
            - a.iter().cloned().fold(f64::MAX, f64::min);
        assert!((1.2..=4.0).contains(&sd), "海拔标准差 {sd:.2}m 不在真机量级（≈2.37）");
        assert!(span >= 4.0, "海拔跨度 {span:.2}m 过小（真机 ≈19m，至少应有数米起伏）");
        // 同时不能太平滑也不能乱跳
        let mut d1: Vec<f64> = a.windows(2).map(|w| (w[1] - w[0]).abs()).collect();
        d1.sort_by(|x, y| x.partial_cmp(y).unwrap());
        let med = d1[d1.len() / 2];
        assert!((0.02..=1.5).contains(&med), "相邻海拔差中位 {med:.3}m 不合理");
    }
}

#[cfg(test)]
mod bearing_realism_tests {
    use crate::track::generator::build;

    /// `bdD`（CLLocation.course）的噪声形状必须与真机一致。
    ///
    /// 真实样本（518 点）相邻航向变化：中位 15.34°、均值 29.25°、最大 179.48°、
    /// 39% 小于 10° —— 是**厚尾**分布，不是干净的单高斯。
    #[test]
    fn bearing_noise_has_heavy_tail() {
        let pts = [(22.411290, 112.940320), (22.411343, 112.941001),
                   (22.410866, 112.941100), (22.410582, 112.941216),
                   (22.410823, 112.940376)];
        let mut deltas: Vec<f64> = Vec::new();
        for seed in [2026u64, 7, 99] {
            let t = build(1800.0, 1056, seed, (22.4109, 112.9407), 1_793_840_000_000, &pts);
            let b: Vec<f64> = t.locations.iter().map(|p| p.bdD).collect();
            deltas.extend(b.windows(2).map(|w| {
                ((w[1] - w[0] + 180.0).rem_euclid(360.0) - 180.0).abs()
            }));
        }
        assert!(deltas.iter().all(|d| *d <= 180.0), "航向差应在 [0,180]");
        deltas.sort_by(|x, y| x.partial_cmp(y).unwrap());
        let med = deltas[deltas.len() / 2];
        let mean = deltas.iter().sum::<f64>() / deltas.len() as f64;
        let small = deltas.iter().filter(|d| **d < 10.0).count() as f64 / deltas.len() as f64;
        let max = deltas[deltas.len() - 1];
        assert!((10.0..=22.0).contains(&med), "航向变化中位 {med:.2}° 偏离真机 15.34°");
        assert!(mean > 17.0, "航向变化均值 {mean:.2}° 太小，缺少厚尾");
        assert!(max > 100.0, "航向变化最大值 {max:.2}° 太小，缺少多径跳变");
        assert!((0.25..=0.50).contains(&small), "<10° 占比 {small:.2} 偏离真机 39%");
    }
}
