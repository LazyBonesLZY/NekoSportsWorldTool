//! 自然轨迹生成器。
//!
//! 画像驱动：打卡点拟合闭合环（Catmull-Rom，每段 18 采样）→ 弧长表；
//! 采样间隔主 5s（80%）；速度曲线 = ramp × 疲劳 × 三正弦 × 余弦凹陷 × 微噪；
//! 哨兵点（首点 type=0、索引1 type=5、末点 type=6）。
//!
//! **距离口径唯一**：所有距离都从最终下发的 `gLat/gLng` 反算，
//! `totalDis` 是折线前缀和、`totalDistance` 是折线实长、`avgSpeed` 是本段实长/本段时长，
//! 十秒窗与总步数也由同一份实测值派生。因此服务端用坐标重算的结果与声明值一致。
#![allow(non_snake_case)]

use super::geom::{fmt_gain_time, make_stadium_ring, ring_point_at, round_to, to_bd, Rng};
use super::model::{GenPoint, TenWindow, Track};
use super::postfix::apply_post_fixes;

/// 有效配速窗口（判定规则 2'21"-10'00"/km ≈ 1.667-7.092 m/s），硬边界留余量。
pub const SPEED_FLOOR: f64 = 1.90;
pub const SPEED_CEIL: f64 = 6.30;

/// 海拔曲线基准（米）。GPS 气压海拔本身有几十米量级的系统偏差，
/// 这里只保证曲线形状与爬升量自洽；需要真实海拔时由用户手动覆盖。
const ALT_BASE_M: f64 = 82.0;

/// 将每段的「实测距离 / 实测步数」按真实时间边界拆到连续的十秒窗。
///
/// 每个窗独立取整会累积误差（旧实现 Σ窗步数 1200 而 totalSteps 1203）。
/// 这里把取整余量搬运到下一窗，保证
/// `Σ窗距离 == totalDistance`、`Σ窗步数 == totalSteps` 严格成立。
#[derive(Default)]
pub(super) struct TenWindowAccumulator {
    time: f64,
    distance: f64,
    steps: f64,
    distance_carry: f64,
    steps_carry: f64,
    speeds: Vec<TenWindow>,
    step_counts: Vec<TenWindow>,
}

impl TenWindowAccumulator {
    /// `distance` / `steps` 是本段（`dt` 秒）实际发生的量。
    pub(super) fn add(&mut self, mut dt: f64, mut distance: f64, mut steps: f64) {
        while dt > 1e-9 {
            let part = dt.min(10.0 - self.time);
            let frac = part / dt;
            self.time += part;
            self.distance += distance * frac;
            self.steps += steps * frac;
            distance -= distance * frac;
            steps -= steps * frac;
            dt -= part;
            if self.time >= 10.0 - 1e-9 {
                self.flush();
            }
        }
    }

    fn flush(&mut self) {
        // 生成器的时间网格以整秒为单位，尾窗保留实际秒数和累计量。
        let time = round_to(self.time, 0) as i64;
        let d = self.distance + self.distance_carry;
        let rd = round_to(d, 2);
        self.distance_carry = d - rd;
        self.speeds.push(TenWindow { time, value: rd });
        let s = self.steps + self.steps_carry;
        let rs = round_to(s, 0);
        self.steps_carry = s - rs;
        self.step_counts.push(TenWindow { time, value: rs });
        self.time = 0.0;
        self.distance = 0.0;
        self.steps = 0.0;
    }

    pub(super) fn finish(mut self) -> (Vec<TenWindow>, Vec<TenWindow>) {
        if self.time > 1e-9 {
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
pub fn build(
    dist: f64,
    dur: i64,
    seed: u64,
    center: (f64, f64),
    start_ms: i64,
    points_bd: &[(f64, f64)],
) -> Track {
    build_with_fence(dist, dur, seed, center, start_ms, points_bd, None)
}

/// 声明距离与折线实长的对齐容差（米）。
/// 服务端会用 `gLat/gLng` 重算一次距离，声明的 `totalDis` 必须等于折线实长，
/// 否则距离/配速等交叉校验会出现系统性偏差（旧实现固定偏大 1.33%）。
const LENGTH_TOLERANCE_M: f64 = 0.05;

/// 校准目标比请求距离多留 0.5 m：提交层按 `(long)totalDis` 截断，
/// 少 0.5 m 会让 1000 m 的达标阈值变成 999。
const LENGTH_BIAS_M: f64 = 0.25;

/// 无点位（户外跑/自由跑）时合成一圈标准跑道的采样点（GCJ-02）。
///
/// 这两类跑法服务端不下发打卡点和围栏，轨迹只能围绕设备锚点生成。生成的形状取
/// 标准体育场：两条直道 + 两个半圆，周长 `perimeter_m`（默认 400m），
/// 弯道半径按周长等比缩放（400m 跑道取 R≈36.5m）。
///
/// 返回值直接喂给 `build_with_fence`：这些点都在同一条跑道上，`make_stadium_ring`
/// 的 40m 覆盖校验必然成立，不会退化成「按点位包围盒猜形状」。
pub fn synthetic_track_points(center: (f64, f64), perimeter_m: f64, seed: u64) -> Vec<(f64, f64)> {
    let mut rng = Rng::new(seed ^ 0x5DEE_CE66_D1CE_4E5B);
    let p = perimeter_m.clamp(200.0, 800.0);
    // 400m 标准跑道：R=36.5，直道 L=(400-2πR)/2≈85.3；按周长等比缩放。
    let radius = 36.5 * p / 400.0;
    let straight = ((p - 2.0 * std::f64::consts::PI * radius) / 2.0).max(5.0);
    let rot: f64 = rng.uniform(0.0, std::f64::consts::TAU);
    let (sin_r, cos_r) = rot.sin_cos();
    let m_lat = super::geom::m_per_deg_lat(center.0);
    let m_lng = super::geom::m_per_deg_lng(center.0);
    let mut out = Vec::with_capacity(20);
    // 沿跑道的参数化：0..2L 为直道，2L..2L+πR 为半圆，依此类推。
    let half_l = straight / 2.0;
    for i in 0..20 {
        let s = i as f64 / 20.0 * p;
        let (mut x, mut y) = if s < straight {
            (-half_l + s, -radius)
        } else if s < straight + std::f64::consts::PI * radius {
            let a = (s - straight) / radius;
            (half_l + radius * a.sin(), -radius * a.cos())
        } else if s < 2.0 * straight + std::f64::consts::PI * radius {
            (half_l - (s - straight - std::f64::consts::PI * radius), radius)
        } else {
            let a = (s - 2.0 * straight - std::f64::consts::PI * radius) / radius;
            (-half_l - radius * a.sin(), radius * a.cos())
        };
        // 轻微扰动，避免 20 个点严格共形（真机 GPS 也不是完美跑道）。
        x += rng.gauss(0.0, 0.6);
        y += rng.gauss(0.0, 0.6);
        let (rx, ry) = (x * cos_r - y * sin_r, x * sin_r + y * cos_r);
        out.push((center.0 + ry / m_lat, center.1 + rx / m_lng));
    }
    out
}

/// 已知场地优先沿卫星描线生成；其他点位保留原有估计路径。
/// 围栏只是边界，绝不参与推测跑道长轴。
pub fn build_with_fence(
    dist: f64,
    dur: i64,
    seed: u64,
    _center: (f64, f64),
    start_ms: i64,
    points_bd: &[(f64, f64)],
    fence_bd: Option<&[(f64, f64)]>,
) -> Track {
    // 两段式标定：先按单位尺度生成并量出真实折线长度，再按 目标/实测 缩放沿环推进量。
    // 缩放只改变推进弧长，不改变 RNG 抽样序列，因此每次重生成的人性化参数完全一致。
    let target = dist + LENGTH_BIAS_M;
    let mut scale = 1.0f64;
    let mut out = emit(dist, dur, seed, start_ms, points_bd, fence_bd, scale);
    for _ in 0..5 {
        if (out.polyline_len - target).abs() <= LENGTH_TOLERANCE_M || out.polyline_len < 1.0 {
            break;
        }
        scale = (scale * target / out.polyline_len).clamp(0.2, 5.0);
        out = emit(dist, dur, seed, start_ms, points_bd, fence_bd, scale);
    }
    out.track
}

struct Emitted {
    track: Track,
    polyline_len: f64,
}

/// 单次生成。`scale` 是沿描线推进量的缩放系数，由 `build_with_fence` 标定。
fn emit(
    dist: f64,
    dur: i64,
    seed: u64,
    start_ms: i64,
    points_bd: &[(f64, f64)],
    fence_bd: Option<&[(f64, f64)]>,
    scale: f64,
) -> Emitted {
    let mut rng = Rng::new(seed);
    let dur_f = (dur.max(1)) as f64;
    // 稀疏点位不是跑道完整边界。先匹配已有描线，避免PCA把北侧点群当成全场。
    let reference = super::reference::matching_ring(points_bd);
    let has_reference = reference.is_some();
    let (dense, arcs, pc) = match reference {
        Some(reference) => reference.ring,
        None => make_stadium_ring(points_bd, &mut rng, fence_bd),
    };
    let (c_lat, c_lng) = pc;
    let direction: f64 = rng.choice(&[1.0, -1.0]);
    let s0 = rng.uniform(0.0, *arcs.last().unwrap_or(&400.0));
    let phase_v = rng.uniform(0.0, std::f64::consts::TAU);
    let phase_l = rng.uniform(0.0, std::f64::consts::TAU);

    // ── 时间轴 ────────────────────────────────────────────────────────
    // 点位 i 的时刻就是 times[i]；首点与「起点哨兵」同处 t=0（实机 type=5 的累计量
    // 恒为零），末点恰为 dur。dts[i] = 「到达点 i 所花的秒数」，因此
    // dts[0] = dts[1] = 0 且 Σ dts = dur —— 时间轴与距离轴同源。
    let mut times: Vec<f64> = vec![0.0, 0.0];
    let max_interval = if has_reference {
        (14.0 / (dist / dur_f).clamp(SPEED_FLOOR, SPEED_CEIL)).floor().clamp(1.0, 8.0)
    } else {
        8.0
    };
    let mut t = 0.0f64;
    while t < dur_f {
        // 采样间隔：真机以 **3 秒**为主。真实轨迹样本（518 点 / 1056s）的时间增量分布是
        // `{0s:162, 1s:4, 2s:4, 3s:344, 4s:3}` —— 3 秒占绝对多数，0 秒来自暂停（跑动中
        // 位置不变）；我们生成无暂停跑动，因此取 3 秒主导 + 少量 1/2/4 抖动。
        // 旧实现 80% 用 5 秒，点位密度只有真机的 0.6 倍，是可统计判别的特征。
        let interval: f64 = if rng.random() < 0.75 {
            3.0
        } else {
            rng.choice(&[1.0, 2.0, 4.0, 5.0])
        };
        t += interval.min(max_interval);
        if t >= dur_f {
            break;
        }
        times.push(t);
    }
    times.push(dur_f);
    let n = times.len();
    let dts: Vec<f64> =
        (0..n).map(|i| if i == 0 { 0.0 } else { times[i] - times[i - 1] }).collect();

    // ── 速度剖面 ──────────────────────────────────────────────────────
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
        } * if tt > dur_f - 8.0 { 1.0 + 0.03 * (tt - (dur_f - 8.0)) / 8.0 } else { 1.0 };
        let km_done = (tt / dur_f) * dist / 1000.0;
        let fatigue = if km_done <= 0.5 {
            1.03
        } else {
            (1.03 - 0.06 * (km_done - 0.5)).max(0.80)
        };
        let wave = (1.0 + 0.010 * (std::f64::consts::TAU * tt / 115.0 + phase_v).sin()
            + 0.035 * (std::f64::consts::TAU * tt / 47.0 + phase_v * 2.3).sin()
            + 0.015 * (std::f64::consts::TAU * tt / 19.0 + phase_v * 3.7).sin())
            * dip_factor(tt);
        let noise = 1.0 + rng.gauss(0.0, 0.008);
        w.push(base * ramp * fatigue * wave * noise);
    }
    // 逐点速度全部约束在有效配速窗口内，并让 Σ（速度×时长）精确命中目标距离。
    fit_speeds(&mut w, &dts, dist);
    // 每个点承担的目标位移：首点与起点哨兵零位移（实机语义），其余按速度×时长。
    let mut disp_of = vec![0.0f64; n];
    for i in 2..n {
        disp_of[i] = w[i] * dts[i];
    }

    // 点位类型：`-[SWRunCore _fixTypeOfLocation:]` @0x100143148 只写 3（暂停）与 8（恢复），
    // 停止哨兵写 6，默认 0。**1/2 在 ObjC 侧找不到任何赋值点**，属旧实现凭空生成。
    // 本工具生成「无暂停、GPS 良好」的跑动，因此全程只发 0，加首/中/末三个哨兵。
    let kinds: Vec<i64> = vec![0; n];

    // ── 几何 ──────────────────────────────────────────────────────────
    // 关键约束：每个点相对前一点的**实测位移**必须等于它的目标位移。
    // 否则 GPS 抖动会在短采样段上吃掉几十个百分点，服务端按坐标重算的距离
    // 就会与声明值系统性分叉（旧实现在 1200 m / 540 s 上偏大 1.33%）。
    let (ring_cx, ring_cy) = dense
        .iter()
        .fold((0.0f64, 0.0f64), |a, q| (a.0 + q.0, a.1 + q.1));
    let ring_cx = ring_cx / dense.len().max(1) as f64;
    let ring_cy = ring_cy / dense.len().max(1) as f64;
    let ring_len = *arcs.last().unwrap_or(&1.0);
    let laps = dist / ring_len.max(1.0);
    let curve_cut0 = rng.uniform(0.5, 2.5);
    let drift_strength = rng.uniform(0.3, 1.8);
    let curve_radius = (ring_len / std::f64::consts::TAU) * 0.55;

    let mut locs: Vec<GenPoint> = Vec::with_capacity(n);
    let mut s = s0;
    let (mut prev_x, mut prev_y) = ring_point_at(&dense, &arcs, s);
    let mut jx = 0.0f64;
    let mut jy = 0.0f64;
    let mut alt = ALT_BASE_M + rng.uniform(-1.0, 1.0);
    // 海拔噪声：目标是**平稳标准差** 1.8~2.8m，而不是随跑动时长衰减。
    // 依据真实轨迹样本（DebugRunTrack_locations.geojson，518 点）：
    //   bdA 标准差 = 2.37m，5%~95% 跨度 = 7.26m，全跨度 19.46m（含一次 GPS 跳变）。
    // 旧实现 `uniform(3.8,6.2)/(0.4*n_est)` 让跑得越久越平滑（长跑 n_est 大 → σ 小），
    // 实测只有 σ≈0.18m、跨度 1.05m，比真机平滑 13 倍，是可直接统计判别的特征。
    // 平稳标准差与均值回归系数 θ 的关系：σ_step = σ_target * sqrt(2θ − θ²)。
    const ALT_THETA: f64 = 0.04;
    let alt_sigma =
        rng.uniform(1.8, 2.8) * (2.0 * ALT_THETA - ALT_THETA * ALT_THETA).sqrt();
    let mut steps_cum = 0.0f64;

    for i in 0..n {
        let dt = dts[i];
        let typ = kinds[i];
        let (bx, by, px, py);
        if i < 2 {
            // 首点与起点哨兵必须是同一坐标：实机哨兵的累计量归零，
            // 两点坐标若不同就会凭空多出一段不进入 totalDis 的位移。
            bx = prev_x;
            by = prev_y;
            px = prev_x;
            py = prev_y;
        } else {
            let prog = times[i] / dur_f;
            let lap_now = (prog * laps).floor();
            // 人性化随机量先抽取固定，再参与下面的解算，保证解算过程不消耗 RNG。
            let u_curve = rng.uniform(0.6, 1.0);
            let u_drift = rng.uniform(0.4, 1.0);
            let branch = rng.random();
            let njx = 0.72 * jx + rng.gauss(0.0, 0.60);
            let njy = 0.72 * jy + rng.gauss(0.0, 0.60);
            let (nnx, nny) = if has_reference {
                let jitter = njx.hypot(njy);
                if jitter > 1.5 {
                    (njx * 1.5 / jitter, njy * 1.5 / jitter)
                } else {
                    (njx, njy)
                }
            } else {
                (njx, njy)
            };
            jx = nnx;
            jy = nny;
            // place(s)：环上参数 s 处、叠加径向偏移与抖动后的平面坐标。
            let place = |ss: f64| -> (f64, f64) {
                let (rx, ry) = ring_point_at(&dense, &arcs, ss);
                let radx = rx - ring_cx;
                let rady = ry - ring_cy;
                let rlen = radx.hypot(rady).max(1e-6);
                let mut radial = 0.0f64;
                if rlen > curve_radius {
                    radial -= curve_cut0 * (1.0 + 0.35 * lap_now) * u_curve;
                } else if prog > 0.35 && branch < 0.30 {
                    radial += drift_strength * prog * u_drift;
                }
                if has_reference {
                    radial = radial.clamp(-0.8, 0.8);
                }
                let k = ((rlen + radial) / rlen).clamp(0.65, 1.35);
                (ring_cx + radx * k + jx, ring_cy + rady * k + jy)
            };
            // 一维牛顿：沿环推进 step 米，使实测位移命中目标。
            let target = (disp_of[i] * scale).max(0.01);
            let mut step = target;
            let mut best = place(s + direction * step);
            for _ in 0..6 {
                let d = (best.0 - prev_x).hypot(best.1 - prev_y);
                let err = target - d;
                if err.abs() < 0.01 {
                    break;
                }
                step = (step + 0.85 * err).max(0.01);
                best = place(s + direction * step);
            }
            s += direction * step;
            px = best.0;
            py = best.1;
            let (rbx, rby) = ring_point_at(&dense, &arcs, s);
            bx = rbx;
            by = rby;
        }
        prev_x = px;
        prev_y = py;

        // radius = CLLocation.horizontalAccuracy。户外常态 3~15m；
        // 全程 1.4~2.4m 属于「GPS 完美到不可能」，本身就是可疑特征。
        // 真机还会对它做 10 样本滑动中位数（见下面的 median_filter_accuracy）。
        let rad = round_to(rng.uniform(3.2, 12.0), 2);
        let state = if typ == 0 {
            rng.weighted(&[(1, 145), (2, 45), (3, 164)])
        } else {
            rng.weighted(&[(1, 145), (2, 256), (3, 151)])
        };
        let (lat, lng) = to_bd(px, py, c_lat, c_lng);
        alt += ALT_THETA * (ALT_BASE_M - alt) + rng.gauss(0.0, alt_sigma);

        // 步频只由目标速度决定，累计步数在时间轴上积分；总步数与十秒窗同源。
        let v_now = w[i];
        let stride_target = 0.62
            + 0.17 * v_now
            + 0.03 * (std::f64::consts::TAU * times[i] / 200.0 + phase_l).sin()
            + rng.gauss(0.0, 0.008);
        let v_cad = v_now * (1.0 + 0.03 * (std::f64::consts::TAU * times[i] / 70.0 + phase_v).sin());
        let cad = (v_cad / stride_target.max(0.4) * 60.0).clamp(100.0, 200.0);
        steps_cum += cad / 60.0 * dt;

        let (nx2, ny2) = ring_point_at(&dense, &arcs, s + direction * 2.0);
        // `bdD` = CLLocation.course（航向，度）。真实样本相邻航向变化中位 **15.34°**
        // （均值 29.25°，39% 小于 10°），其中约 6° 来自跑道本身的转弯
        // （3s×6.7m 在 400m 跑道上约转 6°），其余是 GPS 航向噪声。
        // 旧实现 `gauss(0, 35)` 两个独立噪声取差后 σ_d≈49°，抖动是真机的 2 倍多。
        // 取 σ=14：差值的 median|Δ|≈0.954σ≈13.4°，叠加转弯约 15°，与样本吻合。
        // 真实航向噪声是**厚尾**的：中位 15.34° 但均值 29.25°、最大 179.48°（一次完全翻转），
        // 说明多数点噪声不大、少数点会因多径大幅跳变。用混合分布模拟：
        // 95% 取 N(0,14)（主导），5% 取 N(0,55)（多径跳变）。
        let brg_noise = if rng.random() < 0.05 {
            rng.gauss(0.0, 55.0)
        } else {
            rng.gauss(0.0, 14.0)
        };
        let brg = ((nx2 - bx).atan2(ny2 - by).to_degrees() + brg_noise).rem_euclid(360.0);

        locs.push(GenPoint {
            id: i as i64 + 1,
            flag: start_ms,
            lat: -1.0,
            lng: -1.0,
            gLat: round_to(lat, 7),
            gLng: round_to(lng, 7),
            speed: 0.0,
            avgSpeed: 0.0,
            radius: rad,
            accuracy: rad,
            ptype: typ,
            locType: 1,
            hasAltitude: true,
            totalTime: round_to(times[i], 0) as i64,
            totalDis: 0.0,
            validDis: 0.0,
            validTime: round_to(times[i], 0) as i64,
            steps: round_to(steps_cum, 0) as i64,
            stepDistance: 0.0,
            gainTime: fmt_gain_time(start_ms + (times[i] * 1000.0) as i64),
            gainTimeMs: start_ms + (times[i] * 1000.0) as i64,
            queueNum: 0,
            coorType: "gcj02".into(),
            bdA: round_to(alt, 2),
            bdD: round_to(brg, 2),
            bdS: 0.0,
            bdG: rng.choice(&[1, 1, 1, -1]),
            count: rng.randint(20, 88),
            dtr: 0.0,
            state,
            locationId: String::new(),
        });
    }

    // 精度序列平滑：`-[SWValidPointDetector _smoothAccuracy:]` @0x100127300 把
    // `(int)horizontalAccuracy` 塞进一个最多 10 个样本的窗口，排序后取 `count>>1` 作中位数，
    // 上传的 `radius` 用的是这个平滑值。逐点独立随机的精度序列在统计上明显更"白"，
    // 是一个可被判别的特征。
    median_filter_accuracy(&mut locs, 10);

    // 打卡点经过保障：距离在 (40m, 60m] 的点把最近轨迹点拉到 ~38m。
    snap_checkpoints(&mut locs, points_bd);

    // 吸附会改变折线长度，而速度标定发生在吸附**之前**。
    // 大轨迹上这点偏移可忽略，但短轨迹（200~300m 量级）会被放大到 7%：
    // 实测 200m 目标只得 185m。这里在吸附后按目标长度做一次整体缩放补正。
    // 缩放是绕质心的相似变换，checkpoint 与轨迹的相对位置最多变化几米，
    // 仍远小于 40m 的通过阈值（越界时 flow 会告警）。
    correct_length_after_snap(&mut locs, dist, points_bd);

    // ── 距离与速度终态（唯一口径）────────────────────────────────────
    // 折线实长从这里开始就是权威值：逐点 totalDis = 前缀和，
    // 逐点 avgSpeed = 本段实长 / 本段时长，totalDistance = 折线实长。
    let gdist = segment_distances(&locs);
    let total_dis: f64 = gdist.iter().sum();
    let mut acc = 0.0f64;
    for i in 0..n {
        acc += gdist[i];
        locs[i].totalDis = round_to(acc, 4);
        locs[i].validDis = round_to(acc, 4);
    }

    apply_post_fixes(&mut locs, &mut rng, start_ms);

    for i in 2..n {
        let dt = dts[i];
        if dt > 0.0 {
            locs[i].avgSpeed = round_to(gdist[i] / dt, 4);
        }
    }

    // `speed` 是**配速（min/km）**，不是 km/h：真机 `-[SWRunRawData speed]` @0x1000A03F4
    // 走 `sub_1000FEBF0` = `16.666666666666668 / velocity`（velocity 为 m/s）。
    // 所以 `speed` 完全由 `avgSpeed` 推出，二者不能各自加噪声——独立噪声会让同一个点上
    // 两个字段互相矛盾，是可直接交叉校验的特征。
    // `bdS` 是 CLLocation.speed（m/s），与配速是两个不同的量，仍单独给读数。
    let mut nrng = Rng::new(seed ^ 0x9E37_79B9_7F4A_7C15);
    for p in locs.iter_mut().skip(1) {
        let avg = p.avgSpeed;
        p.speed = if avg > 0.000_001 {
            round_to(16.666_666_666_666_668 / avg, 4)
        } else {
            0.0
        };
        // `bdS` 是 CLLocation.speed（瞬时 m/s）。它应当**围绕** avgSpeed 波动，
        // 而不是恒定低于平均值：真实轨迹样本（DebugRunTrack_locations.geojson）
        // 里 bdS > avgSpeed 占 94.6%。恒低是一个可直接统计判别的特征。
        // 我们生成的是匀速无暂停跑动，因此比值取 0.90~1.10 的对称噪声。
        p.bdS = round_to((avg * nrng.uniform(0.90, 1.10)).max(0.0), 3);
    }
    // 首点：`totalDis=0`、`totalTime=0`，因此 `avgSpeed` 必须是 **0**（真机样本首点
    // `avgSpeed=0 speed=0 bdS=0`）。旧实现把首个非零 avgSpeed 复制过来、再用
    // `avg*2.2~3.2` 编了个 speed —— 既与 totalDis=0 自相矛盾，也让首点 speed 脱离配速口径。
    locs[0].avgSpeed = 0.0;
    locs[0].speed = 0.0;
    locs[0].bdS = 0.0;

    // ── 十秒窗与总步数（同源）────────────────────────────────────────
    let mut ten_windows = TenWindowAccumulator::default();
    let mut prev_steps = 0i64;
    for i in 1..n {
        let steps_delta = (locs[i].steps - prev_steps) as f64;
        prev_steps = locs[i].steps;
        ten_windows.add(dts[i], gdist[i], steps_delta);
    }
    let (ten_speed, ten_steps) = ten_windows.finish();
    let total_steps = ten_steps.iter().map(|w| w.value).sum::<f64>()

        as i64;

    let total_dis_rounded = round_to(total_dis, 4);
    let track = Track {
        totalTime: dur,
        totalDistance: total_dis_rounded,
        validDistance: total_dis_rounded,
        validTime: dur,
        startTime: start_ms,
        startLatitude: locs[0].gLat,
        startLongitude: locs[0].gLng,
        totalSteps: total_steps,
        locations: locs,
        speedPerTenSec: ten_speed,
        stepsPerTenSec: ten_steps,
    };
    Emitted { track, polyline_len: total_dis_rounded }
}

/// 对 `radius` 做滑动中位数（窗口最多 `window` 个样本，含当前值）。
/// 与 `_smoothAccuracy:` 一致：先取整，再取排序后的 `len >> 1`。
/// 吸附后把折线实长拉回目标值（绕质心相似缩放，最多 3 轮）。
///
/// 只调整 `gLat`/`gLng`；`lat`/`lng` 在上传体里恒为 0，不参与几何。
///
/// **打卡点优先**：每轮缩放后测量「每个打卡点到轨迹的最近距离」，
/// 一旦有打卡点被推超过 40m，就回滚到本轮之前的状态并停止。
/// 目标距离短于打卡点分布时（例如 100m 目标配 5 个跨度 100m 的打卡点），
/// 几何上无法同时满足两者——此时保留吸附结果、放弃长度精确，因为
/// 服务端是按坐标复核打卡点的，长度短一点只影响达标判定。
fn correct_length_after_snap(locs: &mut [GenPoint], target: f64, checkpoints: &[(f64, f64)]) {
    if locs.len() < 3 || target <= 1.0 || !target.is_finite() {
        return;
    }
    const MAX_CHECKPOINT_DIST_M: f64 = 40.0;
    for _ in 0..3 {
        let len: f64 = locs
            .windows(2)
            .map(|w| super::geom::local_distance_m(w[0].gLat, w[0].gLng, w[1].gLat, w[1].gLng))
            .sum();
        if !len.is_finite() || len < 1.0 {
            return;
        }
        if (len - target).abs() <= 0.5 {
            return;
        }
        // 单轮限制在 ±8%，避免一次跳过头把打卡点甩出阈值
        let f = (target / len).clamp(0.92, 1.08);
        let n = locs.len() as f64;
        let clat = locs.iter().map(|p| p.gLat).sum::<f64>() / n;
        let clng = locs.iter().map(|p| p.gLng).sum::<f64>() / n;
        let snapshot: Vec<(f64, f64)> = locs.iter().map(|p| (p.gLat, p.gLng)).collect();
        for p in locs.iter_mut() {
            p.gLat = clat + (p.gLat - clat) * f;
            p.gLng = clng + (p.gLng - clng) * f;
        }
        // 打卡点优先：越界即回滚本轮
        let worst = checkpoints
            .iter()
            .map(|&(la, lo)| {
                locs.iter()
                    .map(|p| super::geom::local_distance_m(p.gLat, p.gLng, la, lo))
                    .fold(f64::MAX, f64::min)
            })
            .fold(0.0f64, f64::max);
        if worst > MAX_CHECKPOINT_DIST_M {
            for (p, (la, lo)) in locs.iter_mut().zip(snapshot) {
                p.gLat = la;
                p.gLng = lo;
            }
            return;
        }
    }
}

pub(super) fn median_filter_accuracy(locs: &mut [GenPoint], window: usize) {
    let window = window.max(1);
    let mut buf: Vec<i64> = Vec::with_capacity(window);
    for p in locs.iter_mut() {
        buf.push(p.radius.round() as i64);
        if buf.len() > window {
            buf.remove(0);
        }
        let mut sorted = buf.clone();
        sorted.sort_unstable();
        let median = sorted[sorted.len() >> 1];
        p.radius = median as f64;
        p.accuracy = p.radius;
    }
}

/// 逐段折线长度（米），只用最终下发的 `gLat/gLng` 计算。
/// 首点与起点哨兵零位移，故 gdist[0] = gdist[1] = 0。
pub(super) fn segment_distances(locs: &[GenPoint]) -> Vec<f64> {
    let mut out = vec![0.0f64; locs.len()];
    for i in 2..locs.len() {
        let a = &locs[i - 1];
        let b = &locs[i];
        out[i] = crate::track::geom::local_distance_m(a.gLat, a.gLng, b.gLat, b.gLng);
    }
    out
}

/// 把离打卡点 (40m, 60m] 的最近轨迹点沿径向回拉到 ~38m（自然路过距离，非穿心）。
fn snap_checkpoints(locs: &mut [GenPoint], points_bd: &[(f64, f64)]) {
    for pl in points_bd {
        let mut best_i = None;
        let mut best_d = 1e18f64;
        for (i, q) in locs.iter().enumerate() {
            let d = crate::track::geom::local_distance_m(q.gLat, q.gLng, pl.0, pl.1);
            if d < best_d {
                best_d = d;
                best_i = Some(i);
            }
        }
        if let Some(i) = best_i {
            if best_d > 40.0 && best_d <= 60.0 {
                let p = &mut locs[i];
                let k = 38.0 / best_d;
                p.gLat += (pl.0 - p.gLat) * (1.0 - k);
                p.gLng += (pl.1 - p.gLng) * (1.0 - k);
            }
        }
    }
}

#[cfg(test)]
mod reference_geometry_tests {
    use super::*;
    use crate::track::geom::{m_per_deg_lat, m_per_deg_lng};
    use crate::track::reference::{distance_to_ring, matching_ring};

    const POINTS: [(f64, f64); 5] = [
        (22.411290,112.940320), (22.411343,112.941001),
        (22.410866,112.941100), (22.410582,112.941216),
        (22.410823,112.940376),
    ];

    #[test]
    fn all_seeds_follow_full_mapped_track_instead_of_crossing_pitch() {
        let reference = matching_ring(&POINTS).unwrap();
        let (dense, _, center) = reference.ring;
        let scale = m_per_deg_lng(center.0);
        let m_lat = m_per_deg_lat(center.0);
        for case in 0..384 {
            let seed = case % 128;
            let (distance, duration) = [(1200.0, 540), (3300.0, 1220), (1200.0, 200)][case as usize / 128];
            let track = build(distance, duration, seed, center, 1_793_840_000_000, &POINTS);
            track.validate_consistency().unwrap();
            let mut min_y = f64::INFINITY;
            let mut max_y = f64::NEG_INFINITY;
            let visible: Vec<_> = track.locations.iter().filter(|p| p.ptype != -1).collect();
            for p in &track.locations {
                let xy = ((p.gLng-center.1)*scale, (p.gLat-center.0)*m_lat);
                let d = distance_to_ring(xy, &dense);
                assert!(d <= 3.0, "seed={seed} point={} leaves outline by {d}m", p.id);
                min_y = min_y.min(xy.1);
                max_y = max_y.max(xy.1);
            }
            assert!(max_y-min_y > 148.0, "seed={seed}: missing north/south ends");
            // 显示折线每段再检查9个内点，防止端点正确但连接横切球场。
            for pair in visible.windows(2) {
                for sample in 1..10 {
                    let t = sample as f64 / 10.0;
                    let xy = ((pair[0].gLng + (pair[1].gLng-pair[0].gLng)*t-center.1)*scale,
                              (pair[0].gLat + (pair[1].gLat-pair[0].gLat)*t-center.0)*m_lat);
                    let d = distance_to_ring(xy, &dense);
                    assert!(d < 4.0, "seed={seed}: rendered segment cuts {d}m from outline");
                }
            }
            for point in POINTS {
                let nearest = visible.iter().map(|p|
                    ((p.gLng-point.1)*scale).hypot((p.gLat-point.0)*m_lat))
                    .fold(f64::INFINITY, f64::min);
                assert!(nearest < 15.0, "seed={seed}: checkpoint distance {nearest}m");
            }
        }
    }

    #[test]
    fn seed_randomness_and_repeatability_are_preserved() {
        let make = |seed| build(1200.0, 540, seed, (22.4169,112.9472),
                               1_793_840_000_000, &POINTS);
        let a = serde_json::to_value(make(2026)).unwrap();
        assert_eq!(a, serde_json::to_value(make(2026)).unwrap());
        assert_ne!(a, serde_json::to_value(make(2027)).unwrap());
    }

    /// IPA 7.4.0 着色语义：3=灰 7/10=红 -1=不绘制。正常跑动只发绿色类型
    /// （0/1/2/8）加起终点哨兵（5/6，只画图标不着色线段）。
    #[test]
    fn all_seeds_emit_only_green_types_plus_sentinels() {
        // 真机只会产生 0（正常）/3（暂停）/8（中断后恢复）/6（停止哨兵）/5（起点哨兵）。
        // 本工具无暂停，因此实际只应出现 0 与 5/6；1/2 在 ObjC 侧没有任何赋值点。
        let allowed = |t: i64| matches!(t, 0 | 5 | 6);
        for seed in 0..64 {
            let track = build(1200.0, 540, seed, (22.4169,112.9472),
                              1_793_840_000_000, &POINTS);
            for p in &track.locations {
                assert!(allowed(p.ptype), "seed={seed} 点{} 非绿色类型 {}", p.id, p.ptype);
            }
            let net_seed_track = build(3300.0, 1220, seed, (22.4169,112.9472),
                                       1_793_840_000_000, &POINTS);
            for p in &net_seed_track.locations {
                assert!(allowed(p.ptype), "seed={seed} 长距离点{} 非绿色类型 {}", p.id, p.ptype);
            }
        }
    }
}

#[cfg(test)]
mod ten_window_tests {
    use super::*;

    #[test]
    fn splits_each_interval_at_boundaries_and_retains_one_second_tail() {
        let mut windows = TenWindowAccumulator::default();
        // (时长, 本段距离, 本段步数)
        windows.add(4.0, 8.0, 4.0);
        windows.add(8.0, 32.0, 24.0);
        // 单个采样段跨越多个完整窗和尾窗。
        windows.add(19.0, 57.0, 38.0);
        let (speed, steps) = windows.finish();
        assert_eq!(speed.iter().map(|w| (w.time, w.value)).collect::<Vec<_>>(),
                   vec![(10, 32.0), (10, 32.0), (10, 30.0), (1, 3.0)]);
        assert_eq!(steps.iter().map(|w| (w.time, w.value)).collect::<Vec<_>>(),
                   vec![(10, 22.0), (10, 22.0), (10, 20.0), (1, 2.0)]);
        // 取整余量被搬运，窗口合计严格等于输入合计。
        assert_eq!(speed.iter().map(|w| w.value).sum::<f64>(), 97.0);
        assert_eq!(steps.iter().map(|w| w.value).sum::<f64>(), 66.0);
    }

    #[test]
    fn exact_boundary_and_empty_input_have_no_extra_window() {
        let (speed, steps) = TenWindowAccumulator::default().finish();
        assert!(speed.is_empty() && steps.is_empty());
        let mut windows = TenWindowAccumulator::default();
        windows.add(10.0, 20.0, 15.0);
        let (speed, steps) = windows.finish();
        assert_eq!(speed.len(), 1);
        assert_eq!((speed[0].time, speed[0].value), (10, 20.0));
        assert_eq!((steps[0].time, steps[0].value), (10, 15.0));
    }

    #[test]
    fn every_partial_tail_keeps_its_actual_duration_and_amounts() {
        for tail in 1..10 {
            let mut windows = TenWindowAccumulator::default();
            let dur = (10 + tail) as f64;
            windows.add(dur, dur * 2.5, dur * 2.0);
            let (speed, steps) = windows.finish();
            assert_eq!((speed[1].time, speed[1].value), (tail, tail as f64 * 2.5));
            assert_eq!((steps[1].time, steps[1].value), (tail, tail as f64 * 2.0));
        }
    }

    /// 取整余量搬运：任意长度、任意小数的输入，窗口合计都必须与总量严格相等。
    #[test]
    fn rounding_carry_keeps_window_totals_exact() {
        for dur in [7.0, 13.0, 61.0, 97.5, 540.0] {
            let mut windows = TenWindowAccumulator::default();
            windows.add(dur, 1234.567, 987.654);
            let (speed, steps) = windows.finish();
            assert!((speed.iter().map(|w| w.value).sum::<f64>() - 1234.567).abs() < 0.006,
                    "dur={dur} 距离合计不符");
            assert_eq!(steps.iter().map(|w| w.value).sum::<f64>(), round_to(987.654, 0));
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
        // 取整余量被搬运，合计必须严格对上（不再允许逐窗误差累积）。
        assert!((speed.iter().map(|w| w.value).sum::<f64>() - track.totalDistance).abs() < 0.02,
                "十秒窗距离合计 {} != totalDistance {}", 
                speed.iter().map(|w| w.value).sum::<f64>(), track.totalDistance);
        assert_eq!(steps.iter().map(|w| w.value).sum::<f64>(), track.totalSteps as f64);
    }
}

#[cfg(test)]
mod length_calibration_tests {
    use super::*;
    use crate::track::geom::local_distance_m;

    fn pts() -> [(f64, f64); 5] {
        [(22.411290, 112.940320), (22.411343, 112.941001), (22.410866, 112.941100),
         (22.410582, 112.941216), (22.410823, 112.940376)]
    }

    fn worst_checkpoint_m(track: &crate::track::model::Track) -> f64 {
        pts()
            .iter()
            .map(|&(la, lo)| {
                track
                    .locations
                    .iter()
                    .map(|p| local_distance_m(p.gLat, p.gLng, la, lo))
                    .fold(f64::MAX, f64::min)
            })
            .fold(0.0f64, f64::max)
    }

    /// 距离标定必须在 **吸附之后**仍然成立。
    ///
    /// 历史上 `snap_checkpoints` 跑在 `fit_speeds` 之后，短轨迹被吸附改变长度却不重标，
    /// 实测 200m 目标只得 185m（−7.3%）、300m 得 292m（−2.7%），500m 以上才收敛。
    #[test]
    fn distance_hits_target_across_scales() {
        for &d in &[300.0f64, 500.0, 800.0, 1000.0, 3000.0] {
            let t = (d / 3.0) as i64; // 3 m/s，在速度窗口内
            let tr = build(d, t, 7, (22.4109, 112.9407), 1_793_840_000_000, &pts());
            let err = (tr.totalDistance - d).abs() / d;
            assert!(err < 0.005, "目标 {d}m 实得 {:.2}m（误差 {:.2}%）", tr.totalDistance, err * 100.0);
        }
    }

    /// 长度补正不得以牺牲打卡点为代价。
    ///
    /// 目标距离短于打卡点分布时（100~200m 配跨度约 100m 的 5 个点），
    /// 几何上无法同时满足，此时应回滚补正、保留吸附结果。
    #[test]
    fn length_correction_never_pushes_checkpoints_away() {
        for &d in &[100.0f64, 200.0, 300.0, 500.0] {
            let t = (d / 3.0) as i64;
            let tr = build(d, t, 7, (22.4109, 112.9407), 1_793_840_000_000, &pts());
            let worst = worst_checkpoint_m(&tr);
            // 300m 以上必须全部落在 40m 内；更短的目标允许放宽（无法满足）
            if d >= 300.0 {
                assert!(worst <= 40.0, "目标 {d}m 时最远打卡点 {worst:.1}m 超出 40m");
            }
            assert!(worst.is_finite(), "目标 {d}m 时打卡点距离非有限值");
        }
    }
}
