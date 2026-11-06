//! 全链编排：policy → 实时点位 → 轨迹生成 → 提交 → OBS → 详情验证。
//! 由 UI 后台线程调用，log 闭包回传日志。

use super::client::ApiClient;
use super::model::Session;
use super::points;
use super::records::fetch_one_record;
use super::submit::{submit_record, SubmitParams, SubmitResult};
use crate::location::Coordinate;
use crate::track::generate_road::RouteMode;
use crate::track::generator::build as gen_track;
use crate::track::wire::{build_obs_object, five_point_wrapper, obs_keys};
use serde_json::Value;

#[derive(Clone)]
pub struct RunParams {
    /// 跑法（= 策略请求的 `runMode`，也是记录 `sportType`/`policy` 的索引）。
    ///
    /// 0 随机跑 / 1 顺序跑（校园跑、计分跑）/ 2 定向跑 / 3 约跑 / 4 自由跑 /
    /// 5 室内 / 6 户外跑。来源：`+[SWRunPrepareImpl runPrepareImplWithRunPolicy:]`
    /// @0x10018a678 的类表 `off_104CA3798`。
    pub run_mode: i64,
    /// 距离（米）与时长（秒）已由 UI 参数解析。
    pub dist: f64,
    pub dur: i64,
    /// 开始时间（毫秒）。
    pub start_ms: i64,
    pub face_check: i64,
    /// 用户手动填写的绝对海拔（米）；None 使用生成器海拔。
    pub manual_altitude: Option<f64>,
    /// 用户手动填写的海拔范围；与单值字段兼容，范围优先。
    pub manual_altitude_range: Option<crate::track::altitude::AltitudeRange>,
    pub seed: u64,
    /// 路线算法模式。
    pub route_mode: RouteMode,
    /// 只生成轨迹与自检，不提交（CLI `--dry-run`）。
    pub dry_run: bool,
    /// `--dry-run` 时把轨迹写成 JSON（GCJ），供离线叠图核对；None 不写。
    pub out_path: Option<String>,
}

pub struct RunOutcome {
    pub result: SubmitResult,
    pub obs_ok: usize,
    pub detail_ok: bool,
}

/// 阶段间等待：以毫秒级随机区间替代固定整数秒。
/// 固定 2s/1s/2s 的节拍是自动化脚本的典型指纹；真人操作的阶段间隔是连续分布的。
/// 预检用：参数里的开始时刻（尚未叠加随机偏移）。
fn start_ms_for_check(p: &RunParams) -> i64 {
    p.start_ms
}

/// 把整段记录钳进服务端下发的有效时间窗。
///
/// 窗口是「每天 HH:mm–HH:mm」的本地时间；跨零点的窗口按 +24h 处理。
/// 时长超过窗口长度时无法满足，直接报错（发出去也只会被拒）。
fn clamp_into_valid_window(
    start_ms: i64,
    dur: i64,
    pol: &crate::api::policy::PolicyInfo,
    log: &mut dyn FnMut(&str),
) -> Result<i64, String> {
    use crate::api::policy::time_of_day_minutes;
    use chrono::{Local, TimeZone};

    let (Some(s), Some(e)) = (pol.valid_start.as_deref(), pol.valid_end.as_deref()) else {
        return Ok(start_ms);
    };
    let (Some(ws), Some(we)) = (time_of_day_minutes(s), time_of_day_minutes(e)) else {
        log(&format!("⚠ [policy] 有效时间窗格式无法解析（{s}~{e}），跳过窗口校验"));
        return Ok(start_ms);
    };
    if ws == we {
        return Ok(start_ms);
    }
    let win = if we > ws { we - ws } else { we + 24 * 60 - ws };
    if dur / 60 > win {
        return Err(format!(
            "本次时长 {}s 超过服务端有效时间窗 {}~{}（{} 分钟），无法提交",
            dur, s, e, win
        ));
    }
    let day = Local
        .timestamp_millis_opt(start_ms)
        .single()
        .ok_or("开始时刻无法解析为本地时间")?
        .date_naive();
    let base = Local
        .from_local_datetime(&day.and_hms_opt(0, 0, 0).unwrap())
        .single()
        .map(|d| d.timestamp_millis())
        .ok_or("本地零点无法换算")?;
    let sod = (start_ms - base).div_euclid(60_000);
    // 相对窗口起点的偏移；负数/超界都要平移到最近的合法位置
    let rel = (sod - ws).rem_euclid(24 * 60);
    if rel <= win - dur / 60 {
        return Ok(start_ms);
    }
    let new_start = base + ws * 60_000;
    log(&format!(
        "⚠ [policy] 记录时段落在服务端有效时间窗 {s}~{e} 之外，开始时刻已前移到 {}",
        Local
            .timestamp_millis_opt(new_start)
            .single()
            .map(|t| t.format("%Y-%m-%d %H:%M:%S").to_string())
            .unwrap_or_default()
    ));
    Ok(new_start)
}

fn sleep_jitter(lo_ms: u64, hi_ms: u64) {
    let span = hi_ms.saturating_sub(lo_ms).max(1);
    let ms = lo_ms + (rand::random::<u64>() % span);
    std::thread::sleep(std::time::Duration::from_millis(ms));
}

/// 非顺序跑（户外跑/自由跑）的轨迹中心（GCJ-02）。
///
/// 这两类跑法服务端不下发围栏（实测 runMode=2/3/4/6 的 `geoFence` 都是 null），
/// 所以中心只能来自设备锚点。锚点是用户在设备页填的坐标，历史上按 **BD-09** 录入
/// （把 BD 当 GCJ 用会整体偏 ~900m），而上传体必须是 GCJ。
///
/// 判定办法不靠猜：另外拉一次计分跑（runMode=1）的策略，用服务端下发的学校围栏
/// （GCJ）当参照，比较「锚点原值」和「锚点 BD→GCJ」哪个离围栏更近。
fn free_run_center(
    client: &mut ApiClient,
    anchor: Coordinate,
    log: &mut dyn FnMut(&str),
) -> Result<(f64, f64), String> {
    let raw = (anchor.latitude, anchor.longitude);
    let converted = crate::track::wire::bd09_to_gcj02(raw.0, raw.1);
    // 参照物：学校围栏（GCJ）质心。
    let reference = crate::api::policy::fetch_policy_raw(client, 1).ok().and_then(|env| {
        let data = env.get("data").cloned().unwrap_or(env);
        fence_centroid_gcj(&data)
    });
    let Some(fc) = reference else {
        log("⚠ [track] 取不到学校围栏，无法判定锚点坐标系，按 GCJ 原值使用锚点");
        return Ok(raw);
    };
    let gap = |p: (f64, f64)| crate::track::geom::local_distance_m(p.0, p.1, fc.0, fc.1);
    let (d_raw, d_conv) = (gap(raw), gap(converted));
    log(&format!(
        "[track] 学校围栏中心 GCJ=({:.6},{:.6})；锚点原值距它 {:.0}m，锚点 BD→GCJ 后距它 {:.0}m",
        fc.0, fc.1, d_raw, d_conv
    ));
    // 有围栏时直接用围栏质心当中心：围栏就是这块场地的边界，质心比"人站的位置"
    // 更接近跑道中心（人在跑道边上站着时锚点会偏出半个场地）。
    if d_raw.min(d_conv) < 400.0 {
        log(&format!(
            "√ [track] 用学校围栏质心当轨迹中心（锚点换算后 {:.0}m / 原值 {:.0}m）",
            d_conv, d_raw
        ));
        return Ok(fc);
    }
    if d_conv + 50.0 < d_raw {
        log(&format!(
            "√ [track] 锚点按 BD-09 录入，已换算成 GCJ（({:.6},{:.6})）",
            converted.0, converted.1
        ));
        Ok(converted)
    } else {
        Ok(raw)
    }
}

/// 从策略 `data.geoFence.geoFences[*].points` 取围栏质心（GCJ）。
///
/// 服务端围栏点的 `lat/lng` 恒为 0，真实坐标只在 `glat/glng`（实测），
/// 因此这里只读 `glat/glng`，不做任何换算。
fn fence_centroid_gcj(data: &Value) -> Option<(f64, f64)> {
    let fences = data.get("geoFence")?.get("geoFences")?.as_array()?;
    let mut lat = 0.0;
    let mut lng = 0.0;
    let mut n = 0.0;
    for f in fences {
        for p in f.get("points")?.as_array()? {
            let (Some(la), Some(lo)) = (
                p.get("glat").and_then(|v| v.as_f64()),
                p.get("glng").and_then(|v| v.as_f64()),
            ) else {
                continue;
            };
            if la == 0.0 && lo == 0.0 {
                continue;
            }
            lat += la;
            lng += lo;
            n += 1.0;
        }
    }
    (n > 0.0).then_some((lat / n, lng / n))
}

/// 跑步全链。
pub fn run_full_flow(
    client: &mut ApiClient,
    params: &RunParams,
    log: &mut dyn FnMut(&str),
) -> Result<RunOutcome, String> {
    let sess: Session = client.login.clone().ok_or("未登录")?;
    // 本机记录序号（真机是 Realm 自增主键，见 HeaderIdentity::record_seq）。
    // 每次提交 +1 并落盘，同一次记录的所有点共用同一个值。
    let record_seq = {
        let mut id = client.identity.clone();
        id.record_seq = id.record_seq.saturating_add(1);
        if let Err(e) = crate::api::model::save_identity(&id) {
            log(&format!("⚠ [track] 记录序号落盘失败（不影响本次提交）: {e}"));
        }
        client.identity.record_seq = id.record_seq;
        id.record_seq
    };
    // 清掉上一次（可能是别的账号）的详情判定，避免本次校验失败时弹窗里显示旧数据。
    if let Ok(mut slot) = VERIFY_DETAIL.lock() {
        slot.take();
    }

    // ① policy
    log(&format!("[policy] 拉取跑步策略（runMode={}）…", params.run_mode));
    let pol = crate::api::policy::fetch_policy_mode(client, params.run_mode)?;
    log(&format!(
        "√ [policy] ts={} policy={} minDistance={} minRunTime={} 有效时间窗={}~{} 真人核验={}",
        pol.timestamp,
        pol.policy,
        pol.min_distance,
        pol.min_run_time,
        pol.valid_start.as_deref().unwrap_or("未下发"),
        pol.valid_end.as_deref().unwrap_or("未下发"),
        pol.face_verify
    ));
    // 提交体的 `policy` 与 `sportType` 都来自 runMode 查表（见 ⑤ 处取证）。
    // 策略响应里的 `policy` 是另一条独立通道，两者应一致；不一致就提示核对。
    {
        let expect = crate::api::indoor::policy_for(params.run_mode);
        if pol.policy != expect {
            log(&format!(
                "⚠ [policy] 响应里的 policy={} 与 runMode 查表值 {} 不同，请核对（记录体仍按查表值提交）",
                pol.policy, expect
            ));
        }
    }

    // 兜底值会原样进提交体（selDistance/selRunTime/faceCheck），必须让用户看见。
    if !pol.defaults_used.is_empty() {
        log(&format!(
            "⚠ [policy] 服务端未下发以下字段，已用兜底值（会写进提交体）：{}",
            pol.defaults_used.join("、")
        ));
    }

    // 服务端认可的速度/步频窗口（`SWRunRule.speedBottom/speedTop/stepBottom/stepTop`）。
    // 单位未在二进制里定死，因此只回显供核对，不做强制。
    log(&format!(
        "[policy] 速度窗口={:?}~{:?} 步频窗口={:?}~{:?}（未强制，仅供核对）",
        pol.speed_bottom, pol.speed_top, pol.step_bottom, pol.step_top
    ));

    // 场地启用真人核验时直接停止：
    // 真机 `buildGradeWithCompletion:` 在 `faceCheckTypes` 非空时发 faceCheck=1（全部通过）
    // 或 2（有未通过），**没有"未核验"取值**。本工具无法完成人脸核验，
    // 硬发 0 会与 policy.faceVerify=true 直接矛盾，记录必然被判无效。
    if pol.face_verify {
        return Err(
            "该场地启用了真人核验（policy.faceVerify=true），本工具无法完成人脸核验，\
             提交会被判无效。已停止，未产生记录。"
                .into(),
        );
    }

    sleep_jitter(900, 2_600);

    // ② 实时点位（拒绝本地样本兜底）
    //
    // 只有**顺序跑（校园跑，runMode=1）**有点位/围栏概念：
    // `SWRunPrepareSequenceImpl` 的 `originDetectionType` 返回 1（必须识别起点）、
    // `spilePointFetchStrategy` 是 `SWSpilePointSequenceStrategy`。
    // 户外跑（6）/自由跑（4）的 `originDetectionType` 都是 0、策略类是
    // Outdoor/Free，而户外跑的 `runCondition` 直接是 nil —— 没有点位就不该去拉点位。
    let scored = params.run_mode == 1;
    if !scored {
        log(&format!(
            "[points] runMode={} 非顺序跑：不拉点位、不套围栏，轨迹围绕设备锚点生成",
            params.run_mode
        ));
    }
    if client.identity.has_unconfigured_default_location() {
        return Err("请先在设备信息页填写本次跑步所在城市和定位锚点，不能使用大连默认配置".into());
    }
    let anchor: Coordinate = client.identity.anchor_coordinate()?;
    let pts = if scored {
        log("[points] 拉取实时点位…");
        let fetched = points::fetch_points(client, anchor, log)?;
        if fetched.is_empty() {
            return Err("实时点位为空 —— 拒绝本地样本兜底".into());
        }
        fetched
    } else {
        Vec::new()
    };
    log(&format!("√ [points] {} 个点位", pts.len()));
    for p in pts.iter().take(5) {
        log(&format!(
            "  [points] {} BD=({:.6},{:.6}) GCJ=({},{})",
            p.get("pointName").and_then(|v| v.as_str()).unwrap_or(""),
            p.get("lat").and_then(|v| v.as_f64()).unwrap_or(0.0),
            p.get("lon").and_then(|v| v.as_f64()).unwrap_or(0.0),
            p.get("glat").map(|v| v.to_string()).unwrap_or_default(),
            p.get("glon").map(|v| v.to_string()).unwrap_or_default(),
        ));
    }

    // 坐标体系自检：真机 `SWLocation.coordinate` 返回的是原始 CLLocation（WGS-84），
    // 而上传层把实体坐标原样回传并标 `coorType="gcj02"`。如果用户锚点和服务端点位
    // 属于不同体系，两者中心会正好相差一个 BD↔GCJ 或 WGS↔GCJ 偏移量。
    // 这里只在差值**恰好落在偏移量附近**时告警，避免正常散布被误报。
    if !pts.is_empty() {
        let here = points::points_gcj(&pts);
        let n = here.len().max(1) as f64;
        let clat: f64 = here.iter().map(|p| p.0).sum::<f64>() / n;
        let clng: f64 = here.iter().map(|p| p.1).sum::<f64>() / n;
        let dlat = clat - anchor.latitude;
        let dlng = clng - anchor.longitude;
        let near = |v: f64, target: f64| (v - target).abs() < 0.0015;
        // BD↔GCJ 在华南约 (+0.0057, +0.0065)；WGS↔GCJ 约 (+0.0045, +0.0060)
        if (near(dlat, 0.0057) && near(dlng, 0.0065)) || (near(dlat, -0.0057) && near(dlng, -0.0065))
        {
            log(&format!(
                "⚠ [points] 锚点与点位中心相差 ({dlat:+.4},{dlng:+.4})°，\
                 恰好是 BD↔GCJ 偏移量 —— 锚点与点位的坐标体系可能不一致，请核对设备页填写方式"
            ));
        } else {
            log(&format!(
                "[points] 点位中心相对锚点偏移 ({dlat:+.4},{dlng:+.4})°"
            ));
        }
    }

    // ③ 轨迹生成（必经点 + 打卡点）
    //
    // **坐标系唯一**：上传体的 `gLat/gLng` 是 GCJ（`SWCoordinate.currentType` 恒 1 →
    // `coorType="gcj02"`），服务端点位的 `glat/glon` 也是 GCJ，因此轨迹必须直接用
    // GCJ 生成。此前这里用 `points_bd`（点位响应里的 BD `lat/lon`）生成轨迹再原样
    // 当 GCJ 上传，整条轨迹整体平移约 900m —— 记录位置落在校外，必然被判异常。
    let pts_gcj: Vec<(f64, f64)> = if scored {
        points::points_gcj(&pts)
    } else {
        // 非顺序跑没有点位：围绕「本场地的真实中心」合成一圈标准跑道采样点。
        // 中心不能直接用 identity 里的锚点 —— 锚点是按 BD 录入的，直接当 GCJ 用
        // 会让整条轨迹再偏 ~900m（这正是 11-06 那次记录的错法）。这里用服务端
        // 下发的学校围栏（GCJ）来判定锚点属于哪个体系，再换算。
        let center = free_run_center(client, anchor, log)?;
        log(&format!(
            "[track] 户外轨迹中心 GCJ=({:.6},{:.6})",
            center.0, center.1
        ));
        match crate::track::reference::samples_near(center, 300.0) {
            Some(samples) => {
                log("[track] 锚点所在场地有卫星描线，户外轨迹沿真实跑道生成");
                samples
            }
            None => {
                log("[track] 该场地无描线，按标准 400m 体育场形生成");
                crate::track::generator::synthetic_track_points(center, 400.0, params.seed)
            }
        }
    };
    if pts_gcj.len() < 3 {
        return Err("有效点位不足三个，停止生成轨迹，请核对坐标与点位响应".into());
    }
    // 必经点保持策略顺序置于前端（waypoints[0] 即起点），剩余打卡点去重后按质心角
    // 排序，使环序自然且不破坏必经点顺序。
    let mut route_pts: Vec<(f64, f64)> = pol.must_points.clone();
    let mut free: Vec<(f64, f64)> = Vec::new();
    for p in &pts_gcj {
        if !route_pts
            .iter()
            .any(|q| (q.0 - p.0).abs() < 1e-6 && (q.1 - p.1).abs() < 1e-6)
        {
            free.push(*p);
        }
    }
    if !free.is_empty() {
        route_pts.extend(crate::track::generate_road::radial_order(&free));
    }
    if !pol.must_points.is_empty() {
        log(&format!(
            "√ [policy] 必经点 {} 个（命中字段 {:?}，保持顺序），合并后路线 waypoint 共 {} 个",
            pol.must_points.len(),
            pol.must_points_key.unwrap_or("未知"),
            route_pts.len()
        ));
        // 防御性日志：must_points 的「首个=起点」语义未经证实（提交时 skip(1) 依赖此假设），
        // 逐点打印便于抓真实响应核对，避免首个必经点被静默丢弃。
        for (i, &(mlat, mlon)) in pol.must_points.iter().enumerate() {
            log(&format!(
                "  [policy] must_points[{i}] GCJ=({mlat:.6},{mlon:.6}){}",
                if i == 0 { "（假设为起点）" } else { "（必经点）" }
            ));
        }
    } else {
        log(&format!(
            "[policy] 响应未含必经点列表，仅用{} {} 个",
            if scored { "打卡点" } else { "合成轨迹控制点" },
            route_pts.len()
        ));
    }
    if let Some(&(slat, slon)) = route_pts.first() {
        log(&format!("√ [track] 起点 GCJ=({slat:.6},{slon:.6})"));
    }
    // 保持用户配置的锚点不变；读取点位不应隐式改写设备位置。
    // 平均配速须落在有效窗口内（否则逐点速度无法全窗内），越界时修正时长。
    let mut params = params.clone();
    if params.dur <= 0 || params.dist <= 0.0 {
        return Err(format!(
            "距离/时长无效：dist={} dur={}",
            params.dist, params.dur
        ));
    }
    let avg = params.dist / params.dur as f64;
    let fixed_avg = avg.clamp(
        crate::track::generator::SPEED_FLOOR + 0.1,
        crate::track::generator::SPEED_CEIL - 0.1,
    );
    if (fixed_avg - avg).abs() > 1e-6 {
        let fixed_dur = (params.dist / fixed_avg).round() as i64;
        log(&format!(
            "[track] 平均配速 {} m/s 超出有效窗口，时长 {} -> {}s",
            (avg * 100.0).round() / 100.0,
            params.dur,
            fixed_dur
        ));
        params.dur = fixed_dur;
    }
    // 围栏并不提供跑道中心线；不将围栏外形当成跑道。
    let fence_for_stadium: Option<&[(f64, f64)]> = None;
    if params.route_mode == RouteMode::Legacy {
        match crate::track::reference::matching_ring(&pts_gcj) {
            Some(reference) => log(&format!(
                "[track] 已匹配卫星描线 {}；打卡点用于位置核对，不再推断整场长宽",
                reference.id
            )),
            None if scored => {
                log("⚠ [track] 当前场地暂无跑道描线，按打卡点估计形状；估计形状不代表真实跑道")
            }
            None => log("[track] 户外跑无点位：按合成跑道环生成（标准 400m 体育场形）"),
        }
    }

    // 时序自检：真人跑完会在很短时间内上传，stopTime 与实际上传时刻的间隔
    // 是可持续观测的自动化特征。间隔过大只提示、不阻断（用户可能确实在补录）。
    {
        let dur_s = params.dur;
        let stop_ms = start_ms_for_check(&params) + dur_s * 1000;
        let lag_s = (crate::crypto::envelope::now_ms() - stop_ms) / 1000;
        if lag_s > 900 {
            log(&format!(
                "⚠ [track] 记录结束时刻比当前早 {} 分钟，与真机「跑完即上传」有明显差异；\
                 可用 --ago 0 让结束时刻贴近当前",
                lag_s / 60
            ));
        } else if lag_s < -60 {
            log(&format!(
                "⚠ [track] 记录结束时刻在 {} 秒后（未来时间），服务端可能判定异常",
                -lag_s
            ));
        }
    }

    log(&format!(
        "[track] 生成轨迹 {:.0}m / {}s（{} 点位）…",
        params.dist,
        params.dur,
        route_pts.len()
    ));
    // 随机 0-4 秒偏移（终端上报的 flag 与首点差 <5s），轨迹/提交/OBS/五点统一使用
    // 注意：i64 取模保留符号，`rand::random::<i64>() % 5` 实际是 -4..=+4 秒，
    // 会让 startTime 早于用户设定的时刻。这里显式取非负 0..=4 秒。
    let mut start_ms = params.start_ms + (rand::random::<u64>() % 5) as i64 * 1000;
    // 有效时间窗来自服务端 `runRuleModel.validTime`（`SWRunValidTime.{start,end}`）。
    // 窗口外的记录会被拒绝（11016），因此这里把整段记录钳进窗口，而不是发出去再失败。
    start_ms = clamp_into_valid_window(start_ms, params.dur, &pol, log)?;
    let mut track = match params.route_mode {
        RouteMode::Road => {
            let cfg = crate::api::model::load_config();
            if cfg.osm_path.is_empty() {
                log("⚠ [track] 未配置 OSM 路网，回退经典算法");
                crate::track::generator::build_with_fence(
                    params.dist,
                    params.dur,
                    params.seed,
                    (anchor.latitude, anchor.longitude),
                    start_ms,
                    &pts_gcj,
                    fence_for_stadium,
                )
            } else {
                match crate::track::generate_road::load_network_path(&cfg.osm_path) {
                    Ok(mut net) => {
                        crate::track::generate_road::align_network(&mut net);
                        // 电子围栏：裁剪到围栏内道路（失败/无围栏则跳过）
                        let fences = match crate::api::fence::fetch_geo_fence(client) {
                            Ok(f) => {
                                let _ = crate::api::model::save_fence_cache(&f);
                                log(&format!("√ [track] 电子围栏 {} 个", f.len()));
                                f
                            }
                            Err(e) => {
                                log(&format!("⚠ [track] 围栏获取失败，回退缓存: {e}"));
                                crate::api::model::load_fence_cache().unwrap_or_default()
                            }
                        };
                        let filtered = crate::track::generate_road::apply_fences(&net, &fences);
                        // 强制必经点（不含起点）：其余打卡点仅软引导 + <40m 吸附
                        let must_bd: Vec<(f64, f64)> =
                            pol.must_points.iter().skip(1).copied().collect();
                        match crate::track::generate_road::build_road(
                            params.dist,
                            params.dur,
                            params.seed,
                            start_ms,
                            &route_pts,
                            &must_bd,
                            &filtered,
                        ) {
                            Ok(t) => {
                                log(&format!(
                                    "√ [track] 真实道路路由 {} 点 / {} 建筑 / {} 围栏",
                                    t.locations.len(),
                                    filtered.buildings.len(),
                                    fences.len()
                                ));
                                t
                            }
                            Err(e) => {
                                log(&format!("⚠ [track] 道路路由失败，回退经典算法: {e}"));
                                gen_track(
                                    params.dist,
                                    params.dur,
                                    params.seed,
                                    (anchor.latitude, anchor.longitude),
                                    start_ms,
                                    &pts_gcj,
                                )
                            }
                        }
                    }
                    Err(e) => {
                        log(&format!("⚠ [track] 路网加载失败，回退经典算法: {e}"));
                        gen_track(
                            params.dist,
                            params.dur,
                            params.seed,
                            (anchor.latitude, anchor.longitude),
                            start_ms,
                            &pts_gcj,
                        )
                    }
                }
            }
        }
        RouteMode::Legacy => crate::track::generator::build_with_fence(
            params.dist,
            params.dur,
            params.seed,
            (anchor.latitude, anchor.longitude),
            start_ms,
            &pts_gcj,
            fence_for_stadium,
        ),
    };
    if let Some(range) = params.manual_altitude_range {
        crate::track::altitude::override_bd_a_range(&mut track, range)?;
        log(&format!(
            "√ [track] 已将海拔曲线映射到 {:.2}-{:.2}m，覆盖 {} 个点，爬升/圈数据将按覆盖值计算",
            range.min_m,
            range.max_m,
            track.locations.len()
        ));
    } else if let Some(altitude_m) = params.manual_altitude {
        crate::track::altitude::override_bd_a(&mut track, altitude_m)?;
        log(&format!(
            "√ [track] 已用手动海拔 {:.2}m 覆盖 {} 个点，爬升/圈数据将按覆盖值计算",
            altitude_m,
            track.locations.len()
        ));
    }
    log(&format!(
        "√ [track] {} 点 totalDis={:.2}m（目标 {:.0}m）steps={} 起点={}",
        track.locations.len(),
        track.totalDistance,
        params.dist,
        track.totalSteps,
        chrono::Local
            .timestamp_millis_opt(track.startTime)
            .single()
            .map(|t| t.format("%H:%M:%S").to_string())
            .unwrap_or_default(),
    ));
    // 距离口径自检分两件事，不能混为一谈：
    //
    // ① **服务端会不会算出别的数**：它用 gLat/gLng 重算折线实长，所以要比的是
    //    「声明的 totalDis」vs「这条折线的实际长度」，不是用户想要的目标距离。
    // ② **有没有跑到用户要的距离**：road 模式受路网限制，误差到 1~2% 是正常的，
    //    这是"没跑够"，不是"协议不一致"。
    let polyline = track.polyline_length_m();
    if (track.totalDistance - polyline).abs() > 0.5 {
        log(&format!(
            "⚠ [track] 声明距离 {:.2}m 与折线实长 {:.2}m 不一致（服务端会按坐标重算）",
            track.totalDistance, polyline
        ));
    }
    if (track.totalDistance - params.dist).abs() > 2.0 {
        log(&format!(
            "[track] 实际生成距离 {:.2}m，与目标 {:.0}m 相差 {:+.1}%（受路网/几何限制，非协议问题）",
            track.totalDistance,
            params.dist,
            (track.totalDistance - params.dist) / params.dist * 100.0
        ));
    }

    // ④ 五点 wrapper（跑完态）
    //
    // `flag` 必须与点位用的是**同一个记录 id**：真机 `_buildFivePointInfo` 与
    // `_buildLocationPathInfo` 都读 `[builder recordID]`，两处必须是同一值。
    // 这里此前误传了 `track.startTime`，会导致五点与点位的 flag 不一致。
    let five = five_point_wrapper(&pts, record_seq);

    // ④-b 本地达标判定。
    //
    // **实机的判据不是轨迹几何，而是五点实体的标志位**（IDA：
    // `-[SWRunGradeReviewRandomImpl reasonForIncompletion]` @0x10014f078）：
    //   · 必经点：`fivePoints.contain(isFixed && isPass)` —— 至少一个
    //   · 打卡点：`fivePoints.select(!isFixed && isPass).count > 1` —— 至少两个
    //   · 距离：`totalDis >= runRule.minDistance`
    //   · 时长：`minRunTime == 0 || minRunTime <= totalTime`
    // 我们发出的五点实体统一 `isPass = true`，所以真正的变量是 `isFixed`。
    let flag = |p: &Value, key: &str| -> Option<bool> {
        p.get(key).and_then(|v| {
            v.as_bool()
                .or_else(|| v.as_i64().map(|n| n != 0))
                .or_else(|| v.as_f64().map(|n| n != 0.0))
        })
    };
    // 非顺序跑没有点位响应，`isFixed` 判定无从谈起，直接按「无点位要求」通过。
    let has_fixed_field = !scored || pts.iter().any(|p| p.get("isFixed").is_some());
    let fixed_count = pts
        .iter()
        .filter(|p| flag(p, "isFixed").unwrap_or(false))
        .count();
    let free_count = pts
        .iter()
        .filter(|p| !flag(p, "isFixed").unwrap_or(false))
        .count();
    // 响应若完全没有 isFixed 字段，就无法判断必经点，不能凭空判为未通过。
    let must_ok = !has_fixed_field || fixed_count > 0;
    let normal_ok = free_count > 1;
    if !has_fixed_field && scored {
        log("⚠ [grade] 点位响应未提供 isFixed 字段，跳过必经点判定");
    }

    // 位置自检（硬闸门）：轨迹必须真的落在服务端点位所在的那块场地上。
    //
    // 这一条是为「整条轨迹平移 900m」那类事故加的：坐标系用错时，声明、步频、配速
    // 全部自洽，只有位置是错的，服务端只能靠坐标发现，等它判异常已经晚了。
    {
        let n = pts_gcj.len().max(1) as f64;
        let (plat, plng) = (
            pts_gcj.iter().map(|p| p.0).sum::<f64>() / n,
            pts_gcj.iter().map(|p| p.1).sum::<f64>() / n,
        );
        let tn = track.locations.len().max(1) as f64;
        let (tlat, tlng) = (
            track.locations.iter().map(|p| p.gLat).sum::<f64>() / tn,
            track.locations.iter().map(|p| p.gLng).sum::<f64>() / tn,
        );
        let centroid_gap = crate::track::geom::local_distance_m(tlat, tlng, plat, plng);
        let worst = pts_gcj
            .iter()
            .map(|target| {
                track
                    .locations
                    .iter()
                    .map(|q| {
                        crate::track::geom::local_distance_m(q.gLat, q.gLng, target.0, target.1)
                    })
                    .fold(f64::INFINITY, f64::min)
            })
            .fold(0.0_f64, f64::max);
        log(&format!(
            "[track] 位置自检：轨迹中心与点位中心相距 {:.1}m，最远打卡点距轨迹 {:.1}m",
            centroid_gap, worst
        ));
        // 非顺序跑的对照物是自己合成的控制点，这里比较没有意义，只做记录。
        if scored && (centroid_gap > 200.0 || worst > 150.0) {
            return Err(format!(
                "轨迹不在点位所在场地：轨迹中心偏离点位中心 {centroid_gap:.0}m，最远打卡点离轨迹 {worst:.0}m。\n                 已停止，未产生记录（检查坐标体系：上传体 gLat/gLng 必须是 GCJ）"
            ));
        }
    }

    // 几何复核只作为告警：服务端会用坐标独立重算，几何真的没经过就必须让用户看到。
    {
        let missed: Vec<String> = pts_gcj
            .iter()
            .filter(|target| {
                !track.locations.iter().any(|q| {
                    crate::track::geom::local_distance_m(q.gLat, q.gLng, target.0, target.1) <= 40.0
                })
            })
            .map(|(la, lo)| format!("({la:.6},{lo:.6})"))
            .collect();
        if !missed.is_empty() {
            log(&format!(
                "⚠ [track] 下列打卡点 40m 内没有轨迹点，服务端按坐标复核可能判未通过：{}",
                missed.join(" ")
            ));
        }
    }

    if params.dry_run {
        log(&format!(
            "√ [dry-run] 轨迹 {} 点 / totalDis={:.2}m / 时长={}s / 起点={} / 配速={:.0}s/km",
            track.locations.len(),
            track.totalDistance,
            track.totalTime,
            chrono::Local
                .timestamp_millis_opt(track.startTime)
                .single()
                .map(|t| t.format("%Y-%m-%d %H:%M:%S").to_string())
                .unwrap_or_default(),
            track.totalTime as f64 / (track.totalDistance / 1000.0).max(1e-6),
        ));
        log(&format!(
            "√ [dry-run] 首点 GCJ=({:.6},{:.6}) 末点 GCJ=({:.6},{:.6})",
            track.locations.first().map(|p| p.gLat).unwrap_or(0.0),
            track.locations.first().map(|p| p.gLng).unwrap_or(0.0),
            track.locations.last().map(|p| p.gLat).unwrap_or(0.0),
            track.locations.last().map(|p| p.gLng).unwrap_or(0.0),
        ));
        if let Some(path) = params.out_path.as_deref() {
            let dump = serde_json::json!({
                "crs": "gcj02",
                "runMode": params.run_mode,
                "track": track.locations.iter().map(|p| [p.gLat, p.gLng]).collect::<Vec<_>>(),
                "checkpoints": pts_gcj,
                "record": {
                    "totalDistance": track.totalDistance,
                    "totalTime": track.totalTime,
                    "totalSteps": track.totalSteps,
                    "startTime": track.startTime,
                },
            });
            match std::fs::write(path, serde_json::to_string(&dump).unwrap_or_default()) {
                Ok(()) => log(&format!("[dry-run] 轨迹已写入 {path}")),
                Err(e) => log(&format!("⚠ [dry-run] 写轨迹失败: {e}")),
            }
        }
        log("[dry-run] 未提交任何数据");
        return Ok(RunOutcome {
            result: SubmitResult {
                rrid: 0,
                uuid: "DRY-RUN".into(),
                start_ms: track.startTime,
                total_dis: track.totalDistance,
                total_time: track.totalTime,
                total_steps: track.totalSteps,
                avg_step_freq: 0,
                calorie: 0,
                avg_power: 0,
                sel_distance: pol.min_distance,
            },
            obs_ok: 0,
            detail_ok: false,
        });
    }

    let review = crate::track::grade::review(&crate::track::grade::GradeInput {
        total_time: track.totalTime,
        total_dis: track.totalDistance,
        min_run_time: pol.min_run_time,
        min_distance: pol.min_distance,
        must_points_ok: must_ok,
        normal_points_ok: normal_ok,
    });
    // ④-c 服务器对时（真机提交前必做）。
    //
    // `-[SWCHttpTool isVaildTimeRange:]` @0x10028323c → `uploadGrade` 的完成块
    // `sub_100047DA8`：|服务器秒 − 本机秒| > 300 时直接以 1090090909 终止上传。
    // 本机时钟偏得多的话跑一次也是白跑，所以这里也硬停。
    match super::client::fetch_server_time_secs(client) {
        Ok(server_secs) => {
            let local_secs = crate::crypto::envelope::now_ms() / 1000;
            let skew = (server_secs - local_secs).abs();
            if skew > super::client::MAX_CLOCK_SKEW_SECS {
                return Err(format!(
                    "本机时钟与服务器相差 {skew} 秒（上限 {} 秒），真机会拒绝上传。\
                     请先校准系统时间后重试。已停止，未产生记录。",
                    super::client::MAX_CLOCK_SKEW_SECS
                ));
            }
            log(&format!("√ [clock] 与服务器时间相差 {skew} 秒"));
        }
        Err(e) => {
            // 拿不到时间不阻断（真机会失败，但本工具不该因此完全不可用）。
            log(&format!("⚠ [clock] 服务器时间获取失败（{e}），跳过对时检查"));
        }
    }

    // ⑤ 提交
    //
    // `record.sportType` 与 `record.policy` 在真机里**同源**（`sub_1000D2D14`）：
    //     [record setSportType:[runCtl runType]];
    //     [record setPolicy:      <同一个值>];
    // 而这个值由 runMode 查表得到（`indoor::sport_type_for` / `indoor::policy_for`，
    // 表在 `qword_1045C6388` / `dword_1045C6368`）：
    //     runMode 1（校园跑）→ sportType 1 / policy 1
    //     runMode 6（户外跑）→ sportType 6 / policy 0
    //     runMode 4（自由跑）→ sportType 4 / policy 0
    let sport_type = crate::api::indoor::sport_type_for(params.run_mode);
    let record_policy = crate::api::indoor::policy_for(params.run_mode);
    log(&format!(
        "[record] 提交跑步记录（runMode={} → sportType={} policy={}）…",
        params.run_mode, sport_type, record_policy
    ));
    let sp = SubmitParams {
        track,
        uid: sess.uid,
        selected_unid: sess.unid.parse().unwrap_or(0),
        policy: record_policy,
        sport_type,
        min_distance: pol.min_distance,
        // calorie 用**原始** userWeight（缺失即 0，真机也就是 0）；
        // avgPower 用 `userWeightForCalorie`（缺失按性别回退 67/54）。
        weight: sess.weight,
        power_weight: {
            let w = sess.weight_for_power();
            if sess.weight <= 0.0 {
                log(&format!(
                    "⚠ [submit] 登录未提供体重：calorie 按 0 计（真机同此），\
                     avgPower 按性别回退 {w}kg"
                ));
            }
            w
        },
        // 走到这里说明 `policy.faceVerify == false`（为 true 时本函数已提前返回），
        // 实机在该情形下不产生 faceCheckTypes → `buildGradeWithCompletion:` 发 faceCheck=0。
        face_check: 0,
        five_point_json: five,
        address: client.identity.city.clone(),
        // iOS selRunTime 取策略的 minRunTime（IPA buildGradeWithCompletion）
        min_run_time: pol.min_run_time,
        complete: review.complete(),
        uncomplete_reason: review.reason,
        record_id: record_seq,
        policy_timestamp: pol.timestamp,
    };
    let result = submit_record(client, &sp, log)?;
    sleep_jitter(600, 1_800);

    // ⑥ OBS 上传（真机单 key：run_data/{yyyyMMddHH}/{uuid}.json）
    log("[obs] 上传 OBS 对象（gzip+base64，11 键）…");
    // 从提交结果回填 track.startTime（含随机秒偏移），保证 body/OBS/flag 全链一致
    let mut track_for_obs = sp.track.clone();
    track_for_obs.startTime = result.start_ms;
    let obj = build_obs_object(&track_for_obs, result.rrid, &result.uuid, sess.uid, &pts, record_seq);
    let payload = obj.to_string().into_bytes();
    let keys = obs_keys(&track_for_obs, result.rrid, &result.uuid);
    let obs_ok = super::obs::upload_keys(client, &keys, &payload, log);
    if obs_ok == keys.len() {
        log(&format!("√ [obs] 上传成功 key={}", keys.join(",")));
    } else {
        log(&format!(
            "⚠ [obs] 上传成功 {obs_ok}/{}（记录已入库，但轨迹明细缺失）",
            keys.len()
        ));
    }

    // ⑦ 详情验证
    sleep_jitter(1_100, 3_200);
    log("[verify] 拉取详情验证…");
    let detail_ok = match fetch_one_record(client, result.rrid, &result.uuid) {
        Ok(d) => {
            log(&format!(
                "√ [verify] rrid={} complete={:?} dis={:?} time={:?}",
                result.rrid,
                d.get("complete").and_then(|v| v.as_bool()),
                d.get("totalDis"),
                d.get("totalTime"),
            ));
            if let Ok(mut slot) = VERIFY_DETAIL.lock() {
                slot.replace(d.clone());
            }
            true
        }
        Err(e) => {
            log(&format!(
                "⚠ [verify] 详情拉取失败（提交已成功 rrid={}）：{e}",
                result.rrid
            ));
            false
        }
    };
    Ok(RunOutcome {
        result,
        obs_ok,
        detail_ok,
    })
}

/// 室内跑参数。
pub struct IndoorRunParams {
    pub steps: i64,
    pub dur: i64,
    pub start_ms: i64,
    pub seed: u64,
}

pub struct IndoorOutcome {
    pub total_steps: i64,
    pub total_time: i64,
    pub avg_step_freq: i64,
    pub complete: bool,
    pub uncomplete_reason: i64,
    pub body: serde_json::Value,
}

/// 室内跑完整流程：策略（runMode=5）→ 步数生成 → body → 提交。
///
/// 与室外不同：
/// - **没有** `isVaildTimeRange:` 对时闸门（`uploadIndoorGrade` 里不存在该调用）；
/// - 没有打卡点/围栏/轨迹；
/// - 端点 `/api/v68/indoor/save/record`；
/// - OBS 用 `moudle:1` + `name=rrid`（整数分支），但**模块 1 的键前缀未能静态定位**，
///   因此本流程只构造对象、不擅自上传到猜测的键上（写错键比不写更糟）。
pub fn run_indoor_flow(
    client: &mut ApiClient,
    params: &IndoorRunParams,
    log: &mut dyn FnMut(&str),
) -> Result<IndoorOutcome, String> {
    let sess: Session = client.login.clone().ok_or("未登录")?;
    if params.steps <= 0 {
        return Err("室内步数必须为正".into());
    }
    let record_seq = {
        let mut id = client.identity.clone();
        id.record_seq = id.record_seq.saturating_add(1);
        if let Err(e) = crate::api::model::save_identity(&id) {
            log(&format!("⚠ [indoor] 记录序号落盘失败（不影响本次提交）: {e}"));
        }
        client.identity.record_seq = id.record_seq;
        id.record_seq
    };

    log("[indoor] 拉取室内策略（runMode=5）…");
    let pol = super::policy::fetch_indoor_policy(client, &sess.unid)?;
    if !pol.defaults_used.is_empty() {
        log(&format!(
            "⚠ [indoor] 服务端未下发以下字段，已用兜底值（会写进提交体）：{}",
            pol.defaults_used.join("、")
        ));
    }
    log(&format!(
        "√ [indoor] 策略 minSteps={} minRunTime={} maxRunTime={}",
        pol.min_steps, pol.min_run_time, pol.max_run_time
    ));

    let track = crate::track::indoor::generate(params.steps, params.dur, params.seed)?;
    log(&format!(
        "√ [indoor] {} 步 / {}s（步频 {} 步/分，{} 个 10 秒窗口）",
        track.total_steps,
        track.total_time,
        track.avg_step_freq(),
        track.windows.len()
    ));

    let anchor = client.identity.anchor_coordinate()?;
    let uuid = uuid::Uuid::new_v4().to_string().to_uppercase();
    let input = super::indoor::IndoorBodyInput {
        uuid: uuid.clone(),
        selected_unid: sess.unid.clone(),
        // 真机 `SWIndoorSportRecord.policy`：室内自由跑为 0（表映射后 policy=0 / sportType=1）
        record_policy: 0,
        start_ms: params.start_ms,
        stop_ms: params.start_ms + params.dur * 1000,
        total_time: track.total_time,
        total_steps: track.total_steps,
        latitude: anchor.latitude,
        longitude: anchor.longitude,
        min_steps: pol.min_steps,
        min_run_time: pol.min_run_time,
        max_run_time: pol.max_run_time,
    };
    let body = super::indoor::build_indoor_body(&input, crate::crypto::sign::SALT);
    let body_plain = serde_json::Value::Object(body.clone()).to_string();
    log(&format!(
        "[indoor] 提交 body {} 键（uuid={} record_seq={}）",
        body.len(),
        uuid,
        record_seq
    ));
    let resp = super::indoor::submit_indoor_record(client, &body_plain, log)?;
    log(&format!("√ [indoor] 提交完成: {}", resp["business"]["error"]));

    // OBS：对象已构造，但模块 1 的键前缀未定位，不擅自上传。
    let _obs = track.build_obs_object(&uuid, sess.uid, params.start_ms);
    log("⚠ [indoor] OBS 对象（3 键）已构造，但 moudle=1 的键前缀未静态定位，暂不上传");

    let reason = super::indoor::uncomplete_reason(
        track.total_time,
        track.total_steps,
        pol.min_run_time,
        pol.min_steps,
    );
    Ok(IndoorOutcome {
        total_steps: track.total_steps,
        total_time: track.total_time,
        avg_step_freq: track.avg_step_freq(),
        complete: reason == 0,
        uncomplete_reason: reason,
        body: serde_json::Value::Object(body),
    })
}

/// AI 提交流（UI 线程用）。
pub fn run_ai_submit(
    client: &mut ApiClient,
    sport_id: i64,
    mode: super::ai::AiMode,
    log: &mut dyn FnMut(&str),
) -> Result<Value, String> {
    log(&format!("[ai] 提交 sportId={sport_id} mode={mode:?}…"));
    let biz = super::ai::upload(client, sport_id, mode, None)?;
    log("√ [ai] 提交成功");
    Ok(biz)
}

/// AI 列表（UI 线程用）。
pub fn run_ai_list(
    client: &mut ApiClient,
    log: &mut dyn FnMut(&str),
) -> Result<Vec<super::ai::AiSport>, String> {
    log("[ai] 拉取项目列表…");
    let list = super::ai::fetch_list(client)?;
    log(&format!("√ [ai] {} 个项目", list.len()));
    Ok(list)
}

/// 记录列表（UI 线程用）。
pub fn run_records(
    client: &mut ApiClient,
    log: &mut dyn FnMut(&str),
) -> Result<Vec<super::records::RecordRow>, String> {
    log("[records] 拉取跑步记录…");
    let rows = super::records::fetch_records(client)?;
    log(&format!("√ [records] {} 条记录", rows.len()));
    Ok(rows)
}

use chrono::TimeZone as _;

/// 最近一次详情验证的完整响应（达标判定明细在 reasonList）。
pub static VERIFY_DETAIL: std::sync::Mutex<Option<Value>> = std::sync::Mutex::new(None);

#[cfg(test)]
mod window_tests {
    use super::*;
    use chrono::{Local, TimeZone};

    fn policy(start: &str, end: &str) -> crate::api::policy::PolicyInfo {
        crate::api::policy::PolicyInfo {
            timestamp: 0,
            policy: 0,
            min_distance: 1000,
            min_run_time: 300,
            face_verify: false,
            valid_start: Some(start.into()),
            valid_end: Some(end.into()),
            speed_bottom: None,
            speed_top: None,
            step_bottom: None,
            step_top: None,
            must_points: Vec::new(),
            must_points_key: None,
            defaults_used: Vec::new(),
        }
    }

    fn local_ms(h: u32, m: u32) -> i64 {
        let day = Local::now().date_naive();
        Local
            .from_local_datetime(&day.and_hms_opt(h, m, 0).unwrap())
            .single()
            .unwrap()
            .timestamp_millis()
    }

    fn noop(_: &str) {}

    #[test]
    fn inside_window_is_untouched() {
        let p = policy("06:30", "22:00");
        let t = local_ms(12, 0);
        assert_eq!(clamp_into_valid_window(t, 540, &p, &mut noop).unwrap(), t);
    }

    #[test]
    fn outside_window_is_moved_to_window_start() {
        let p = policy("06:30", "22:00");
        let t = local_ms(23, 30);
        let got = clamp_into_valid_window(t, 540, &p, &mut noop).unwrap();
        let sod = (got - local_ms(0, 0)).div_euclid(60_000);
        assert_eq!(sod, 6 * 60 + 30, "应被平移到窗口起点 06:30");
    }

    #[test]
    fn run_longer_than_window_is_rejected() {
        let p = policy("06:30", "22:00"); // 930 分钟
        let err = clamp_into_valid_window(local_ms(12, 0), 931 * 60, &p, &mut noop).unwrap_err();
        assert!(err.contains("超过服务端有效时间窗"), "{err}");
    }

    #[test]
    fn missing_or_unparsable_window_is_skipped() {
        let t = local_ms(23, 30);
        let mut p = policy("06:30", "22:00");
        p.valid_start = None;
        assert_eq!(clamp_into_valid_window(t, 540, &p, &mut noop).unwrap(), t);
        let mut p = policy("bad", "22:00");
        p.valid_start = Some("bad".into());
        assert_eq!(clamp_into_valid_window(t, 540, &p, &mut noop).unwrap(), t);
    }
}
