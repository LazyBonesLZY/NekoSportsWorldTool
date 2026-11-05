//! 轨迹后处理：
//! 结尾减速、起终点哨兵
//! （索引1 type=5 全零 / 末点 type=6 / 首点 type∈{0,7}）、后半段 type=1/2 散布、
//! 索引2 avgSpeed 边界修正、首点速度取首个非零值。

use super::geom::{fmt_gain_time, round_to, Rng};
use super::model::GenPoint;

pub fn apply_post_fixes(locs: &mut [GenPoint], rng: &mut Rng, start_ms: i64) {
    // 只调整末个正常点的速度；结束时间仍以生成器给出的末点为准。
    if locs.len() >= 4 {
        let mut ci = locs.len() - 1;
        while ci > 0 && locs[ci].ptype == -1 {
            ci -= 1;
        }
        if ci > 1 {
            // 收尾减速仍须落在有效配速窗口内（2.0-2.6 m/s ≈ 7.2-9.4 km/h）
            let stop_ms = round_to(rng.uniform(2.0, 2.6), 4);
            locs[ci].speed = round_to(stop_ms * 3.6, 4);
            locs[ci].avgSpeed = stop_ms;
            locs[ci].bdS = round_to(rng.uniform(0.05, 0.15), 3);
            // totalDis 保持（App 过滤条件 totalDis>0）
        }
    }
    if locs.len() > 2 {
        // 起点哨兵（type=5→setStartPoint）：全零累计 + state=1
        let sp = &mut locs[1];
        sp.ptype = 5;
        sp.state = 1;
        sp.locType = 1;
        sp.radius = round_to(rng.uniform(2.0, 3.5), 2);
        sp.speed = 0.0;
        sp.avgSpeed = 0.0;
        sp.bdS = 0.0;
        sp.totalDis = 0.0;
        sp.validDis = 0.0;
        sp.totalTime = 0;
        sp.validTime = 0;
        sp.steps = 0;
        // 终点哨兵
        let last = locs.len() - 1;
        let e = &mut locs[last];
        e.ptype = 6;
        e.locType = 1;
        e.radius = round_to(rng.uniform(1.5, 3.0), 2);
        // 后半段散布 type=1/2/8（67-99% 行程区间；type=8 真人低频出现）
        let from = (locs.len() as f64 * 0.67) as usize;
        for i in from..locs.len().saturating_sub(2) {
            if rng.random() < 0.06 {
                locs[i].ptype = rng.choice(&[1, 1, 2, 8]);
                locs[i].locType = 1;
            }
        }
    }
    // 边界修正①：索引2 首个真实点 avgSpeed 不跨哨兵计算，并保持在有效窗口内
    if locs.len() > 3 {
        let p2 = &mut locs[2];
        p2.avgSpeed = round_to(p2.totalDis / 1.0f64.max(p2.totalTime as f64), 4).clamp(
            crate::track::generator::SPEED_FLOOR,
            crate::track::generator::SPEED_CEIL,
        );
        p2.speed = round_to(p2.avgSpeed * 3.6, 4);
    }
    // 首点 = 起点：totalTime=0、type∈{0,7}、速度取首个非零值
    if !locs.is_empty() {
        locs[0].totalTime = 0;
        locs[0].validTime = 0;
        locs[0].state = 1;
        locs[0].ptype = rng.choice(&[0, 0, 0, 7]);
        locs[0].locType = 1;
        locs[0].radius = round_to(rng.uniform(1.5, 3.0), 2);
        for j in 1..locs.len() {
            if locs[j].avgSpeed > 0.0 {
                locs[0].avgSpeed = locs[j].avgSpeed;
                locs[0].bdS = locs[j].bdS;
                break;
            }
        }
        if locs[0].speed == 0.0 {
            locs[0].speed = round_to(rng.uniform(7.0, 13.0), 4);
        }
    }

    // 两个生成器都以末点的原始累计时间构造 Track.totalTime。固定这个边界，
    // 而不是将后处理时间向后延伸；同一时间轴还驱动 gainTime 和 gainTimeMs。
    if let Some(last) = locs.last() {
        let end_time = last.totalTime.max(0);
        let mut previous = 0;
        for point in locs.iter_mut() {
            let elapsed = point.totalTime.clamp(previous, end_time);
            point.totalTime = elapsed;
            point.validTime = elapsed;
            point.gainTimeMs = start_ms + elapsed * 1000;
            point.gainTime = fmt_gain_time(point.gainTimeMs);
            previous = elapsed;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::track::{generate_road::build_road, generator::build, model::Track};
    use route_planner::{graph::EdgeData, Coord, RoadGraph};

    const START: i64 = 1_788_958_186_123;

    fn points() -> Vec<(f64, f64)> {
        vec![
            (38.901678, 121.540241),
            (38.902564, 121.541233),
            (38.900921, 121.542310),
            (38.899823, 121.541010),
            (38.900455, 121.539512),
        ]
    }

    fn road_grid() -> RoadGraph {
        let pts = points();
        let clat = pts.iter().map(|p| p.0).sum::<f64>() / pts.len() as f64;
        let clng = pts.iter().map(|p| p.1).sum::<f64>() / pts.len() as f64;
        let mut graph = RoadGraph::new();
        let mut nodes = Vec::new();
        for y in 0..7 {
            for x in 0..7 {
                nodes.push(graph.add_node(Coord::new(
                    clng + (x as f64 - 3.0) * 0.0012,
                    clat + (y as f64 - 3.0) * 0.0012,
                )));
            }
        }
        let mut id = 0;
        for y in 0..7 {
            for x in 0..7 {
                for (dx, dy) in [(1, 0), (0, 1)] {
                    if x + dx >= 7 || y + dy >= 7 {
                        continue;
                    }
                    id += 1;
                    let edge = EdgeData {
                        way_id: id,
                        oneway: false,
                        highway: "residential".into(),
                        maxspeed_kmh: None,
                        length_m: 111.0,
                    };
                    let a = nodes[y * 7 + x];
                    let b = nodes[(y + dy) * 7 + x + dx];
                    graph.add_edge(a, b, edge.clone());
                    graph.add_edge(b, a, edge);
                }
            }
        }
        graph.compute_anchor();
        graph.rebuild_index();
        graph
    }

    fn assert_track_time(track: &Track, expected: i64) {
        assert_eq!(track.totalTime, expected);
        let first = &track.locations[0];
        let last = track.locations.last().unwrap();
        assert_eq!(
            (first.totalTime, first.validTime, first.gainTimeMs),
            (0, 0, START)
        );
        assert_eq!(first.gainTime, fmt_gain_time(START));
        assert_eq!(track.locations[1].totalTime, 0);
        assert_eq!(track.locations[1].gainTimeMs, START);
        assert_eq!(last.totalTime, track.totalTime);
        // stopTime 与提交层的计算方式相同；这里只做本地计算。
        assert_eq!(last.gainTimeMs, track.startTime + track.totalTime * 1000);
        assert_eq!(last.validTime, track.validTime);
        for p in &track.locations {
            assert_eq!(p.validTime, p.totalTime);
            assert_eq!(p.gainTimeMs, START + p.totalTime * 1000);
            assert_eq!(p.gainTime, fmt_gain_time(p.gainTimeMs));
        }
        for pair in track.locations.windows(2) {
            assert!(pair[0].totalTime <= pair[1].totalTime);
            assert!(pair[0].gainTimeMs <= pair[1].gainTimeMs);
        }
    }

    #[test]
    fn legacy_track_times_are_consistent_for_multiple_seeds() {
        for seed in 0..32 {
            let track = build(3300.0, 1220, seed, (38.9, 121.54), START, &points());
            assert_track_time(&track, 1220);
        }
    }

    #[test]
    fn road_track_times_are_consistent_for_multiple_seeds() {
        let net = road_grid();
        let pts = points();
        for seed in 0..4 {
            let track =
                build_road(3300.0, 1220, seed, START, &pts, &pts[1..], &net).expect("road fixture");
            assert_track_time(&track, 1220);
        }
    }

    #[test]
    fn trailing_abnormal_points_and_reversed_times_keep_original_end() {
        let track = build(3300.0, 1220, 42, (38.9, 121.54), START, &points());
        let mut locs = track.locations[..5].to_vec();
        for (point, (time, kind)) in
            locs.iter_mut()
                .zip([(5, 0), (10, 0), (25, 0), (15, -1), (20, -1)])
        {
            point.totalTime = time;
            point.validTime = time;
            point.ptype = kind;
        }
        apply_post_fixes(&mut locs, &mut Rng::new(7), START);
        assert_eq!(
            locs.iter().map(|p| p.totalTime).collect::<Vec<_>>(),
            vec![0, 0, 20, 20, 20]
        );
        assert_eq!(locs.last().unwrap().ptype, 6);
        for p in &locs {
            assert_eq!(p.validTime, p.totalTime);
            assert_eq!(p.gainTimeMs, START + p.totalTime * 1000);
            assert_eq!(p.gainTime, fmt_gain_time(p.gainTimeMs));
        }
    }

    #[test]
    fn empty_and_short_point_lists_are_handled() {
        apply_post_fixes(&mut [], &mut Rng::new(0), START);
        let track = build(3300.0, 1220, 42, (38.9, 121.54), START, &points());
        for len in 1..=3 {
            let mut locs = track.locations[..len].to_vec();
            let end = locs[len - 1].totalTime;
            apply_post_fixes(&mut locs, &mut Rng::new(0), START);
            assert_eq!(locs[0].gainTimeMs, START);
            if len > 1 {
                assert_eq!(locs[len - 1].totalTime, end);
            }
            for pair in locs.windows(2) {
                assert!(pair[0].totalTime <= pair[1].totalTime);
            }
        }
    }
}
