//! 轨迹几何与随机工具。
//!
//! round 封装、RNG、打卡点 Catmull-Rom 拟合环 + 弧长表 + 弧长插值。

use chrono::{Local, TimeZone};
use rand::rngs::StdRng;
use rand::SeedableRng;
use rand_distr::{Distribution, Normal};

/// 赤道子午线度长。**仅**用于 UI 地图的近似投影与旧测试；
/// 任何参与距离计算的路径都必须用 [`m_per_deg_lat`]，否则在高纬度/低纬度会有
/// 0.4% 量级的系统性偏差（22.4°N 处 111132 比真值大 0.36%）。
pub const MET_PER_DEG_LAT: f64 = 111_132.0;
/// 默认经度米/度（大连纬度 38.9°，向后兼容旧测试）；新代码用 [`m_per_deg_lng`]。
pub const MET_PER_DEG_LNG: f64 = 86_600.0;

/// 纬度相关的经度米/度：111320 × cos(lat)。江门(22.4°)≈102913，大连(38.9°)≈86600。
#[inline]
pub fn m_per_deg_lng(lat: f64) -> f64 {
    111_320.0 * lat.to_radians().cos()
}

/// 纬度相关的纬度米/度（WGS-84 子午圈曲率半径展开式）。
/// 22.4°N ≈ 110736 m/deg，赤道 110574，极点 111694。
/// 距离计算必须用它：常数 111132 在 22.4°N 会让人以为跑得比实际多 0.36%。
#[inline]
pub fn m_per_deg_lat(lat: f64) -> f64 {
    let p = lat.to_radians();
    111_132.92 - 559.82 * (2.0 * p).cos() + 1.175 * (4.0 * p).cos()
}

/// 两点间局部平面距离（米）。以中点纬度取换算系数，短基线误差 < 1e-5。
#[inline]
pub fn local_distance_m(a_lat: f64, a_lng: f64, b_lat: f64, b_lng: f64) -> f64 {
    let lat_ref = (a_lat + b_lat) * 0.5;
    let dy = (b_lat - a_lat) * m_per_deg_lat(lat_ref);
    let dx = (b_lng - a_lng) * m_per_deg_lng(lat_ref);
    dx.hypot(dy)
}

/// round(x, n)：按精确二进制值四舍五入。
pub fn round_to(x: f64, n: usize) -> f64 {
    let s = format!("{x:.n$}");
    s.parse().unwrap_or(x)
}

/// 可播种 RNG 封装。
pub struct Rng {
    inner: StdRng,
}

impl Rng {
    pub fn new(seed: u64) -> Self {
        Self {
            inner: StdRng::seed_from_u64(seed),
        }
    }
    pub fn random(&mut self) -> f64 {
        rand::Rng::gen_range(&mut self.inner, 0.0..1.0)
    }
    pub fn uniform(&mut self, a: f64, b: f64) -> f64 {
        rand::Rng::gen_range(&mut self.inner, a..=b)
    }
    pub fn randint(&mut self, a: i64, b: i64) -> i64 {
        rand::Rng::gen_range(&mut self.inner, a..=b)
    }
    pub fn gauss(&mut self, mu: f64, sigma: f64) -> f64 {
        Normal::new(mu, sigma).unwrap().sample(&mut self.inner)
    }
    pub fn choice<T>(&mut self, items: &[T]) -> T
    where
        T: Copy,
    {
        items[rand::Rng::gen_range(&mut self.inner, 0..items.len())]
    }
    pub fn weighted<T>(&mut self, items: &[(T, u32)]) -> T
    where
        T: Copy,
    {
        let total: u32 = items.iter().map(|(_, w)| *w).sum();
        let mut u = self.uniform(0.0, total as f64);
        for (item, w) in items {
            u -= *w as f64;
            if u < 0.0 {
                return *item;
            }
        }
        items[items.len() - 1].0
    }
}

/// 打卡点 BD 系 (lat, lng) → 闭合平面路径 + 弧长表 + 中心。
pub type PointRing = (Vec<(f64, f64)>, Vec<f64>, (f64, f64));

/// 闭合路径累积弧长表（长度 = 点数 + 1，末项为总长）。
pub(super) fn arc_table(dense: &[(f64, f64)]) -> Vec<f64> {
    let mut arcs = vec![0.0f64];
    for i in 1..=dense.len() {
        let a = dense[i - 1];
        let b = dense[i % dense.len()];
        arcs.push(arcs[i - 1] + ((b.0 - a.0).powi(2) + (b.1 - a.1).powi(2)).sqrt());
    }
    arcs
}

pub fn make_point_ring(bd_points: &[(f64, f64)]) -> PointRing {
    let n = bd_points.len();
    let cx = bd_points.iter().map(|q| q.0).sum::<f64>() / n as f64;
    let cy = bd_points.iter().map(|q| q.1).sum::<f64>() / n as f64;
    let mut ordered = bd_points.to_vec();
    ordered.sort_by(|a, b| {
        let ka = (a.0 - cx).atan2(a.1 - cy);
        let kb = (b.0 - cx).atan2(b.1 - cy);
        ka.partial_cmp(&kb).unwrap()
    });
    let m_lng = m_per_deg_lng(cx);
    let plane: Vec<(f64, f64)> = ordered
        .iter()
        .map(|q| ((q.1 - cy) * m_lng, (q.0 - cx) * MET_PER_DEG_LAT))
        .collect();
    let samples = 18usize;
    let mut dense = Vec::with_capacity(n * samples);
    for i in 0..n {
        let p0 = plane[(i + n - 1) % n];
        let p1 = plane[i];
        let p2 = plane[(i + 1) % n];
        let p3 = plane[(i + 2) % n];
        for j in 0..samples {
            let t = j as f64 / samples as f64;
            let (t2, t3) = (t * t, t * t * t);
            let x = 0.5
                * ((2.0 * p1.0)
                    + (-p0.0 + p2.0) * t
                    + (2.0 * p0.0 - 5.0 * p1.0 + 4.0 * p2.0 - p3.0) * t2
                    + (-p0.0 + 3.0 * p1.0 - 3.0 * p2.0 + p3.0) * t3);
            let y = 0.5
                * ((2.0 * p1.1)
                    + (-p0.1 + p2.1) * t
                    + (2.0 * p0.1 - 5.0 * p1.1 + 4.0 * p2.1 - p3.1) * t2
                    + (-p0.1 + 3.0 * p1.1 - 3.0 * p2.1 + p3.1) * t3);
            dense.push((x, y));
        }
    }
    let mut arcs = vec![0.0f64];
    for i in 1..=dense.len() {
        let a = dense[i - 1];
        let b = dense[i % dense.len()];
        arcs.push(arcs[i - 1] + ((b.0 - a.0).powi(2) + (b.1 - a.1).powi(2)).sqrt());
    }
    (dense, arcs, (cx, cy))
}

/// 环线弧长 → 坐标（线性插值）。
pub fn ring_point_at(dense: &[(f64, f64)], arcs: &[f64], s: f64) -> (f64, f64) {
    let total = *arcs.last().unwrap_or(&1.0);
    let s = s.rem_euclid(total);
    let mut lo = 0usize;
    let mut hi = arcs.len();
    while lo < hi {
        let mid = (lo + hi) / 2;
        if arcs[mid] < s {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    let i = lo.max(1);
    let a = dense[(i - 1) % dense.len()];
    let b = dense[i % dense.len()];
    let seg = arcs[i] - arcs[i - 1];
    let t = if seg > 0.0 {
        (s - arcs[i - 1]) / seg
    } else {
        0.0
    };
    (a.0 + (b.0 - a.0) * t, a.1 + (b.1 - a.1) * t)
}

pub fn to_bd(x: f64, y: f64, c_lat: f64, c_lng: f64) -> (f64, f64) {
    let m_lng = m_per_deg_lng(c_lat);
    (c_lat + y / MET_PER_DEG_LAT, c_lng + x / m_lng)
}

// ── 坐标基准转换：WGS84 ↔ GCJ-02 ↔ BD-09 ─────────────────────────
// 打卡点来自服务端为 BD-09；OSM 路网为 WGS84。真实道路路由需先把路网
// 逐节点转成 BD-09，与经典模式（直接在 BD-09 上拟合打卡点环）保持一致。

const X_PI: f64 = std::f64::consts::PI * 3000.0 / 180.0;
const WGS_A: f64 = 6378245.0;
const WGS_EE: f64 = 0.006_693_421_622_965_943;

fn out_of_china(lat: f64, lng: f64) -> bool {
    lng < 72.004 || lng > 137.8347 || lat < 0.8293 || lat > 55.8271
}

fn transform_lat(x: f64, y: f64) -> f64 {
    let pi = std::f64::consts::PI;
    let mut ret = -100.0 + 2.0 * x + 3.0 * y + 0.2 * y * y + 0.1 * x * y + 0.2 * x.abs().sqrt();
    ret += (20.0 * (6.0 * x * pi).sin() + 20.0 * (2.0 * x * pi).sin()) * 2.0 / 3.0;
    ret += (20.0 * (y * pi).sin() + 40.0 * (y / 3.0 * pi).sin()) * 2.0 / 3.0;
    ret += (160.0 * (y / 12.0 * pi).sin() + 320.0 * (y * pi / 30.0).sin()) * 2.0 / 3.0;
    ret
}

fn transform_lng(x: f64, y: f64) -> f64 {
    let pi = std::f64::consts::PI;
    let mut ret = 300.0 + x + 2.0 * y + 0.1 * x * x + 0.1 * x * y + 0.1 * x.abs().sqrt();
    ret += (20.0 * (6.0 * x * pi).sin() + 20.0 * (2.0 * x * pi).sin()) * 2.0 / 3.0;
    ret += (20.0 * (x * pi).sin() + 40.0 * (x / 3.0 * pi).sin()) * 2.0 / 3.0;
    ret += (150.0 * (x / 12.0 * pi).sin() + 300.0 * (x / 30.0 * pi).sin()) * 2.0 / 3.0;
    ret
}

/// WGS84 → GCJ-02（火星坐标，境内偏移）。
pub fn wgs84_to_gcj02(lat: f64, lng: f64) -> (f64, f64) {
    if out_of_china(lat, lng) {
        return (lat, lng);
    }
    let pi = std::f64::consts::PI;
    let mut dlat = transform_lat(lng - 105.0, lat - 35.0);
    let mut dlng = transform_lng(lng - 105.0, lat - 35.0);
    let radlat = lat / 180.0 * pi;
    let mut magic = radlat.sin();
    magic = 1.0 - WGS_EE * magic * magic;
    let sqrtmagic = magic.sqrt();
    dlat = (dlat * 180.0) / ((WGS_A * (1.0 - WGS_EE)) / (magic * sqrtmagic) * pi);
    dlng = (dlng * 180.0) / (WGS_A / sqrtmagic * radlat.cos() * pi);
    (lat + dlat, lng + dlng)
}

/// GCJ-02 → BD-09（`wire::bd09_to_gcj02` 的逆变换）。
pub fn gcj02_to_bd09(gcj_lat: f64, gcj_lng: f64) -> (f64, f64) {
    let z = (gcj_lng * gcj_lng + gcj_lat * gcj_lat).sqrt() + 0.00002 * (gcj_lat * X_PI).sin();
    let theta = gcj_lat.atan2(gcj_lng) + 0.000003 * (gcj_lng * X_PI).cos();
    (z * theta.sin() + 0.006, z * theta.cos() + 0.0065)
}

/// WGS84 → BD-09（OSM 路网对齐打卡点用）。
pub fn wgs84_to_bd09(lat: f64, lng: f64) -> (f64, f64) {
    let (g_lat, g_lng) = wgs84_to_gcj02(lat, lng);
    gcj02_to_bd09(g_lat, g_lng)
}

pub fn fmt_gain_time(ms: i64) -> String {
    Local
        .timestamp_millis_opt(ms)
        .single()
        .map(|t| t.format("%Y-%m-%d %H:%M:%S").to_string())
        .unwrap_or_default()
}

/// 体育场形（标准 400m 跑道）参数：中心、旋转角、弯道半径、直道半长。
#[derive(Clone, Copy, Debug)]
pub struct Stadium {
    pub cx: f64,
    pub cy: f64,
    /// 主轴方向（弧度）
    pub rot: f64,
    /// 弯道半径（米）
    pub radius: f64,
    /// 弯道圆心到中心的距离（米），直道总长 = 4×d
    pub d: f64,
}

impl Stadium {
    /// 周长：2 条直道 + 2 个半圆。
    pub fn perimeter(&self) -> f64 {
        4.0 * self.d + 2.0 * std::f64::consts::PI * self.radius
    }

    /// 弧长 s（从北弯道起点起算）→ 局部坐标 (x, y)（主轴系）。
    fn at(&self, s: f64) -> (f64, f64) {
        let total = self.perimeter();
        let mut s = s.rem_euclid(total);
        let (w, d) = (self.radius, self.d);
        let arc = std::f64::consts::PI * w; // 半圆弧长
        // ① 北半圆：从 (+w, +d) 逆时针到 (-w, +d)
        if s <= arc {
            let a = s / w;
            return (w * a.cos(), d + w * a.sin());
        }
        s -= arc;
        // ② 西直道：(-w, +d) → (-w, -d)
        if s <= 2.0 * d {
            return (-w, d - s);
        }
        s -= 2.0 * d;
        // ③ 南半圆：(-w, -d) 逆时针到 (+w, -d)
        if s <= arc {
            let a = std::f64::consts::PI + s / w;
            return (w * a.cos(), -d + w * a.sin());
        }
        s -= arc;
        // ④ 东直道：(+w, -d) → (+w, +d)
        (w, -d + s.min(2.0 * d))
    }

    /// 弧长 → BD 系经纬度。
    pub fn point_at(&self, s: f64) -> (f64, f64) {
        let (lx, ly) = self.at(s);
        let (sn, cs) = self.rot.sin_cos();
        let x = lx * cs - ly * sn;
        let y = lx * sn + ly * cs;
        let m_lng = m_per_deg_lng(self.cy);
        (self.cy + y / MET_PER_DEG_LAT, self.cx + x / m_lng)
    }

    /// 点到跑道线的最近距离（米，局部系）。
    pub fn distance_to(&self, lat: f64, lng: f64) -> f64 {
        let m_lng = m_per_deg_lng(self.cy);
        let x = (lng - self.cx) * m_lng;
        let y = (lat - self.cy) * MET_PER_DEG_LAT;
        let (sn, cs) = self.rot.sin_cos();
        // 反旋转到主轴系
        let lx = x * cs + y * sn;
        let ly = -x * sn + y * cs;
        let (w, d) = (self.radius, self.d);
        let ax = lx.abs();
        let ay = ly.abs();
        if ay <= d {
            (ax - w).abs()
        } else {
            let r = (ax * ax + (ay - d) * (ay - d)).sqrt();
            (r - w).abs()
        }
    }
}

/// 由打卡点拟合体育场形跑道（PCA 主轴 + 极值包络）。
/// `fence_bd` 仅为旧调用接口保留，不参与拟合。稀疏打卡点拟合只是估计，
/// 不代表真实跑道；有实测/描线参考时应优先使用参考。
/// 返回 (stadium, 弧长表, 稠密采样点) —— 采样点为平面米制。
pub fn fit_stadium(
    bd_points: &[(f64, f64)],
    rng: &mut Rng,
    fence_bd: Option<&[(f64, f64)]>,
) -> (Stadium, Vec<f64>, Vec<(f64, f64)>) {
    let n = bd_points.len().max(1) as f64;
    let cx = bd_points.iter().map(|q| q.1).sum::<f64>() / n;
    let cy = bd_points.iter().map(|q| q.0).sum::<f64>() / n;

    // 平面坐标（米）
    let m_lng = m_per_deg_lng(cy);
    let plane: Vec<(f64, f64)> = bd_points
        .iter()
        .map(|q| ((q.1 - cx) * m_lng, (q.0 - cy) * MET_PER_DEG_LAT))
        .collect();

    // PCA 主轴：最大方差方向
    let (mut sxx, mut syy, mut sxy) = (0.0f64, 0.0f64, 0.0f64);
    for &(x, y) in &plane {
        sxx += x * x;
        syy += y * y;
        sxy += x * y;
    }
    // PCA 的最大方差方向原本是局部 x；Stadium 的长轴沿局部 y。
    let rot = 0.5 * (2.0 * sxy).atan2(sxx - syy) - std::f64::consts::FRAC_PI_2;

    // 主轴系包络（鲁棒：中位数半径，兼顾离群点）
    let (sn, cs) = rot.sin_cos();
    let mut lxs: Vec<f64> = Vec::with_capacity(plane.len());
    let mut lys: Vec<f64> = Vec::with_capacity(plane.len());
    for &(x, y) in &plane {
        let lx = x * cs + y * sn;
        let ly = -x * sn + y * cs;
        lxs.push(lx.abs());
        lys.push(ly.abs());
    }
    lxs.sort_by(|a, b| a.partial_cmp(b).unwrap());
    lys.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let med_lx = lxs[lxs.len() / 2];
    let max_ly = *lys.last().unwrap_or(&30.0);
    // 半径 = 打卡点横向中位（跑道中心线穿过点群），外扩 6% 容纳道次；
    // 若个别点横向离群（松散布点），外扩半径保证所有点仍在跑道带 ≤40m 内。
    let mut radius = (med_lx * 1.06).max(18.0);
    for &lx in &lxs {
        if lx > radius + 40.0 {
            radius = lx - 36.0; // 留 4m 余量（<40m 打卡阈值）
        }
    }
    let d = (max_ly - radius).max(6.0);
    // 围栏只是边界，不是跑道测绘数据；不再用猜测比例扩张跑道。
    // 调用方必须分别检查几何包含关系，不能把拟合结果当成真实跑道。
    let _ = fence_bd;
    let stadium = Stadium { cx, cy, rot, radius, d };

    // 稠密采样（每 2m 一点，含轻微径向扰动消除"完美跑道"感）
    let total = stadium.perimeter();
    let steps = (total / 2.0).max(64.0) as usize;
    let mut dense = Vec::with_capacity(steps);
    for i in 0..steps {
        let s = total * i as f64 / steps as f64;
        let (lx, ly) = stadium.at(s);
        // 径向扰动：以 ±0.8m 的高频抖动模拟真实 GPS 沿跑道内外摆动
        let jitter = rng.uniform(-0.8, 0.8);
        let (px, py) = if ly.abs() <= stadium.d {
            // 直道：横向（径向）偏移
            let sign = if lx >= 0.0 { 1.0 } else { -1.0 };
            (lx + sign * jitter, ly)
        } else {
            // 弯道：沿半径方向偏移
            let r = (lx * lx + (ly.abs() - stadium.d) * (ly.abs() - stadium.d)).sqrt();
            let k = (r + jitter) / r.max(1e-6);
            let ny = if ly >= 0.0 { stadium.d } else { -stadium.d };
            (lx * k, ny + (ly - ny) * k)
        };
        // 主轴系 → 平面米制（与 make_point_ring 的 dense 语义一致，由调用方 to_bd 转回度）
        let x = px * cs - py * sn;
        let y = px * sn + py * cs;
        dense.push((x, y));
    }
    let arcs = arc_table(&dense);
    (stadium, arcs, dense)
}

/// 体育场形环（与 make_point_ring 同签名）：拟合打卡点 → 采样点、弧长表和中心。
/// 点位拟合结果不代表已测绘的跑道，也不保证最终轨迹落在围栏内。
pub fn make_stadium_ring(
    bd_points: &[(f64, f64)],
    rng: &mut Rng,
    fence_bd: Option<&[(f64, f64)]>,
) -> PointRing {
    assert!(!bd_points.is_empty(), "拟合跑道需要有效点位");
    let (stadium, arcs, dense) = fit_stadium(bd_points, rng, fence_bd);
    // 覆盖校验：所有打卡点到跑道线 ≤40m 才用体育场形（真实跑道场景成立）；
    // 松散布点（如测试五边形）回退经典打卡点环，保证打卡达标。
    let all_covered = bd_points
        .iter()
        .all(|q| stadium.distance_to(q.0, q.1) <= 40.0);
    if !all_covered {
        return make_point_ring(bd_points);
    }
    // 质心返回 BD 系（与 make_point_ring 的 pc 语义一致，供 to_bd 使用）
    let n = bd_points.len().max(1) as f64;
    let c_lat = bd_points.iter().map(|q| q.0).sum::<f64>() / n;
    let c_lng = bd_points.iter().map(|q| q.1).sum::<f64>() / n;
    (dense, arcs, (c_lat, c_lng))
}

#[cfg(test)]
mod projection_regression_tests {
    use super::*;

    #[test]
    fn point_ring_preserves_east_west_at_jiangmen_latitude() {
        let pts = [(22.4, 112.95), (22.4003, 112.951), (22.3997, 112.949)];
        let (dense, _, (lat, lon)) = make_point_ring(&pts);
        // Catmull-Rom 的每段首点就是原始控制点；前向和逆向投影须相互抵消。
        for i in 0..pts.len() {
            let (x, y) = dense[i * 18];
            let recovered = to_bd(x, y, lat, lon);
            assert!(pts.iter().any(|p| (p.0-recovered.0).abs() < 1e-9 && (p.1-recovered.1).abs() < 1e-9));
        }
    }

    #[test]
    fn stadium_major_axis_matches_long_direction() {
        let lat = 22.4;
        let lon = 112.95;
        let pts: Vec<_> = [(-100.,-30.), (-100.,30.), (100.,-30.), (100.,30.)]
            .iter().map(|&(x,y)| (lat + y / MET_PER_DEG_LAT, lon + x / m_per_deg_lng(lat))).collect();
        let (stadium, _, _) = fit_stadium(&pts, &mut Rng::new(1), None);
        let long_end = stadium.point_at(0.0);
        let opposite = stadium.point_at(stadium.perimeter()/2.0);
        // 东西方向的点集：长轴应沿东西方向，而不是南北。
        let east_west = (long_end.1 - opposite.1).abs() * m_per_deg_lng(lat);
        let north_south = (long_end.0 - opposite.0).abs() * MET_PER_DEG_LAT;
        assert!(east_west > north_south, "东西={east_west}, 南北={north_south}");
    }

    #[test]
    fn distant_fence_does_not_inflate_fit() {
        let pts = [(22.4,112.95), (22.401,112.95), (22.4,112.951), (22.399,112.95)];
        let far = [(22.5,113.05), (22.5,113.06), (22.51,113.06), (22.51,113.05)];
        let (a, _, _) = fit_stadium(&pts, &mut Rng::new(1), None);
        let (b, _, _) = fit_stadium(&pts, &mut Rng::new(1), Some(&far));
        assert_eq!(a.perimeter(), b.perimeter());
    }
}
