//! 已有卫星图描线的跑道。打卡点只用于匹配场地，不用于猜测完整跑道长宽。
//!
//! 地理坐标、瓦片像素和描线来源随数据一起保存；仅匹配到同一场地时使用。
use super::geom::{arc_table, m_per_deg_lng, PointRing, MET_PER_DEG_LAT};
use serde::Deserialize;

#[derive(Deserialize)]
struct Reference {
    id: String,
    crs: String,
    centerline_samples: Vec<(f64, f64)>,
}

pub struct ReferenceRing {
    pub id: String,
    pub ring: PointRing,
}

/// 点到闭合折线的最近距离；输入均为同一米制平面，不把最近顶点当最近线段。
pub fn distance_to_ring(point: (f64, f64), dense: &[(f64, f64)]) -> f64 {
    if dense.len() < 2 {
        return f64::INFINITY;
    }
    (0..dense.len())
        .map(|i| {
            let a = dense[i];
            let b = dense[(i + 1) % dense.len()];
            let (dx, dy) = (b.0 - a.0, b.1 - a.1);
            let len2 = dx * dx + dy * dy;
            let t = if len2 > 0.0 {
                (((point.0 - a.0) * dx + (point.1 - a.1) * dy) / len2).clamp(0.0, 1.0)
            } else {
                0.0
            };
            (point.0 - a.0 - t * dx).hypot(point.1 - a.1 - t * dy)
        })
        .fold(f64::INFINITY, f64::min)
}

fn reference_ring() -> ReferenceRing {
    let reference: Reference =
        serde_json::from_str(include_str!("../../data/jiangmen_stadium.json"))
            .expect("embedded stadium reference must be valid JSON");
    assert_eq!(reference.crs, "gcj02");
    // 描线样本本身就是 GCJ-02；7.4.0 的上传路径不做任何坐标换算
    // （`coordinateWithLatitude:longitude:` 只 alloc+set、`-latitude` 直接返回 ivar），
    // 因此内部体系与上传体系一致，这里不能再转成 BD。
    let bd: Vec<_> = reference.centerline_samples.clone();
    let n = bd.len() as f64;
    let center = (
        bd.iter().map(|q| q.0).sum::<f64>() / n,
        bd.iter().map(|q| q.1).sum::<f64>() / n,
    );
    let scale = m_per_deg_lng(center.0);
    // 保留描线顺序（绝不按质心角重排），在短段内线性加密，不让样条过冲。
    let plane: Vec<_> = bd
        .iter()
        .map(|q| ((q.1 - center.1) * scale, (q.0 - center.0) * MET_PER_DEG_LAT))
        .collect();
    let mut dense = Vec::new();
    for i in 0..plane.len() {
        let a = plane[i];
        let b = plane[(i + 1) % plane.len()];
        let steps = ((b.0 - a.0).hypot(b.1 - a.1) / 1.0).ceil().max(1.0) as usize;
        for j in 0..steps {
            let t = j as f64 / steps as f64;
            dense.push((a.0 + (b.0 - a.0) * t, a.1 + (b.1 - a.1) * t));
        }
    }
    let arcs = arc_table(&dense);
    ReferenceRing {
        id: reference.id,
        ring: (dense, arcs, center),
    }
}

/// 场地描线的原始采样点（GCJ），前提是这块场地的中心离 `center` 不超过 `max_gap_m`。
///
/// 户外跑/自由跑没有打卡点，只能围绕锚点生成轨迹。如果锚点所在场地正好是描过线的
/// 那块跑道，就直接拿描线当控制点 —— 此时 `matching_ring` 必然命中，轨迹会严格贴着
/// 真实跑道，而不是"在场地中心凭空画一个 400m 椭圆"。
pub fn samples_near(center: (f64, f64), max_gap_m: f64) -> Option<Vec<(f64, f64)>> {
    let reference = reference_ring();
    let (dense, _, rc) = reference.ring;
    let scale = m_per_deg_lng(rc.0);
    let dx = (center.1 - rc.1) * scale;
    let dy = (center.0 - rc.0) * MET_PER_DEG_LAT;
    if dx.hypot(dy) > max_gap_m {
        return None;
    }
    let _ = dense;
    Some(reference_samples())
}

fn reference_samples() -> Vec<(f64, f64)> {
    let reference: Reference =
        serde_json::from_str(include_str!("../../data/jiangmen_stadium.json"))
            .expect("embedded stadium reference must be valid JSON");
    reference.centerline_samples
}

/// 至少三个分散点、且每个点距离描线不超过20m，才识别为已知跑道。
/// 其他学校/空输入/混入异地坐标均不套用这条参考线。
pub fn matching_ring(points_bd: &[(f64, f64)]) -> Option<ReferenceRing> {
    if points_bd.len() < 3
        || points_bd
            .iter()
            .any(|q| !q.0.is_finite() || !q.1.is_finite())
    {
        return None;
    }
    let reference = reference_ring();
    let (dense, _, center) = &reference.ring;
    let scale = m_per_deg_lng(center.0);
    let plane: Vec<_> = points_bd
        .iter()
        .map(|q| ((q.1 - center.1) * scale, (q.0 - center.0) * MET_PER_DEG_LAT))
        .collect();
    if plane.iter().any(|&q| distance_to_ring(q, dense) > 20.0) {
        return None;
    }
    let spread = plane.iter().enumerate().any(|(i, a)| {
        plane[i + 1..]
            .iter()
            .any(|b| (a.0 - b.0).hypot(a.1 - b.1) >= 40.0)
    });
    spread.then_some(reference)
}

#[cfg(test)]
mod tests {
    use super::*;

    pub const CHECKPOINTS: [(f64, f64); 5] = [
        (22.411290, 112.940320),
        (22.411343, 112.941001),
        (22.410866, 112.941100),
        (22.410582, 112.941216),
        (22.410823, 112.940376),
    ];

    #[test]
    fn local_points_match_full_outline_not_their_own_envelope() {
        let reference = matching_ring(&CHECKPOINTS).expect("Jiangmen reference");
        let (dense, arcs, _) = reference.ring;
        let xs = dense
            .iter()
            .map(|q| q.0)
            .fold((f64::INFINITY, f64::NEG_INFINITY), |a, x| {
                (a.0.min(x), a.1.max(x))
            });
        let ys = dense
            .iter()
            .map(|q| q.1)
            .fold((f64::INFINITY, f64::NEG_INFINITY), |a, y| {
                (a.0.min(y), a.1.max(y))
            });
        assert!((95.0..105.0).contains(&(xs.1 - xs.0)));
        assert!((150.0..160.0).contains(&(ys.1 - ys.0)));
        assert!((405.0..425.0).contains(arcs.last().unwrap()));
    }

    #[test]
    fn reference_does_not_leak_to_unrelated_or_degenerate_points() {
        assert!(matching_ring(&[]).is_none());
        assert!(matching_ring(&CHECKPOINTS[..2]).is_none());
        assert!(matching_ring(&[CHECKPOINTS[0]; 5]).is_none());
        let mut remote = CHECKPOINTS;
        remote[0].0 += 0.01;
        assert!(matching_ring(&remote).is_none());
        remote[0].0 = f64::NAN;
        assert!(matching_ring(&remote).is_none());
        let other_school: Vec<_> = CHECKPOINTS.iter().map(|q| (q.0 + 1.0, q.1)).collect();
        assert!(matching_ring(&other_school).is_none());
    }

    #[test]
    fn geographic_samples_reproject_to_recorded_satellite_pixels() {
        let value: serde_json::Value =
            serde_json::from_str(include_str!("../../data/jiangmen_stadium.json")).unwrap();
        let samples = value["centerline_samples"].as_array().unwrap();
        let pixels = value["centerline_pixels"].as_array().unwrap();
        let size = 256.0 * 2.0_f64.powi(18);
        for (sample, pixel) in samples.iter().zip(pixels) {
            let lat = sample[0].as_f64().unwrap();
            let lng = sample[1].as_f64().unwrap();
            let x = (lng + 180.0) / 360.0 * size - 213311.0 * 256.0;
            let y = (1.0 - lat.to_radians().tan().asinh() / std::f64::consts::PI) * 0.5 * size
                - 114318.0 * 256.0;
            assert!((x - pixel[0].as_f64().unwrap()).abs() < 0.002);
            assert!((y - pixel[1].as_f64().unwrap()).abs() < 0.002);
        }
    }

    #[test]
    fn segment_distance_includes_closing_edge() {
        let ring = [(0.0, 0.0), (10.0, 0.0), (10.0, 10.0), (0.0, 10.0)];
        assert_eq!(distance_to_ring((0.0, 5.0), &ring), 0.0);
        assert_eq!(distance_to_ring((5.0, 5.0), &ring), 5.0);
    }
}
