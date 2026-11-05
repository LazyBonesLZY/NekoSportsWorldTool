//! 自然轨迹生成器。
//!
//! 画像驱动：打卡点拟合闭合环（Catmull-Rom，每段 18 采样）→ 弧长表；
//! 采样间隔主 5s（80%）；速度曲线 = ramp × 疲劳 × 三正弦 × 余弦凹陷 × 微噪；
//! 正常点按速度曲线分配位移并归一到精确总距离；异常点（-1）零位移/跳变；
//! 哨兵点（首 type∈{0,7}、索引1 type=5、末 type=6）；结尾断崖；点位吸附。
#![allow(non_snake_case)]

use super::geom::{
    fmt_gain_time, m_per_deg_lng, make_stadium_ring, ring_point_at, round_to, to_bd, Rng,
    MET_PER_DEG_LAT,
};
use super::model::{GenPoint, Segment, TenWindow, Track};
use super::postfix::apply_post_fixes;

/// 有效配速窗口（判定规则 2'21"-10'00"/km ≈ 1.667-7.092 m/s），硬边界留余量。
pub const SPEED_FLOOR: f64 = 1.90;
pub const SPEED_CEIL: f64 = 6.30;

/// 将每段恒定速度与步频按真实时间边界拆到连续的十秒窗。
#[derive(Default)]
pub(super) struct TenWindowAccumulator {
    time: f64,
    distance: f64,
    steps: f64,
    speeds: Vec<TenWindow>,
    step_counts: Vec<TenWindow>,
}

impl TenWindowAccumulator {
    pub(super) fn add(&mut self, mut dt: f64, speed: f64, steps_per_second: f64) {
        while dt > 0.0 {
            let part = dt.min(10.0 - self.time);
            self.time += part;
            self.distance += speed * part;
            self.steps += steps_per_second * part;
            dt -= part;
            if self.time >= 10.0 {
                self.flush();
            }
        }
    }

    fn flush(&mut self) {
        // 生成器的时间网格以整秒为单位，尾窗保留实际秒数和累计量。
        let time = round_to(self.time, 0) as i64;
        self.speeds.push(TenWindow { time, value: round_to(self.distance, 2) });
        self.step_counts.push(TenWindow { time, value: round_to(self.steps, 0) });
        self.time = 0.0;
        self.distance = 0.0;
        self.steps = 0.0;
    }

    pub(super) fn finish(mut self) -> (Vec<TenWindow>, Vec<TenWindow>) {
        if self.time > 0.0 {
            self.flush();
        }
        (self.speeds, self.step_counts)
    }
}

/// 等比缩放逐点速度至目标总距：越界点钳在窗口边界，剩余差量由未饱和点分摊（迭代收敛）。
/// 与整体等比缩放的区别：任何一点的瞬时配速都不会越出有效窗口。
fn fit_speeds(w: &mut [f64], dts: &[f64], target: f64) {
    for _ in 0..24 {
        let cur: f64 = w.iter().zip(dts).map(|(x, dt)| x * dt).sum();
        if (cur - target).abs() <= 1.0 {
            break;
        }
        let k = target / cur;
        for x in w.iter_mut() {
            *x = (*x * k).clamp(SPEED_FLOOR, SPEED_CEIL);
        }
    }
    for x in w.iter_mut() {
        *x = x.clamp(SPEED_FLOOR, SPEED_CEIL);
    }
}

/// 轨迹生成主入口。points_bd 为 BD 系打卡点。
/// 漂移幅度上限：环短轴半径 60%，封顶 60m。
fn drift_cap(dense: &[(f64, f64)]) -> f64 {
    60.0f64.min(crate::track::geom::short_axis_radius(dense) * 0.6).max(10.0)
}

pub fn build(
    dist: f64,
    dur: i64,
    seed: u64,
    _center: (f64, f64),
    start_ms: i64,
    points_bd: &[(f64, f64)],
) -> Track {
    build_with_fence(dist, dur, seed, _center, start_ms, points_bd, None)
}

/// 带电子围栏的构建：围栏用于约束体育场长轴（跑道被围栏包裹）。
pub fn build_with_fence(
    dist: f64,
    dur: i64,
    seed: u64,
    _center: (f64, f64),
    start_ms: i64,
    points_bd: &[(f64, f64)],
    fence_bd: Option<&[(f64, f64)]>,
) -> Track {
    let mut rng = Rng::new(seed);
    let dur_f = dur as f64;
    // 体育场形跑道：PCA 拟合打卡点（打卡点沿真实跑道分布），
    // 轨迹沿跑道绕圈、从打卡点旁自然经过（≤40m），不再是穿心环
    let (dense, arcs, pc) = make_stadium_ring(points_bd, &mut rng, fence_bd);
    let (c_lat, c_lng) = pc;
    let direction: f64 = rng.choice(&[1.0, -1.0]);
    let s0 = rng.uniform(0.0, *arcs.last().unwrap_or(&400.0));
    let phase_v = rng.uniform(0.0, std::f64::consts::TAU);
    let phase_l = rng.uniform(0.0, std::f64::consts::TAU);

    let mut times = Vec::new();
    let mut t = 0.0;
    while t < dur_f {
        times.push(t);
        t += if rng.random() < 0.80 {
            5.0
        } else {
            rng.choice(&[1.0, 2.0, 3.0, 4.0, 6.0, 7.0, 8.0])
        };
    }
    let n = times.len();

    let n_dips = rng.choice(&[1, 1, 2]);
    let mut dips = Vec::new();
    for _ in 0..n_dips {
        dips.push((
            rng.uniform(0.15, 0.75) * dur_f,
            rng.uniform(25.0, 55.0),
            rng.uniform(0.08, 0.16),
        ));
    }
    let dip_factor = |t: f64| -> f64 {
        let mut f = 1.0;
        for &(c, hw, d) in &dips {
            if (t - c).abs() < hw {
                f *= 1.0 - d * 0.5 * (1.0 + std::f64::consts::PI * (t - c) / hw).cos();
            }
        }
        f
    };
    let base = dist / dur_f;
    let mut w = Vec::with_capacity(n);
    for &tt in &times {
        let ramp = if tt < 8.0 {
            (0.85 + 0.15 * (tt / 1.0f64.max(8.0f64.min(dur_f / 20.0)))).min(1.0)
        } else {
            1.0
        } * if tt > dur_f - 8.0 {
            1.0 + 0.03 * (tt - (dur_f - 8.0)) / 8.0
        } else {
            1.0
        };
        let km_done = (tt / dur_f) * dist / 1000.0;
        let fatigue = if km_done <= 0.5 { 1.03 } else { (1.03 - 0.06 * (km_done - 0.5)).max(0.80) };
        let wave = (1.0 + 0.010 * (std::f64::consts::TAU * tt / 115.0 + phase_v).sin()
            + 0.035 * (std::f64::consts::TAU * tt / 47.0 + phase_v * 2.3).sin()
            + 0.015 * (std::f64::consts::TAU * tt / 19.0 + phase_v * 3.7).sin())
            * dip_factor(tt);
        let noise = 1.0 + rng.gauss(0.0, 0.008);
        w.push(base * ramp * fatigue * wave * noise);
    }
    let mut dts: Vec<f64> = (0..n - 1).map(|i| times[i + 1] - times[i]).collect();
    dts.push(1.0f64.max(dur_f - times[n - 1]));
    // 逐点速度全部约束在有效配速窗口内，并精确命中目标距离
    fit_speeds(&mut w, &dts, dist);
    let seg_dist: Vec<f64> = (0..n).map(|i| w[i] * dts[i]).collect();
    let speeds: Vec<f64> = w.clone();

    let tl_w = [((-1i64, 4i64), 54u32), ((-1, 1), 27), ((-1, 12), 13), ((-1, 5), 3), ((-1, 6), 2)];
    let mut kinds: Vec<(i64, i64)> = Vec::with_capacity(n);
    for _ in 0..n {
        let u = rng.random();
        if u < 0.39 {
            kinds.push((3, 1));
        } else if u < 0.93 {
            kinds.push((0, 1));
        } else if u < 0.96 {
            kinds.push(rng.weighted(&tl_w));
        } else {
            kinds.push((rng.choice(&[1, 1, 1, 1, 2, 2]), 1));
        }
    }
    for i in 1..kinds.len() {
        let prev_drift = (-1 == kinds[i - 1].0) || kinds[i - 1].0 == 5 || kinds[i - 1].0 == 6;
        if kinds[i].0 != -1 && prev_drift && rng.random() < 0.08 {
            kinds[i] = (if rng.random() < 0.75 { 7 } else { 8 }, 1);
        }
    }
    for i in 1..n {
        if kinds[i].0 == -1 && kinds[i - 1].0 == -1 {
            kinds[i] = (rng.choice(&[3, 0]), 1);
        }
    }
    let normal_idx: Vec<usize> =
        (0..n).filter(|&i| kinds[i].0 != -1 && i > 1).collect();
    let share: f64 = normal_idx.iter().map(|&i| seg_dist[i]).sum();
    let share = if share == 0.0 { 1.0 } else { share };
    let mut disp_of = vec![0.0f64; n];
    for &i in &normal_idx {
        disp_of[i] = seg_dist[i] * (dist / share);
    }

    let mut locs: Vec<GenPoint> = Vec::with_capacity(n);
    let mut s = s0;
    let mut t_acc = 0.0f64;
    let mut dist_acc = 0.0f64;
    let mut steps_acc = 0.0f64;
    let mut jx = 0.0f64;
    let mut jy = 0.0f64;
    let mut alt = 82.0 + rng.uniform(-1.0, 1.0);
    let n_est = 1.max((dur_f / 5.0) as i64);
    let alt_sigma = rng.uniform(3.8, 6.2) / (0.40 * n_est as f64);
    let mut ten_windows = TenWindowAccumulator::default();

    // 人性化参数（每跑随机）：切弯深度（圈次递增）、直道疲劳外漂强度
    let curve_cut0 = rng.uniform(0.5, 2.5);
    let drift_strength = rng.uniform(0.3, 1.8);
    // 环质心（平面系）用于判断弯道/直道
    let (ring_cx, ring_cy) = dense
        .iter()
        .fold((0.0f64, 0.0f64), |a, q| (a.0 + q.0, a.1 + q.1));
    let ring_cx = ring_cx / dense.len().max(1) as f64;
    let ring_cy = ring_cy / dense.len().max(1) as f64;
    let laps = dist / arcs.last().unwrap_or(&1.0).max(1.0);

    for i in 0..n {
        let dt = dts[i];
        let (typ, lt) = kinds[i];
        t_acc += dt;
        let pos = |ss: f64| ring_point_at(&dense, &arcs, ss);
        let mut d_step = 0.0f64;
        let px;
        let py;
        let x;
        let y;
        let rad;
        let state;
        if typ != -1 {
            d_step = disp_of[i];
            s += direction * d_step;
            let (bx, by) = pos(s);
            x = bx;
            y = by;
            // 人性化：环上点的径向调整（相对环质心）
            let prog = t_acc / dur_f;
            let lap_now = (prog * laps).floor();
            let radx = bx - ring_cx;
            let rady = by - ring_cy;
            let rlen = radx.hypot(rady).max(1e-6);
            let mut radial = 0.0f64;
            // 切弯：弯道段（距质心较远处即弯道顶点附近）向内压，深度随圈次递增
            let curve_zone = rlen > (arcs.last().unwrap_or(&1.0) / std::f64::consts::TAU) * 0.55;
            if curve_zone {
                radial -= curve_cut0 * (1.0 + 0.35 * lap_now) * rng.uniform(0.6, 1.0);
            } else if prog > 0.35 && rng.random() < 0.30 {
                // 直道疲劳：后程向外漂
                radial += drift_strength * prog * rng.uniform(0.4, 1.0);
            }
            let k = ((rlen + radial) / rlen).clamp(0.65, 1.35);
            let hx = ring_cx + radx * k;
            let hy = ring_cy + rady * k;
            jx = 0.72 * jx + rng.gauss(0.0, 0.75);
            jy = 0.72 * jy + rng.gauss(0.0, 0.75);
            px = hx + jx;
            py = hy + jy;
            rad = round_to(
                if typ == 3 { rng.uniform(1.4, 5.1) } else { rng.uniform(1.4, 2.4) },
                2,
            );
            state = if typ == 0 {
                rng.weighted(&[(1, 145), (2, 45), (3, 164)])
            } else {
                rng.weighted(&[(1, 145), (2, 256), (3, 151)])
            };
        } else {
            match lt {
                // 所有漂移幅度统一受限：不超过环短轴半径 60%（体育场小环防跳出围栏）
                4 => {
                    let cap = drift_cap(&dense);
                    if rng.random() >= 0.68 {
                        d_step = if rng.random() < 0.95 { rng.uniform(2.0, 60.0).min(cap) } else { rng.uniform(60.0, 250.0).min(cap) };
                    }
                }
                1 => {
                    if rng.random() >= 0.83 {
                        d_step = rng.uniform(0.5, 36.0).min(drift_cap(&dense));
                    }
                }
                12 => {
                    let cap = drift_cap(&dense);
                    d_step = if rng.random() < 0.9 { rng.uniform(5.0, 80.0).min(cap) } else { rng.uniform(80.0, 220.0).min(cap) };
                }
                5 => d_step = rng.uniform(5.0, 60.0).min(drift_cap(&dense)),
                _ => {
                    let cap = drift_cap(&dense);
                    d_step = rng.uniform((cap * 0.3).max(20.0), cap.max(25.0));
                }
            }
            let (bx, by) = pos(s);
            x = bx;
            y = by;
            if d_step > 0.0 {
                let ang = rng.uniform(0.0, std::f64::consts::TAU);
                px = x + d_step * ang.sin();
                py = y + d_step * ang.cos();
            } else if let Some(last) = locs.last() {
                // 零位移：精确复制上一点坐标（BD→平面）
                py = (last.gLat - c_lat) * MET_PER_DEG_LAT;
                px = (last.gLng - c_lng) * m_per_deg_lng(c_lat);
            } else {
                px = x + jx;
                py = y + jy;
            }
            rad = if lt == 4 {
                round_to(if rng.random() < 0.75 { rng.uniform(30.0, 100.0) } else { rng.uniform(100.0, 550.0) }, 2)
            } else if lt == 1 {
                round_to(if rng.random() < 0.75 { rng.uniform(1.6, 12.0) } else { rng.uniform(12.0, 95.0) }, 2)
            } else if lt == 12 {
                round_to(rng.uniform(30.0, 125.0), 2)
            } else if lt == 5 {
                round_to(rng.uniform(25.0, 300.0), 2)
            } else {
                550.0
            };
            state = rng.weighted(&[(1, 102), (2, 124), (3, 136)]);
        }
        if typ != -1 {
            dist_acc += d_step; // 异常点漂移不计入累计距离
        }
        let (lat, lng) = to_bd(px, py, c_lat, c_lng);
        alt += 0.04 * (82.0 - alt) + rng.gauss(0.0, alt_sigma);
        let v_now = dist / dur_f;
        let stride_target = 0.62 + 0.17 * v_now
            + 0.03 * (std::f64::consts::TAU * t_acc / 200.0 + phase_l).sin()
            + rng.gauss(0.0, 0.008);
        let v_cad = v_now * (1.0 + 0.03 * (std::f64::consts::TAU * t_acc / 70.0 + phase_v).sin());
        let cad = (v_cad / stride_target * 60.0).clamp(100.0, 200.0);
        steps_acc += cad / 60.0 * dt;
        let nxt = pos(s + direction * 2.0);
        let brg = ((nxt.0 - x).atan2(nxt.1 - y).to_degrees() + rng.gauss(0.0, 35.0)).rem_euclid(360.0);
        // 异常点（-1）：avgSpeed 为累计均值（真人与此一致，不为 0）；
        // GPS 瞬时速度多为低速，偶发 15-46 km/h 漂移尖峰
        let (avg_sp, gps_speed) = if typ == -1 {
            let avg = round_to(dist_acc / t_acc.max(1.0), 4);
            let gps = if rng.random() < 0.12 {
                rng.uniform(15.0, 46.0)
            } else {
                rng.uniform(0.5, 6.0)
            };
            (avg, round_to(gps, 4))
        } else {
            let avg = round_to(d_step / dt, 4);
            let kmh = avg * 3.6;
            let sigma = (kmh * 0.08).max(0.05);
            let gps = if rng.random() < 0.20 {
                0.0
            } else {
                round_to((kmh + rng.gauss(0.0, sigma)).max(0.0), 4)
            };
            (avg, gps)
        };

        ten_windows.add(dt, speeds[i], cad / 60.0);
        locs.push(GenPoint {
            id: i as i64 + 1,
            flag: start_ms,
            lat: -1.0,
            lng: -1.0,
            gLat: round_to(lat, 7),
            gLng: round_to(lng, 7),
            speed: round_to(gps_speed, 4),
            avgSpeed: avg_sp,
            radius: rad,
            accuracy: rad,
            ptype: typ,
            locType: lt,
            hasAltitude: true,
            totalTime: round_to(t_acc, 0) as i64,
            totalDis: round_to(dist_acc, 4),
            validDis: round_to(dist_acc, 4),
            validTime: round_to(t_acc, 0) as i64,
            steps: steps_acc as i64,
            stepDistance: 0.0,
            gainTime: fmt_gain_time(start_ms + (t_acc * 1000.0) as i64),
            gainTimeMs: start_ms + (t_acc * 1000.0) as i64,
            queueNum: 0,
            coorType: "gcj02".into(),
            bdA: round_to(alt, 2),
            bdD: round_to(brg, 2),
            bdS: round_to((avg_sp * rng.uniform(0.6, 0.95)).max(0.0), 3),
            bdG: rng.choice(&[1, 1, 1, -1]),
            count: rng.randint(20, 88),
            dtr: 0.0,
            state,
            locationId: String::new(),
        });
    }
    let (ten_speed, ten_steps) = ten_windows.finish();

    let mut segments: Vec<Segment> = Vec::new();
    let (mut seg_t, mut seg_d, mut seg_n) = (0.0f64, 0.0f64, 0i64);
    let mut seg_v: Vec<f64> = Vec::new();
    for i in 0..n {
        seg_t += dts[i];
        seg_d += seg_dist[i];
        seg_v.push(speeds[i]);
        seg_n += 1;
        if seg_t >= 60.0 || i == n - 1 {
            segments.push(Segment {
                totalTime: round_to(seg_t, 0) as i64,
                distance: round_to(seg_d, 0) as i64,
                startTime: round_to((times[i] - seg_t) * 1000.0, 0) as i64,
                endTime: round_to(times[i] * 1000.0, 0) as i64,
                avgSpeed: round_to(seg_v.iter().sum::<f64>() / seg_v.len() as f64, 3),
                avgStep: round_to(steps_acc / 1.0f64.max(t_acc) * 60.0, 0) as i64,
                state: 0,
            });
            seg_t = 0.0;
            seg_d = 0.0;
            seg_v.clear();
            seg_n = 0;
        }
    }
    let _ = seg_n;

    apply_post_fixes(&mut locs, &mut rng, start_ms);

    // 打卡点经过保障：距离在 (40m, 60m] 的点把最近轨迹点拉到 ~38m（自然路过距离，
    // 非穿心）；体育场模式通常已 ≤40m 天然满足，不受影响。
    for pl in points_bd {
        let m_lng = crate::track::geom::m_per_deg_lng(pl.0);
        let mut best_i = None;
        let mut best_d = 1e18f64;
        for (i, q) in locs.iter().enumerate() {
            let dd = ((q.gLat - pl.0) * MET_PER_DEG_LAT).powi(2)
                + ((q.gLng - pl.1) * m_lng).powi(2);
            if dd < best_d {
                best_d = dd;
                best_i = Some(i);
            }
        }
        if let Some(i) = best_i {
            let d = best_d.sqrt();
            if d > 40.0 && d <= 60.0 {
                // 沿"轨迹点→打卡点"方向回拉至 38m
                let (q, rest) = locs.split_at_mut(i);
                let _ = q;
                let p = &mut rest[0];
                let k = 38.0 / d;
                p.gLat += (pl.0 - p.gLat) * (1.0 - k);
                p.gLng += (pl.1 - p.gLng) * (1.0 - k);
            }
        }
    }

    let total_dis = round_to(dist, 3);
    Track {
        totalTime: round_to(t_acc, 0) as i64,
        totalDistance: total_dis,
        validDistance: total_dis,
        validTime: round_to(t_acc, 0) as i64,
        startTime: start_ms,
        startLatitude: locs[0].gLat,
        startLongitude: locs[0].gLng,
        totalSteps: steps_acc as i64,
        locations: locs,
        speedPerTenSec: ten_speed,
        stepsPerTenSec: ten_steps,
        segments,
    }
}

#[cfg(test)]
mod ten_window_tests {
    use super::*;

    #[test]
    fn splits_each_interval_at_boundaries_and_retains_one_second_tail() {
        let mut windows = TenWindowAccumulator::default();
        windows.add(4.0, 2.0, 1.0);
        windows.add(8.0, 4.0, 3.0);
        // 单个采样段跨越多个完整窗和尾窗。
        windows.add(19.0, 3.0, 2.0);
        let (speed, steps) = windows.finish();
        assert_eq!(speed.iter().map(|w| (w.time, w.value)).collect::<Vec<_>>(),
                   vec![(10, 32.0), (10, 32.0), (10, 30.0), (1, 3.0)]);
        assert_eq!(steps.iter().map(|w| (w.time, w.value)).collect::<Vec<_>>(),
                   vec![(10, 22.0), (10, 22.0), (10, 20.0), (1, 2.0)]);
    }

    #[test]
    fn exact_boundary_and_empty_input_have_no_extra_window() {
        let (speed, steps) = TenWindowAccumulator::default().finish();
        assert!(speed.is_empty() && steps.is_empty());
        let mut windows = TenWindowAccumulator::default();
        windows.add(10.0, 2.0, 1.5);
        let (speed, steps) = windows.finish();
        assert_eq!(speed.len(), 1);
        assert_eq!((speed[0].time, speed[0].value), (10, 20.0));
        assert_eq!((steps[0].time, steps[0].value), (10, 15.0));
    }

    #[test]
    fn every_partial_tail_keeps_its_actual_duration_and_amounts() {
        for tail in 1..10 {
            let mut windows = TenWindowAccumulator::default();
            windows.add((10 + tail) as f64, 2.5, 2.0);
            let (speed, steps) = windows.finish();
            assert_eq!((speed[1].time, speed[1].value), (tail, tail as f64 * 2.5));
            assert_eq!((steps[1].time, steps[1].value), (tail, tail as f64 * 2.0));
        }
    }

    #[test]
    fn legacy_track_preserves_actual_tail_and_rounded_totals() {
        let points = [(38.901678, 121.540241), (38.902564, 121.541233),
                      (38.900921, 121.542310), (38.899823, 121.541010),
                      (38.900455, 121.539512)];
        for duration in [1211, 1219, 1220] {
            let track = build(3300.0, duration, 42, (38.9, 121.54), 1_788_958_186_123, &points);
            assert_window_totals(&track, duration);
        }
    }

    pub(super) fn assert_window_totals(track: &Track, duration: i64) {
        let speed = &track.speedPerTenSec;
        let steps = &track.stepsPerTenSec;
        assert_eq!(speed.len(), ((duration + 9) / 10) as usize);
        assert_eq!(speed.len(), steps.len());
        let tail = if duration % 10 == 0 { 10 } else { duration % 10 };
        assert_eq!(speed.last().unwrap().time, tail);
        assert_eq!(steps.last().unwrap().time, tail);
        assert_eq!(speed.iter().map(|w| w.time).sum::<i64>(), duration);
        for (a, b) in speed.iter().zip(steps) {
            assert_eq!(a.time, b.time);
        }
        // 每窗独立舍入，允许每个窗最多半个单位的累计误差。
        assert!((speed.iter().map(|w| w.value).sum::<f64>() - track.totalDistance).abs()
                <= speed.len() as f64 * 0.005 + 1.01);
        assert!((steps.iter().map(|w| w.value).sum::<f64>() - track.totalSteps as f64).abs()
                <= steps.len() as f64 * 0.5 + 1.0);
    }
}
