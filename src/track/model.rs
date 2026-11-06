//! 轨迹数据结构，序列化键名与协议逐字段一致。

#![allow(non_snake_case)]

use serde::Serialize;
use crate::location::Coordinate;


/// 10 秒窗距离之和与 `totalDistance` 的容差（米）。
/// 每个窗口按 0.01 m 独立取整，120 个窗口的理论误差上限约 1.2 m；这里取 1.0 m，
/// 与 `generator::ten_window_tests` 的验收口径同一量级。
pub const WINDOW_DISTANCE_TOLERANCE_M: f64 = 1.0;
/// 10 秒窗步数之和与 `totalSteps` 的容差（步）。
pub const WINDOW_STEPS_TOLERANCE: f64 = 1.0;

/// 声明的 `totalDistance` 与折线实长之间允许的偏差（米）。
///
/// 服务端会用下发的 `gLat`/`gLng` 独立重算距离，这两者必须是同一份数据。
/// 直接比「用户目标距离」是错的——road 模式受路网限制，误差到 1~2% 属正常，
/// 那是"没跑够"，不是"协议不一致"。
pub const POLYLINE_DISTANCE_TOLERANCE_M: f64 = 0.5;
/// 点间距离比较的浮点容差（m）。生成器逐点累加四舍五入到 4 位，等值点允许相等。
const TOTAL_DIS_EPSILON_M: f64 = 1e-9;

#[derive(Serialize, Clone, Debug)]
pub struct GenPoint {
    pub id: i64,
    pub flag: i64,
    pub lat: f64,
    pub lng: f64,
    pub gLat: f64,
    pub gLng: f64,
    pub speed: f64,
    pub avgSpeed: f64,
    pub radius: f64,
    pub accuracy: f64,
    #[serde(rename = "type")]
    pub ptype: i64,
    pub locType: i64,
    pub hasAltitude: bool,
    pub totalTime: i64,
    pub totalDis: f64,
    pub validDis: f64,
    pub validTime: i64,
    pub steps: i64,
    pub stepDistance: f64,
    pub gainTime: String,
    pub gainTimeMs: i64,
    pub queueNum: i64,
    pub coorType: String,
    pub bdA: f64,
    pub bdD: f64,
    pub bdS: f64,
    pub bdG: i64,
    pub count: i64,
    pub dtr: f64,
    pub state: i64,
    pub locationId: String,
}

#[derive(Serialize, Clone, Debug)]
pub struct TenWindow {
    pub time: i64,
    pub value: f64,
}

#[derive(Serialize, Clone, Debug)]
pub struct Track {
    pub totalTime: i64,
    pub totalDistance: f64,
    pub validDistance: f64,
    pub validTime: i64,
    pub startTime: i64,
    pub startLatitude: f64,
    pub startLongitude: f64,
    pub locations: Vec<GenPoint>,
    pub totalSteps: i64,
    pub speedPerTenSec: Vec<TenWindow>,
    pub stepsPerTenSec: Vec<TenWindow>,
}

impl Track {
    /// 记录顶层坐标唯一从轨迹首点派生，避免调用方重复传入另一套坐标。
    pub fn start_coordinate(&self) -> Result<Coordinate, String> {
        let first = self.locations.first().ok_or("轨迹不能为空".to_string())?;
        let coordinate = Coordinate::new(self.startLatitude, self.startLongitude, first.accuracy)?;
        let first_coordinate = Coordinate::new(first.gLat, first.gLng, first.accuracy)?;
        if !coordinate.is_near(first_coordinate, 0.0000001) { return Err("轨迹首点与记录顶层坐标不一致".into()); }
        Ok(coordinate)
    }

    /// 每个 10 秒窗的绝对时间边界（毫秒，半开区间 `begin..end`）。
    ///
    /// `wire::build_windows` 直接用这份边界生成 `beginTime/endTime`，
    /// 因此「窗口连续、末窗结束于 `startTime + totalTime*1000`」是构造保证而不是巧合。
    /// 尾窗保留实际秒数（`time` 由累计器给出，不强制 10 秒）。
    pub fn window_bounds_ms(&self) -> Vec<(i64, i64)> {
        let mut lo = 0i64;
        self.speedPerTenSec
            .iter()
            .map(|window| {
                let hi = lo.saturating_add(window.time).min(self.totalTime);
                let bounds = (self.startTime + lo * 1000, self.startTime + hi * 1000);
                lo = hi;
                bounds
            })
            .collect()
    }

    /// 点位折线的实际长度（米）—— 与生成器 `segment_distances` 同一把尺子。
    ///
    /// 必须用 `geom::local_distance_m`（纬度相关的子午圈/卯酉圈换算）。
    /// 用常数 `MET_PER_DEG_LAT = 111132` 会在这里凭空多出 0.36%（江门 22.4°N
    /// 的真值是 110736），于是同一段折线在生成器和校验器里量出两个长度。
    pub fn polyline_length_m(&self) -> f64 {
        self.locations
            .windows(2)
            .map(|pair| {
                let (a, b) = (&pair[0], &pair[1]);
                crate::track::geom::local_distance_m(a.gLat, a.gLng, b.gLat, b.gLng)
            })
            .sum()
    }

    pub fn validate_consistency(&self) -> Result<Coordinate, String> {
        if self.totalTime <= 0 || !self.totalDistance.is_finite() || self.totalDistance <= 0.0 {
            return Err("轨迹时长和距离必须为正数".into());
        }
        if self.speedPerTenSec.len() != self.stepsPerTenSec.len() {
            return Err("距离窗口与步数窗口数量不一致".into());
        }
        let mut window_time = 0i64;
        let mut window_distance = 0.0f64;
        let mut window_steps = 0.0f64;
        for (distance, steps) in self.speedPerTenSec.iter().zip(&self.stepsPerTenSec) {
            if distance.time <= 0 || distance.time > 10 || distance.time != steps.time
                || !distance.value.is_finite() || !steps.value.is_finite()
                || distance.value < 0.0 || steps.value < 0.0
            {
                return Err("距离或步数窗口数据无效".into());
            }
            window_time += distance.time;
            window_distance += distance.value;
            window_steps += steps.value;
        }
        if !self.speedPerTenSec.is_empty() {
            if window_time != self.totalTime {
                return Err("窗口累计时长与轨迹总时长不一致".into());
            }
            // 服务端会用窗口距离反推总距离，两条口径必须收敛。
            if (window_distance - self.totalDistance).abs() > WINDOW_DISTANCE_TOLERANCE_M {
                return Err(format!(
                    "10 秒窗距离之和 {window_distance:.3}m 与总距离 {:.3}m 相差超过 {WINDOW_DISTANCE_TOLERANCE_M}m",
                    self.totalDistance
                ));
            }
            // 步数同理：totalSteps 与 Σ stepsNum 必须是同一份数据。
            if (window_steps - self.totalSteps as f64).abs() > WINDOW_STEPS_TOLERANCE {
                return Err(format!(
                    "10 秒窗步数之和 {window_steps:.1} 与总步数 {} 相差超过 {WINDOW_STEPS_TOLERANCE}",
                    self.totalSteps
                ));
            }
            let expected_end = self
                .startTime
                .saturating_add(self.totalTime.saturating_mul(1000));
            let mut cursor = self.startTime;
            for (index, &(begin, end)) in self.window_bounds_ms().iter().enumerate() {
                if begin != cursor || end <= begin {
                    return Err(format!(
                        "第 {} 个 10 秒窗的时间边界不连续：{begin}..{end}，应为 {cursor}..",
                        index + 1
                    ));
                }
                cursor = end;
            }
            if cursor != expected_end {
                return Err(format!(
                    "末个 10 秒窗结束于 {cursor}，应为 startTime + totalTime*1000 = {expected_end}"
                ));
            }
        }
        // 声明距离必须等于折线实长（服务端按坐标重算的就是这个值）。
        // 单点轨迹没有折线，跳过。
        if self.locations.len() >= 2 {
            let polyline = self.polyline_length_m();
            if !polyline.is_finite() {
                return Err("折线实长非有限值".into());
            }
            if (self.totalDistance - polyline).abs() > POLYLINE_DISTANCE_TOLERANCE_M {
                return Err(format!(
                    "声明距离 {:.3}m 与折线实长 {:.3}m 相差超过 {POLYLINE_DISTANCE_TOLERANCE_M}m",
                    self.totalDistance, polyline
                ));
            }
        }

        // 点位序列：时间、累计距离、累计步数都必须单调不减。
        let mut previous_time = 0;
        let mut previous_distance = 0.0f64;
        let mut previous_steps = 0i64;
        for (index, point) in self.locations.iter().enumerate() {
            Coordinate::new(point.gLat, point.gLng, point.accuracy)?;
            if point.totalTime < previous_time || point.totalTime > self.totalTime {
                return Err("轨迹点时间倒序或超出总时长".into());
            }
            if point.gainTimeMs != self.startTime.saturating_add(point.totalTime.saturating_mul(1000)) {
                return Err("轨迹点绝对时间与累计时间不一致".into());
            }
            if !point.totalDis.is_finite() || point.totalDis < previous_distance - TOTAL_DIS_EPSILON_M {
                return Err(format!(
                    "轨迹点累计距离回退：第 {index} 点 totalDis={} < 前一点 {previous_distance}",
                    point.totalDis
                ));
            }
            if point.steps < previous_steps {
                return Err(format!(
                    "轨迹点累计步数回退：第 {index} 点 steps={} < 前一点 {previous_steps}",
                    point.steps
                ));
            }
            // 服务端能交叉核对的逐点不变量
            if point.validDis != point.totalDis || point.validTime != point.totalTime {
                return Err(format!(
                    "第 {index} 点 validDis/validTime 与 totalDis/totalTime 不一致\
                     （{}/{} vs {}/{}）",
                    point.validDis, point.validTime, point.totalDis, point.totalTime
                ));
            }
            if !(0.0..=65.0).contains(&point.radius) {
                // 65m 是真机 `_isAccuracyValid:` 的硬上限，超过就应标成无效点
                return Err(format!("第 {index} 点 radius={} 超出 [0,65]", point.radius));
            }
            if !point.bdD.is_finite() || !(-1.0..=360.0).contains(&point.bdD) {
                return Err(format!("第 {index} 点航向 bdD={} 越界 [-1,360]", point.bdD));
            }
            if !point.bdS.is_finite() || point.bdS < -1.0 {
                return Err(format!("第 {index} 点 bdS={} 非法", point.bdS));
            }
            previous_time = point.totalTime;
            previous_distance = point.totalDis;
            previous_steps = point.steps;
        }

        // 末点累计量必须等于总量：否则「逐点前缀和」与「顶层声明」是两套数据。
        if let Some(last) = self.locations.last() {
            if (last.totalDis - self.totalDistance).abs() > POLYLINE_DISTANCE_TOLERANCE_M {
                return Err(format!(
                    "末点 totalDis={:.3}m 与声明 totalDistance={:.3}m 不一致",
                    last.totalDis, self.totalDistance
                ));
            }
            if last.totalTime != self.totalTime {
                return Err(format!(
                    "末点 totalTime={} 与声明 totalTime={} 不一致",
                    last.totalTime, self.totalTime
                ));
            }
            if last.steps != self.totalSteps {
                return Err(format!(
                    "末点 steps={} 与声明 totalSteps={} 不一致",
                    last.steps, self.totalSteps
                ));
            }
        }
        self.start_coordinate()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const START: i64 = 1;

    fn point(id: i64, total_time: i64, total_dis: f64, steps: i64) -> GenPoint {
        GenPoint { id, flag: START, lat: -1.0, lng: -1.0, gLat: 39.9 + id as f64 * 1e-4, gLng: 116.4, speed: 1.0, avgSpeed: 1.0, radius: 3.0, accuracy: 3.0, ptype: 0, locType: 1, hasAltitude: true, totalTime: total_time, totalDis: total_dis, validDis: total_dis, validTime: total_time, steps, stepDistance: 0.0, gainTime: String::new(), gainTimeMs: START + total_time * 1000, queueNum: 0, coorType: "gcj02".into(), bdA: 1.0, bdD: 0.0, bdS: 1.0, bdG: 1, count: 1, dtr: 0.0, state: 0, locationId: String::new() }
    }

    /// 10 秒窗齐全、各项自洽的轨迹（窗口口径的基线）。
    fn windowed_track() -> Track {
        let mut track = sample_track();
        track.totalTime = 10;
        track.totalDistance = 100.0;
        track.validDistance = 100.0;
        track.validTime = 10;
        track.totalSteps = 20;
        // 两点间距取约 100m（纬度 1e-3° ≈ 111m），并让 totalDistance 等于折线实长 ——
        // 夹具本身必须满足"声明距离 == 折线实长"这个不变量，否则测的就不是窗口逻辑了。
        let a = point(1, 5, 40.0, 8);
        let mut b = point(2, 10, 100.0, 20);
        b.gLat = a.gLat + 100.0 / crate::track::geom::m_per_deg_lat(a.gLat);
        b.gLng = a.gLng;
        track.locations = vec![a, b];
        track.totalDistance = track.polyline_length_m();
        track.validDistance = track.totalDistance;
        track.startLatitude = track.locations[0].gLat;
        track.startLongitude = track.locations[0].gLng;
        track.speedPerTenSec = vec![TenWindow { time: 10, value: track.totalDistance }];
        track.stepsPerTenSec = vec![TenWindow { time: 10, value: 20.0 }];
        track
    }

    fn sample_track() -> Track {
        let point = GenPoint { id: 1, flag: 1, lat: -1.0, lng: -1.0, gLat: 39.9, gLng: 116.4, speed: 1.0, avgSpeed: 1.0, radius: 3.0, accuracy: 3.0, ptype: 0, locType: 1, hasAltitude: true, totalTime: 1, totalDis: 1.0, validDis: 1.0, validTime: 1, steps: 1, stepDistance: 0.0, gainTime: String::new(), gainTimeMs: 1001, queueNum: 0, coorType: "gcj02".into(), bdA: 1.0, bdD: 0.0, bdS: 1.0, bdG: 1, count: 1, dtr: 0.0, state: 0, locationId: String::new() };
        Track { totalTime: 1, totalDistance: 1.0, validDistance: 1.0, validTime: 1, startTime: 1, startLatitude: 39.9, startLongitude: 116.4, locations: vec![point], totalSteps: 1, speedPerTenSec: vec![], stepsPerTenSec: vec![] }
    }

    #[test] fn top_level_coordinate_is_derived_from_first_point() { assert!(sample_track().validate_consistency().is_ok()); let mut invalid = sample_track(); invalid.startLatitude = 38.9; assert!(invalid.validate_consistency().is_err()); }
    #[test] fn empty_track_is_rejected() { let mut track = sample_track(); track.locations.clear(); assert!(track.validate_consistency().is_err()); }
    #[test]
    fn rejects_inconsistent_times_and_window_counts() {
        let mut track = sample_track();
        track.locations[0].gainTimeMs = 0;
        assert!(track.validate_consistency().is_err());
        track.locations[0].gainTimeMs = 1001;
        track.speedPerTenSec.push(TenWindow { time: 1, value: 1.0 });
        assert!(track.validate_consistency().is_err());
    }

    #[test]
    fn windowed_baseline_is_accepted() {
        assert!(windowed_track().validate_consistency().is_ok());
    }

    /// 护栏：窗口距离之和必须与 totalDistance 收敛（服务端会用两套口径交叉校验）。
    #[test]
    fn rejects_window_distance_diverging_from_total_distance() {
        let mut track = windowed_track();
        track.speedPerTenSec[0].value = 102.0;
        assert!(track.validate_consistency().is_err(), "窗口 102m / 总距 100m 应被拒");
        track.speedPerTenSec[0].value = 100.5;
        assert!(track.validate_consistency().is_ok(), "0.5m 在 1.0m 容差内");
    }

    /// 护栏：totalSteps 与 Σ stepsNum 必须是同一份数据。
    #[test]
    fn rejects_window_steps_diverging_from_total_steps() {
        let mut track = windowed_track();
        track.stepsPerTenSec[0].value = 23.0;
        assert!(track.validate_consistency().is_err(), "窗口 23 步 / 总计 20 步应被拒");
    }

    /// 护栏：点位累计步数不允许回退（哨兵置零造成的 10→0 属于回归）。
    #[test]
    fn rejects_non_monotonic_point_steps_and_distance() {
        let mut track = windowed_track();
        track.locations[1].steps = 5;
        assert!(track.validate_consistency().is_err());
        let mut track = windowed_track();
        track.locations[1].totalDis = 10.0;
        assert!(track.validate_consistency().is_err());
        let mut track = windowed_track();
        track.locations[1].totalTime = 3;
        assert!(track.validate_consistency().is_err());
    }

    /// 护栏：窗口时间轴必须连续且末窗收在 startTime + totalTime*1000。
    #[test]
    fn rejects_broken_window_bounds() {
        let mut track = windowed_track();
        track.speedPerTenSec = vec![TenWindow { time: 5, value: 50.0 }, TenWindow { time: 5, value: 50.0 }];
        track.stepsPerTenSec = vec![TenWindow { time: 5, value: 10.0 }, TenWindow { time: 5, value: 10.0 }];
        assert!(track.validate_consistency().is_ok(), "两段 5 秒窗仍应连续");
        let mut track = windowed_track();
        track.speedPerTenSec[0].time = 8;
        track.stepsPerTenSec[0].time = 8;
        assert!(track.validate_consistency().is_err(), "窗口总时长 8s ≠ totalTime 10s");
    }

    /// 折线实长必须与生成器 `segment_distances` 用同一把尺子（纬度相关的
    /// 子午圈换算），而不是常数 111132。
    #[test]
    fn polyline_length_uses_the_same_metre_scale_as_the_generator() {
        let track = windowed_track();
        let lat = track.locations[0].gLat;
        // `windowed_track()` 现在按 `100.0 / m_per_deg_lat` 摆放两点，
        // 所以实长应与 100.0 一致；同一把尺子意味着差值只在浮点噪声量级。
        let expected = 100.0;
        assert!(
            (track.polyline_length_m() - expected).abs() < 1e-3,
            "{}",
            track.polyline_length_m()
        );
        // 常数 111132 与纬度相关值在 39.9° 处相差约 0.09%，两者确实不同一把尺子。
        let with_const = 100.0 / crate::track::geom::m_per_deg_lat(lat) * 111_132.0;
        assert!((with_const - expected).abs() > 0.01, "两把尺子应当可区分");
        // 与生成器 `segment_distances` 的结果必须逐字节同源。
        let via_helper: f64 = track
            .locations
            .windows(2)
            .map(|p| crate::track::geom::local_distance_m(p[0].gLat, p[0].gLng, p[1].gLat, p[1].gLng))
            .sum();
        assert_eq!(track.polyline_length_m(), via_helper);
    }
}
