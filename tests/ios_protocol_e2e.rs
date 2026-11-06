//! 离线 iOS 协议端到端校验：不联网、不登录、不调用任何接口。
//!
//! 覆盖提交链路的全部产物：轨迹 → OBS 对象 → 五点 → iOS body → 签名。
//! 每一层都对着 IPA 反编译得到的键集/语义做断言，任何一层偏离立即失败。

use nekosportsworldtool::api::submit::{build_ios_body, total_ascent, IosBodyInput, IOS_BODY_KEYS};
use nekosportsworldtool::crypto::sign::{original_sign_ios, signature_ios, IOS_SIGN_EXCLUDED};
use nekosportsworldtool::track::calorie::{avg_power, display_kcal, submitted_calorie};
use nekosportsworldtool::track::generator::build;
use nekosportsworldtool::track::geom::round_to;
use nekosportsworldtool::track::model::Track;
use nekosportsworldtool::track::wire::{
    build_obs_object, five_point_payload, five_point_wrapper, obs_keys, validate_five_point_wrapper,
};
use serde_json::Value;
use std::collections::BTreeSet;

/// 江门校区实时打卡点（BD-09）。
const CHECKPOINTS: [(f64, f64); 5] = [
    (22.411290, 112.940320),
    (22.411343, 112.941001),
    (22.410866, 112.941100),
    (22.410582, 112.941216),
    (22.410823, 112.940376),
];

const UID: i64 = 17_837_509;
const UNID: i64 = 47_203;
const START_MS: i64 = 1_793_851_519_462;
/// 体重（与 `api::flow` 的默认值同一来源：会话未带体重时取 68）。
const WEIGHT_KG: f64 = 68.0;

/// 某一时刻（相对开始的毫秒数）按 10 秒窗累计的距离。
fn window_cumulative_m(track: &Track, elapsed_ms: i64) -> f64 {
    let mut acc = 0.0;
    for ((begin, end), window) in track.window_bounds_ms().iter().zip(&track.speedPerTenSec) {
        let (b, e) = (begin - track.startTime, end - track.startTime);
        if elapsed_ms >= e {
            acc += window.value;
        } else {
            if elapsed_ms > b && e > b {
                acc += window.value * ((elapsed_ms - b) as f64 / (e - b) as f64);
            }
            break;
        }
    }
    acc
}

/// 同一时刻按点位 `totalDis` 线性插值得到的累计距离。
fn point_cumulative_m(track: &Track, elapsed_ms: i64) -> f64 {
    let mut previous = (0i64, 0.0f64);
    for point in &track.locations {
        let t = point.totalTime * 1000;
        if t >= elapsed_ms {
            if t == previous.0 {
                return point.totalDis;
            }
            let fraction =
                ((elapsed_ms - previous.0) as f64 / (t - previous.0) as f64).clamp(0.0, 1.0);
            return previous.1 + (point.totalDis - previous.1) * fraction;
        }
        previous = (t, point.totalDis);
    }
    previous.1
}

/// 两条距离口径（10 秒窗 / 点位累计）的偏差。
///
/// 逐窗口比对而不是逐时刻比对累计前缀：窗口内若有多个不同速度的采样段，
/// 「窗口按恒速把 10 秒摊开」与「点位折线在窗内是折线」之间会有最多约 1.5 m 的
/// 插值不确定度（纯定义差异，不是数据不一致）。而**每个窗口的增量**在两条口径下
/// 都是「按时间比例切分跨越边界的那个采样段」，可以精确对齐 —— 并且逐窗口比对
/// 严格强于累计前缀比对（每个窗口都对上 ⇒ 每个窗口边界的累计都对上）。
fn worst_window_gap(track: &Track) -> (f64, i64) {
    let mut worst = (0.0f64, 0i64);
    for ((begin, end), window) in track.window_bounds_ms().iter().zip(&track.speedPerTenSec) {
        let accrued = point_cumulative_m(track, end - track.startTime)
            - point_cumulative_m(track, begin - track.startTime);
        let gap = (accrued - window.value).abs();
        if gap > worst.0 {
            worst = (gap, begin - track.startTime);
        }
    }
    worst
}

fn keys(v: &Value) -> BTreeSet<String> {
    v.as_object().expect("对象").keys().cloned().collect()
}

fn map_keys(m: &serde_json::Map<String, Value>) -> BTreeSet<String> {
    m.keys().cloned().collect()
}

fn set(items: &[&str]) -> BTreeSet<String> {
    items.iter().map(|s| s.to_string()).collect()
}

fn gunzip_b64(s: &str) -> String {
    use flate2::read::GzDecoder;
    use std::io::Read;
    let raw = nekosportsworldtool::crypto::envelope::b64_decode(s).expect("base64");
    let mut d = GzDecoder::new(&raw[..]);
    let mut out = String::new();
    d.read_to_string(&mut out).expect("gzip");
    out
}

/// 点位响应夹具。
///
/// `CHECKPOINTS` 本身就是 GCJ（第 16 轮已按真机"不做换算"的口径统一），
/// 因此这里**原样**作为 `lat/lon`，不再做 BD→GCJ 转换 —— 之前遗留的那次转换
/// 会把夹具整体平移约 600m，令 `live_points()` 与 `track` 用的 CHECKPOINTS 不同源。
fn live_points() -> Vec<Value> {
    CHECKPOINTS
        .iter()
        .enumerate()
        .map(|(i, (lat, lon))| {
            serde_json::json!({
                "pointName": format!("点位{}", i + 1),
                "lat": lat, "lon": lon,
                "isFixed": i % 2 == 0,
                "position": 999,
            })
        })
        .collect()
}

#[test]
fn full_offline_ios_submission_chain_is_protocol_complete() {
    let track = build(1200.0, 540, 2026, (22.4169, 112.9472), START_MS, &CHECKPOINTS);
    track.validate_consistency().expect("轨迹自洽");

    // ── ① OBS 对象：11 键，全部 gzip+base64 ─────────────────────────
    let pts = live_points();
    let obj = build_obs_object(&track, 1_356_050_720, "UUID-TEST", UID, &pts, 7);
    assert_eq!(
        keys(&obj),
        set(&[
            "rrid", "uuid", "uid", "run_data", "fixed_point_json", "step_freq_json",
            "speed_json", "segment_json", "runFaceCheck", "extension_json", "laps_json",
        ]),
        "OBS 顶层键集必须等于 IPA buildObsDataWithRrid 的 11 键"
    );

    // run_data 解包后必须是 22 键点位
    let wrap: Value = serde_json::from_str(&gunzip_b64(obj["run_data"].as_str().unwrap())).unwrap();
    assert_eq!(wrap["useZip"], false);
    let run_pts: Vec<Value> =
        serde_json::from_str(wrap["allLocJson"].as_str().unwrap()).unwrap();
    assert!(!run_pts.is_empty());
    let point_keys = set(&[
        "id", "flag", "speed", "avgSpeed", "radius", "gainTime", "type", "totalDis",
        "totalTime", "validDis", "validTime", "lat", "lng", "gLat", "gLng",
        "coorType", "bdA", "bdD", "bdS", "locType", "stepDistance", "queueNum",
    ]);
    for p in &run_pts {
        assert_eq!(keys(p), point_keys, "点位键集必须等于 IPA 的 22 键");
        // 数值字段是字符串（实机 stringWithFormat），lat/lng/locType 是数字 0
        assert!(p["gLat"].is_string() && p["gLng"].is_string());
        assert!(p["speed"].is_string() && p["radius"].is_string());
        assert_eq!(p["lat"], 0);
        assert_eq!(p["lng"], 0);
        assert_eq!(p["locType"], 0);
    }

    // 10s 窗：5 键，且总时长等于轨迹总时长
    let speed_json: Value = serde_json::from_str(&gunzip_b64(obj["speed_json"].as_str().unwrap())).unwrap();
    let step_json: Value = serde_json::from_str(&gunzip_b64(obj["step_freq_json"].as_str().unwrap())).unwrap();
    let sp = speed_json.as_array().unwrap();
    let st = step_json.as_array().unwrap();
    assert_eq!(sp.len(), st.len());
    assert!(!sp.is_empty());
    // 服务端看到的是这两个数组本身：线级总量必须与 track 对得上（不是内存里的 f64）。
    let emitted_distance: f64 = sp
        .iter()
        .map(|w| w["distance"].as_str().unwrap().parse::<f64>().unwrap())
        .sum();
    assert!(
        (emitted_distance - track.totalDistance).abs() <= 1.0,
        "Σ 窗口 distance = {emitted_distance:.2} 与 totalDis {:.2} 不一致",
        track.totalDistance
    );
    let emitted_steps: i64 = st.iter().map(|w| w["stepsNum"].as_i64().unwrap()).sum();
    assert!(
        (emitted_steps - track.totalSteps).abs() <= 1,
        "Σ 窗口 stepsNum = {emitted_steps} 与 totalSteps {} 不一致",
        track.totalSteps
    );
    assert_eq!(
        keys(&sp[0]),
        set(&["beginTime", "endTime", "distance", "queueNum", "state"])
    );
    assert_eq!(
        keys(&st[0]),
        set(&["beginTime", "endTime", "stepsNum", "queueNum", "state"])
    );
    let win_total: i64 = sp
        .iter()
        .map(|w| (w["endTime"].as_i64().unwrap() - w["beginTime"].as_i64().unwrap()) / 1000)
        .sum();
    assert_eq!(win_total, track.totalTime, "窗口累计时长必须等于轨迹总时长");

    // laps：11 键
    let laps: Value = serde_json::from_str(&gunzip_b64(obj["laps_json"].as_str().unwrap())).unwrap();
    let laps = laps.as_array().unwrap();
    assert!(!laps.is_empty());
    assert_eq!(
        keys(&laps[0]),
        set(&[
            "lapIndex", "distance", "duration", "cumulativeDuration", "avgPace",
            "avgCadence", "avgStride", "endAltAbs", "endAltRel", "elevationGain",
            "isFullLap",
        ])
    );
    // 圈数与单位：只在整公里建圈，每圈 distance 恰好 1000，
    // avgPace 是秒/公里、avgStride 是米/步（见 SWLapManager.saveLap 的寄存器映射）。
    assert_eq!(
        laps.len(),
        (track.totalDistance / 1000.0).floor() as usize,
        "圈数应为整公里数（末段不足 1km 不建圈）"
    );
    for lap in laps {
        assert_eq!(lap["distance"].as_f64().unwrap(), 1000.0);
        assert_eq!(lap["isFullLap"], true);
        let pace = lap["avgPace"].as_f64().unwrap();
        assert!((150.0..=900.0).contains(&pace), "avgPace 应为秒/公里: {pace}");
        let stride = lap["avgStride"].as_f64().unwrap();
        assert!((0.3..=2.5).contains(&stride), "avgStride 应为米/步: {stride}");
        let cad = lap["avgCadence"].as_f64().unwrap();
        assert!((80.0..=240.0).contains(&cad), "avgCadence 应为步/分: {cad}");
    }

    // 7.4.0 里 `record.pointData` 恒为空串（记录创建块 `sub_1000D2D14` 明确写入空 NSString），
    // 因此 `_buildFivePointInfo` 的 `fivePointJson` 恒为 `"[]"`。
    // 点位的 `flag` 必须全部等于同一个记录 id（真机读 `[builder recordID]`）。
    {
        let wrap: Value =
            serde_json::from_str(&gunzip_b64(obj["fixed_point_json"].as_str().unwrap())).unwrap();
        assert_eq!(wrap["fivePointJson"].as_str().unwrap(), "[]", "pointData 恒空 → \"[]\"");
        assert_eq!(wrap["runAreaId"].as_i64().unwrap(), -1);
        assert_eq!(wrap["geoFencesJson"].as_str().unwrap(), "[]");
        assert!(!wrap["freedomShowFence"].as_bool().unwrap());
        assert!(!wrap["useZip"].as_bool().unwrap());

        let run_wrap: Value =
            serde_json::from_str(&gunzip_b64(obj["run_data"].as_str().unwrap())).unwrap();
        let locs: Value = serde_json::from_str(run_wrap["allLocJson"].as_str().unwrap()).unwrap();
        for p in locs.as_array().unwrap() {
            assert_eq!(p["flag"].as_i64().unwrap(), 7, "点位 flag 必须是记录 id");
        }
    }

    // 两个 OBS key
    // 真机只上传一个 OBS 键：run_data/{yyyyMMddHH}/{uuid}.json
    let ks = obs_keys(&track, 1_356_050_720, "UUID-TEST");
    assert_eq!(ks.len(), 1);
    assert!(ks[0].starts_with("run_data/"));
    assert!(ks[0].ends_with("/UUID-TEST.json"));

    // ── ② 五点 ─────────────────────────────────────────────────────
    // flag 必须与 OBS 点位用的是同一个记录 id（真机两处都读 [builder recordID]）
    let five = five_point_wrapper(&pts, 7);
    validate_five_point_wrapper(&five).expect("五点必须通过自身校验");
    let outer: Value = serde_json::from_str(&five).unwrap();
    assert_eq!(
        keys(&outer),
        set(&["useZip", "fivePointJson", "runAreaId", "geoFencesJson", "freedomShowFence"])
    );
    // 7.4.0 里 `record.pointData` 恒为空串（记录创建块写入空 NSString）→ 恒发 "[]"。
    let echoed: Vec<Value> = serde_json::from_str(outer["fivePointJson"].as_str().unwrap()).unwrap();
    assert!(echoed.is_empty(), "pointData 恒空，fivePointJson 必须是 []");
    assert_eq!(outer["geoFencesJson"], "[]");
    assert_eq!(outer["runAreaId"], -1);
    assert_eq!(outer["useZip"], false);
    assert_eq!(outer["freedomShowFence"], false);
    // 十键实体构造仍然存在（供本地判定/调试），但**不进入上传体**。
    let payload = five_point_payload(&pts, track.startTime);
    assert_eq!(payload.len(), CHECKPOINTS.len());
    for p in &payload {
        assert_eq!(
            keys(p),
            set(&["id", "flag", "isPass", "lat", "lon", "glat", "glon", "position", "isFixed", "pointName"])
        );
        assert!(p["glat"].is_string() && p["glon"].is_string(), "五点坐标必须是字符串，不能是 null");
    }

    // ── ③ iOS 提交 body ────────────────────────────────────────────
    // body 里的派生量全部从轨迹反推（与 api::submit::submit_record 同一套公式）。
    // 写死常量会让轨迹一变就静默失配 —— 这正是此前 originalSign 事故的同款风险。
    let total_time = track.totalTime;
    let total_dis = track.totalDistance;
    let total_steps = track.totalSteps;
    let kcal = submitted_calorie(WEIGHT_KG, total_dis);
    let power = avg_power(WEIGHT_KG, total_dis, total_time);
    let ascent = total_ascent(&track.locations) as i64;
    // 真机 `_buildStepAndSpeedInfo`：totalSteps==0 → 0，否则截断 totalSteps/(totalTime/60)。
    let avg_step_freq = if total_steps == 0 {
        0
    } else {
        (total_steps as f64 / (total_time as f64 / 60.0)) as i64
    };
    // iOS：round2(分/公里) * 1000（IPA buildGradeWithCompletion 的 speed 字段）
    let speed_ios = {
        let km = (total_dis * 100.0).ceil() / 100.0 / 1000.0;
        (round_to(total_time as f64 / 60.0 / km, 2) * 1000.0) as i64
    };

    let body = build_ios_body(&IosBodyInput {
        five_point_json: five.clone(),
        run_uuid: "FAB5C889-71AC-411F-9EC2-4677B3AFF30F".into(),
        uid: UID,
        unid: UNID,
        policy: 1,
        sport_type: 1,
        total_time,
        start_ms: track.startTime,
        stop_ms: track.startTime + total_time * 1000,
        min_run_time: 300,
        min_distance: 1000,
        total_dis,
        speed: speed_ios,
        kcal,
        address: "江门市".into(),
        ascent,
        power,
        total_steps,
        avg_step_freq,
        face_check: 0, // 该场地未启用真人核验
        complete: true,
        uncomplete_reason: 0,
    });

    assert_eq!(
        map_keys(&body),
        set(&IOS_BODY_KEYS),
        "iOS body 键集必须等于 IPA buildGradeWithCompletion 的 38 键（含 GeeGuard 回执）"
    );

    // ── ④ 签名：服务端视角重算 ─────────────────────────────────────
    let body_val = Value::Object(body.clone());
    let sig = body_val["signature"].as_str().unwrap().to_string();
    let orig = body_val["originalSign"].as_str().unwrap().to_string();

    let mut server_view = body_val.as_object().unwrap().clone();
    for k in ["segmentJson", "speedPerTenSec", "stepsPerTenSec", "signature", "originalSign"] {
        server_view.remove(k);
    }
    assert_eq!(
        signature_ios(&Value::Object(server_view.clone())),
        sig,
        "服务端重算签名必须等于提交的 signature"
    );
    assert_eq!(
        original_sign_ios(&Value::Object(server_view)),
        orig,
        "originalSign 必须等于签名输入明文"
    );

    // 签名输入 = 38 − 2(排除) − 5(后插) = 31 键（geeToken/errorCode 参与签名）
    let signed: BTreeSet<String> = body
        .keys()
        .filter(|k| !IOS_SIGN_EXCLUDED.contains(&k.as_str()))
        .filter(|k| {
            !["segmentJson", "speedPerTenSec", "stepsPerTenSec", "signature", "originalSign"]
                .contains(&k.as_str())
        })
        .cloned()
        .collect();
    assert_eq!(signed.len(), 31);
    assert!(signed.contains("geeToken") && signed.contains("errorCode"));

    // ── ⑤ 跨层一致性 ───────────────────────────────────────────────
    // body.totalDis 必须与 OBS 轨迹、track 一致
    assert_eq!(body_val["totalDis"].as_i64().unwrap(), track.totalDistance as i64);
    assert_eq!(body_val["totalTime"].as_i64().unwrap(), track.totalTime);
    assert_eq!(body_val["validDis"].as_i64().unwrap(), body_val["totalDis"].as_i64().unwrap());
    assert_eq!(body_val["validTime"].as_i64().unwrap(), track.totalTime);
    // 派生量必须与轨迹本身的关系式一致（不是写死常量）
    assert_eq!(body_val["totalSteps"].as_i64().unwrap(), track.totalSteps);
    // 提交体 calorie = (long)(km × 体重 × 1.036)
    assert_eq!(
        body_val["calorie"].as_i64().unwrap(),
        submitted_calorie(WEIGHT_KG, track.totalDistance)
    );
    // MET 公式只用于界面展示，仍然要能算
    assert!(display_kcal(WEIGHT_KG, track.totalTime, track.totalDistance) > 0);
    assert_eq!(body_val["avgPower"].as_i64().unwrap(), avg_power(WEIGHT_KG, track.totalDistance, track.totalTime));
    assert_eq!(body_val["totalAscent"].as_i64().unwrap(), total_ascent(&track.locations) as i64);
    // 真机口径：截断（FCVTZU），不是四舍五入
    assert_eq!(
        body_val["avgStepFreq"].as_i64().unwrap(),
        (track.totalSteps as f64 / (track.totalTime as f64 / 60.0)) as i64
    );
    // 结束时刻 = 开始 + 总时长
    assert_eq!(
        body_val["stopTime"].as_i64().unwrap() - body_val["startTime"].as_i64().unwrap(),
        track.totalTime * 1000
    );
    // `roomId` 只在 sportType==3 时才写（`if (sportType == 3) setObject:recordId forKey:@"roomId"`），
    // 本工具固定 sportType=1，因此绝不能出现这个键。
    assert!(
        !body_val.as_object().unwrap().contains_key("roomId"),
        "sportType=1 不得写 roomId"
    );

    // body 的 `policy` 必须等于 `sportType`（真机记录创建块里两者同源 = cer.runType）
    assert_eq!(body_val["policy"].as_i64().unwrap(), 1);
    assert_eq!(
        body_val["policy"].as_i64().unwrap(),
        body_val["sportType"].as_i64().unwrap(),
        "policy 必须与 sportType 相同"
    );

    // 该场地未启用核验 → faceCheck 必须是 0（与 policy 一致）
    assert_eq!(body_val["faceCheck"].as_i64().unwrap(), 0);
    // complete 必须是字符串（实机 CFSTR("true")），且与 unCompleteReason 自洽
    assert!(body_val["complete"].is_string(), "complete 必须是字符串");
    assert_eq!(body_val["complete"], "true");
    assert_eq!(body_val["unCompleteReason"].as_i64().unwrap(), 0);
    // getPrize / unauthorized 同样是字符串
    assert!(body_val["getPrize"].is_string());
    assert!(body_val["unauthorized"].is_string());
}

/// 轨迹类型必须只包含「绿色」类型 + 起终点哨兵，不能出现灰(3)/红(7,10)/隐藏(-1)。
#[test]
fn generated_track_only_emits_green_types() {
    let track = build(1200.0, 540, 7, (22.4169, 112.9472), START_MS, &CHECKPOINTS);
    for p in &track.locations {
        assert!(
            matches!(p.ptype, 0 | 1 | 2 | 8 | 5 | 6),
            "点 {} 出现非绿色类型 {}",
            p.id,
            p.ptype
        );
    }
}

/// road 模式（`RouteMode::Road`）也是提交路径，必须同样通过运行时护栏。
/// 单位测试里只有 `build_road` 的窗口/哨兵断言，没有任何地方对它调用过
/// `validate_consistency` —— 这里补上，避免两条生成路径的护栏覆盖不对称。
#[test]
fn road_track_passes_the_runtime_guard() {
    use nekosportsworldtool::track::generate_road::build_road;
    use route_planner::{graph::EdgeData, Coord, RoadGraph};

    let checkpoints = [
        (38.901678, 121.540241),
        (38.902564, 121.541233),
        (38.900921, 121.542310),
        (38.899823, 121.541010),
        (38.900455, 121.539512),
    ];
    let clat = checkpoints.iter().map(|p| p.0).sum::<f64>() / checkpoints.len() as f64;
    let clng = checkpoints.iter().map(|p| p.1).sum::<f64>() / checkpoints.len() as f64;

    let mut net = RoadGraph::new();
    let nodes: Vec<_> = (0..7)
        .flat_map(|y| (0..7).map(move |x| (x, y)))
        .map(|(x, y)| {
            net.add_node(Coord::new(
                clng + (x as f64 - 3.0) * 0.0012,
                clat + (y as f64 - 3.0) * 0.0012,
            ))
        })
        .collect();
    for y in 0..7 {
        for x in 0..7 {
            for (dx, dy) in [(1, 0), (0, 1)] {
                if x + dx >= 7 || y + dy >= 7 {
                    continue;
                }
                let edge = EdgeData {
                    way_id: (y * 7 + x + 1) as u64,
                    oneway: false,
                    highway: "residential".into(),
                    maxspeed_kmh: None,
                    length_m: 111.0,
                };
                let a = nodes[y * 7 + x];
                let b = nodes[(y + dy) * 7 + x + dx];
                net.add_edge(a, b, edge.clone());
                net.add_edge(b, a, edge);
            }
        }
    }
    net.compute_anchor();
    net.rebuild_index();

    for seed in 0..4u64 {
        let track = build_road(
            3300.0,
            1220,
            seed,
            START_MS,
            &checkpoints,
            &checkpoints[1..],
            &net,
        )
        .expect("road fixture");
        track
            .validate_consistency()
            .unwrap_or_else(|e| panic!("road seed={seed} 未通过运行时护栏: {e}"));

        let chord = track.polyline_length_m();
        let ratio = chord / track.totalDistance;
        let (gap, at) = worst_window_gap(&track);
        println!(
            "road seed={seed}: 折线/totalDis={ratio:.5} 最大逐窗偏差={gap:.3}m@{}s",
            at / 1000
        );
        assert!(
            (0.995..=1.005).contains(&ratio),
            "road seed={seed}: 折线实长 {chord:.3}m / totalDis {:.3}m = {ratio:.5}",
            track.totalDistance
        );
        assert!(
            gap <= 1.0,
            "road seed={seed}: {}s 起的 10 秒窗与点位在该窗内的位移相差 {gap:.2}m",
            at / 1000
        );
    }
}
/// 速度三件套的真实感与自洽。
///
/// 首点 `totalDis=0/totalTime=0` → `avgSpeed=speed=bdS=0`（真机样本首点即全零）；
/// `speed` 是配速，必须恒等于 16.6667/avgSpeed；
/// `bdS`（瞬时 m/s）必须**围绕** avgSpeed 波动，不能恒低于平均
/// （真实样本里 bdS > avgSpeed 占 94.6%；我们匀速无暂停，取 0.90~1.10 对称噪声）。
#[test]
fn speed_fields_are_consistent_and_realistic() {
    let track = build(1200.0, 540, 2026, (22.4169, 112.9472), START_MS, &CHECKPOINTS);
    let p0 = &track.locations[0];
    assert_eq!((p0.totalDis, p0.totalTime), (0.0, 0));
    assert_eq!((p0.avgSpeed, p0.speed, p0.bdS), (0.0, 0.0, 0.0), "首点速度必须全零");

    let mut above = 0;
    let mut below = 0;
    for p in track.locations.iter().filter(|p| p.avgSpeed > 1e-6) {
        let expect = 16.666_666_666_666_668 / p.avgSpeed;
        assert!(
            (p.speed - expect).abs() < 1e-3,
            "speed 必须是配速 min/km: speed={} 期望={}",
            p.speed,
            expect
        );
        let r = p.bdS / p.avgSpeed;
        assert!((0.85..=1.15).contains(&r), "bdS/avgSpeed 越界: {r}");
        if p.bdS > p.avgSpeed { above += 1; } else if p.bdS < p.avgSpeed { below += 1; }
    }
    assert!(above > 0 && below > 0, "bdS 应围绕 avgSpeed 双向波动: above={above} below={below}");
}

/// P1/P2/P3 回归护栏：三条都是「两条口径必须同源」。
///
/// - P1：折线实长 / totalDis（服务端会用坐标重算距离）
/// - P2：10 秒窗与点位在**同一窗口内**的位移必须相等（逐窗口严格强于累计前缀）
/// - P3：Σ stepsPerTenSec vs totalSteps
#[test]
fn track_series_agree_on_distance_steps_and_windows() {
    for (dist, dur) in [(1200.0, 540i64), (3300.0, 1220), (1050.0, 480)] {
        for seed in [7u64, 2026] {
            let track = build(dist, dur, seed, (22.4169, 112.9472), START_MS, &CHECKPOINTS);
            let steps: f64 = track.stepsPerTenSec.iter().map(|w| w.value).sum();

            let chord = track.polyline_length_m();
            let ratio = chord / track.totalDistance;
            let (gap, at) = worst_window_gap(&track);
            println!(
                "dist={dist} dur={dur} seed={seed}: 折线/totalDis={ratio:.5}({chord:.2}/{:.2}m) 最大逐窗偏差={gap:.3}m@{}s Σ步数-totalSteps={:+.1}",
                track.totalDistance,
                at / 1000,
                steps - track.totalSteps as f64
            );

            assert!(
                (0.995..=1.005).contains(&ratio),
                "dist={dist} dur={dur} seed={seed}: 折线实长 {chord:.3}m / totalDis {:.3}m = {ratio:.5}，\
                 服务端按坐标重算会得到与 totalDis 不一致的距离",
                track.totalDistance
            );

            assert!(
                gap <= 1.0,
                "dist={dist} dur={dur} seed={seed}: {}s 起的 10 秒窗与点位在该窗内的位移相差 {gap:.2}m",
                at / 1000
            );
            // 终点：两条口径都必须落在 totalDistance（服务端只会核对总量与窗口总量）。
            let window_total = window_cumulative_m(&track, track.totalTime * 1000);
            let point_total = point_cumulative_m(&track, track.totalTime * 1000);
            assert!(
                (window_total - track.totalDistance).abs() <= 1.0
                    && (point_total - track.totalDistance).abs() <= 1.0,
                "dist={dist} dur={dur} seed={seed}: 终点窗口 {window_total:.2}m / 点位 {point_total:.2}m / 声明 {:.2}m",
                track.totalDistance
            );

            assert!(
                (steps - track.totalSteps as f64).abs() <= 1.0,
                "dist={dist} dur={dur} seed={seed}: Σ stepsPerTenSec = {steps:.1} 与 totalSteps = {} 不一致",
                track.totalSteps
            );
        }
    }
}

/// 室内跑的离线端到端：策略请求体 → 18 键 body → 签名 → 3 键 OBS。
///
/// 与室外用例对等，覆盖 7.4.0 上两条独立链路中的另一条。
#[test]
fn indoor_chain_is_protocol_complete() {
    use nekosportsworldtool::api::indoor::{
        build_indoor_body, policy_for, sport_type_for, uncomplete_reason, IndoorBodyInput,
        INDOOR_BODY_KEYS,
    };
    use nekosportsworldtool::track::indoor::generate;

    // ── ① 策略请求体：同一端点，runMode=5 ──────────────────────────
    let pol = nekosportsworldtool::api::policy::policy_body(5, 0, 0, "47203");
    assert_eq!(pol["runMode"], 5, "室内策略的 runMode 必须是 5");
    assert_eq!(pol.as_object().unwrap().len(), 5);

    // ── ② 步数生成 ────────────────────────────────────────────────
    let track = generate(2400, 1200, 77).expect("室内生成");
    assert_eq!(track.window_sum(), 2400);
    assert_eq!(track.windows.len(), 120);

    // ── ③ 18 键 body ──────────────────────────────────────────────
    let input = IndoorBodyInput {
        uuid: "INDOOR-UUID-1".into(),
        selected_unid: "47203".into(),
        record_policy: 0,
        start_ms: START_MS,
        stop_ms: START_MS + 1_200_000,
        total_time: 1200,
        total_steps: 2400,
        latitude: 22.4169,
        longitude: 112.9472,
        min_steps: 1500,
        min_run_time: 600,
        max_run_time: 7200,
    };
    let body = build_indoor_body(&input, nekosportsworldtool::crypto::sign::SALT);
    assert_eq!(body.len(), 18);
    for k in INDOOR_BODY_KEYS {
        assert!(body.contains_key(k), "室内 body 缺 {k}");
    }
    // 绝不能出现室外字段
    for k in ["allLocJson", "fivePointJson", "totalDis", "calorie", "avgPower",
              "address", "selDistance", "goalId"] {
        assert!(!body.contains_key(k), "室内 body 不得含 {k}");
    }
    // 查表：policy=0 → policy 0 / sportType 1
    assert_eq!(body["policy"], policy_for(0));
    assert_eq!(body["sportType"], sport_type_for(0));
    assert_eq!(body["sportType"], 1);
    // 经纬度是 "%.6f" 字符串
    assert_eq!(body["latitude"], "22.416900");
    assert_eq!(body["longitude"], "112.947200");
    assert!(body["latitude"].is_string() && body["longitude"].is_string());
    // 判定期望：2400 ≥ 1500 且 1200 ≥ 600 → 达标
    assert_eq!(body["complete"], "true");
    assert_eq!(body["unCompleteReason"], uncomplete_reason(1200, 2400, 600, 1500));
    assert_eq!(body["unCompleteReason"], 0);
    assert_eq!(body["stepsPerTenSec"].as_array().unwrap().len(), 0);

    // ── ④ 签名只覆盖前 16 键 ──────────────────────────────────────
    let mut pre = body.clone();
    pre.remove("signature");
    pre.remove("stepsPerTenSec");
    assert_eq!(pre.len(), 16, "参与签名的必须是 16 键");
    let expect = nekosportsworldtool::crypto::sign::indoor_signature(
        &Value::Object(pre),
        nekosportsworldtool::crypto::sign::SALT,
    );
    assert_eq!(body["signature"].as_str().unwrap(), expect);

    // ── ⑤ OBS 3 键 ────────────────────────────────────────────────
    let obj = track.build_obs_object("INDOOR-UUID-1", UID, START_MS);
    assert_eq!(obj.as_object().unwrap().len(), 3);
    for k in ["uuid", "step_json", "uid"] {
        assert!(obj[k].is_string(), "室内 OBS {k} 应为 gzipBase64 字符串");
    }
    assert!(!obj.as_object().unwrap().contains_key("rrid"));
    let steps: Vec<Value> =
        serde_json::from_str(&gunzip_b64(obj["step_json"].as_str().unwrap())).unwrap();
    let sum: i64 = steps.iter().map(|w| w["stepsNum"].as_i64().unwrap()).sum();
    assert_eq!(sum, 2400, "step_json 的 ΣstepsNum 必须等于 totalSteps");
    for w in &steps {
        assert_eq!(w.as_object().unwrap().len(), 5, "step_json 元素应为 5 键");
    }
}
