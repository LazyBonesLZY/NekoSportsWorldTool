//! 室内跑的纯步数轨迹（无 GPS）。
//!
//! 室内记录的提交体只有步数与时长（见 `api::indoor`），但 OBS 侧仍需要
//! 10 秒粒度的步数序列（`_getStepsPerTenSecArrayDataFromRecord:`）。
//! 本模块产出这条序列，并保证：
//!
//! - 窗口数 = `ceil(duration / 10)`；
//! - Σ窗口步数 **精确等于** 声明的 `total_steps`
//!   （服务端会用 `totalSteps` 与 `avgStepFreq` 交叉核对，不能有舍入残差）；
//! - 步频落在室内跑的真实量级内（热身偏低 → 稳态 → 疲劳缓降）。

/// 每窗口秒数。真机 cadence 实体的粒度就是 10 秒。
pub const WINDOW_SECS: i64 = 10;

#[derive(Clone, Debug)]
pub struct IndoorTrack {
    pub total_steps: i64,
    pub total_time: i64,
    /// 每个 10 秒窗口的步数，长度 = ceil(total_time / 10)。
    pub windows: Vec<i64>,
}

impl IndoorTrack {
    /// Σ窗口步数。
    pub fn window_sum(&self) -> i64 {
        self.windows.iter().sum()
    }

    /// 步频（步/分），与 `api::indoor::avg_step_freq` 同口径。
    pub fn avg_step_freq(&self) -> i64 {
        crate::api::indoor::avg_step_freq(self.total_steps, self.total_time)
    }

/// 室内 `step_json` 的元素形态：与室外 `stepsPerTenSec` **完全相同**的 5 键。
///
/// 来源 `-[SWIndoorSportGradeBuilder _getStepsPerTenSecArrayDataFromRecord:]` @0x1002018a4：
/// `beginTime`/`endTime` 是 `numberWithLongLong:`（毫秒），`stepsNum`/`queueNum` 是
/// `numberWithInt:`，`state` 是 `numberWithInteger:`（cadence 实体的 status）。
pub fn build_step_windows(&self, start_ms: i64) -> Vec<serde_json::Value> {
    let end_total = start_ms + self.total_time * 1000;
    self.windows
        .iter()
        .enumerate()
        .map(|(i, &steps)| {
            let begin = start_ms + i as i64 * WINDOW_SECS * 1000;
            let end = (begin + WINDOW_SECS * 1000).min(end_total);
            serde_json::json!({
                "beginTime": begin,
                "endTime": end,
                "stepsNum": steps,
                "queueNum": 0,
                "state": 0,
            })
        })
        .collect()
}

/// 室内 OBS 对象（**3 键**，全部 `gzipBase64` 字符串）。
///
/// 来源 `-[SWIndoorSportGradeBuilder buildObsDataWithRrid:]` @0x100201010：
/// `uuid` = gzip(record.uuid)、`step_json` = gzip(step 数组的 JSON)、`uid` = gzip(record.uid)。
/// 注意**没有** `rrid` 键——与室外 11 键不同。
pub fn build_obs_object(&self, uuid: &str, uid: i64, start_ms: i64) -> serde_json::Value {
    let steps = serde_json::Value::Array(self.build_step_windows(start_ms));
    serde_json::json!({
        "uuid": crate::track::wire::gz_str(uuid),
        "step_json": crate::track::wire::gz_json(&steps),
        "uid": crate::track::wire::gz_str(&uid.to_string()),
    })
}
}

/// 生成室内步数序列。
///
/// - `total_steps` / `duration_s` 必须为正；
/// - `seed` 决定抖动，同 seed 同结果（可复现）。
pub fn generate(total_steps: i64, duration_s: i64, seed: u64) -> Result<IndoorTrack, String> {
    if total_steps <= 0 {
        return Err(format!("室内步数必须为正，收到 {total_steps}"));
    }
    if duration_s <= 0 {
        return Err(format!("室内时长必须为正，收到 {duration_s}"));
    }
    let n = ((duration_s + WINDOW_SECS - 1) / WINDOW_SECS).max(1) as usize;

    // 用与室外同款的确定性 Rng，避免引入新依赖。
    let mut rng = crate::track::geom::Rng::new(seed ^ 0x1D00_0BEE_F00D_1234);

    let base = total_steps as f64 / n as f64;
    let mut w: Vec<f64> = Vec::with_capacity(n);
    for i in 0..n {
        let t = i as f64 / n as f64;
        // 热身：前 15% 逐渐提速；疲劳：后 25% 缓降；再叠加慢波与抖动。
        let ramp = if t < 0.15 { 0.82 + 0.18 * (t / 0.15) } else { 1.0 };
        let fatigue = if t > 0.75 {
            1.0 - 0.12 * ((t - 0.75) / 0.25)
        } else {
            1.0
        };
        let wave = 1.0 + 0.05 * (std::f64::consts::TAU * (i as f64) / 7.3).sin();
        let noise = 1.0 + rng.gauss(0.0, 0.03);
        w.push((base * ramp * fatigue * wave * noise).max(0.0));
    }

    // 精确配平：按最大余数法把整数步数分到各窗口，保证 Σ == total_steps。
    let sum: f64 = w.iter().sum();
    let scale = if sum > 0.0 { total_steps as f64 / sum } else { 1.0 };
    let scaled: Vec<f64> = w.iter().map(|x| x * scale).collect();
    let floors: Vec<i64> = scaled.iter().map(|x| x.floor().max(0.0) as i64).collect();
    let mut remainder = total_steps - floors.iter().sum::<i64>();
    let mut order: Vec<usize> = (0..n).collect();
    order.sort_by(|&a, &b| {
        let fa = scaled[a] - scaled[a].floor();
        let fb = scaled[b] - scaled[b].floor();
        fb.partial_cmp(&fa).unwrap_or(std::cmp::Ordering::Equal)
    });
    let mut windows = floors;
    let mut k = 0usize;
    while remainder > 0 && !order.is_empty() {
        windows[order[k % n]] += 1;
        remainder -= 1;
        k += 1;
    }

    Ok(IndoorTrack {
        total_steps,
        total_time: duration_s,
        windows,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Σ窗口步数必须精确等于声明的 total_steps（服务端会交叉核对）。
    #[test]
    fn window_sum_equals_declared_steps() {
        for &(steps, dur) in &[(2100i64, 1200i64), (1500, 900), (3001, 601), (97, 47)] {
            let t = generate(steps, dur, 42).unwrap();
            assert_eq!(t.window_sum(), steps, "{steps}步/{dur}s 未精确配平");
            assert_eq!(
                t.windows.len() as i64,
                (dur + WINDOW_SECS - 1) / WINDOW_SECS,
                "窗口数应等于 ceil(dur/10)"
            );
            assert!(t.windows.iter().all(|&x| x >= 0), "不得出现负窗口步数");
        }
    }

    /// 步频落在室内跑的真实量级（不是 0，也不离谱）。
    #[test]
    fn cadence_is_plausible() {
        let t = generate(2100, 1200, 7).unwrap();
        let f = t.avg_step_freq();
        assert!((80..=200).contains(&f), "步频 {f} 超出室内跑量级");
    }

    /// 同 seed 可复现；不同 seed 会变。
    #[test]
    fn deterministic_by_seed() {
        let a = generate(2100, 1200, 7).unwrap();
        let b = generate(2100, 1200, 7).unwrap();
        let c = generate(2100, 1200, 8).unwrap();
        assert_eq!(a.windows, b.windows, "同 seed 必须完全一致");
        assert_ne!(a.windows, c.windows, "不同 seed 应有差异");
    }

    /// 形状：开头热身偏低、结尾疲劳缓降，中间高于两端。
    #[test]
    fn has_warmup_and_fatigue_shape() {
        let t = generate(3000, 1200, 11).unwrap();
        let n = t.windows.len();
        let head: f64 = t.windows[..n / 10].iter().sum::<i64>() as f64 / (n / 10) as f64;
        let mid_lo = n * 3 / 10;
        let mid_hi = n * 7 / 10;
        let mid: f64 =
            t.windows[mid_lo..mid_hi].iter().sum::<i64>() as f64 / (mid_hi - mid_lo) as f64;
        let tail: f64 =
            t.windows[n - n / 10..].iter().sum::<i64>() as f64 / (n / 10) as f64;
        assert!(head < mid, "开场应低于中段: head={head:.1} mid={mid:.1}");
        assert!(tail < mid, "收尾应低于中段: tail={tail:.1} mid={mid:.1}");
    }

    /// 室内 OBS 只有 3 键；`step_json` 解压后是 5 键元素的数组，且 ΣstepsNum == totalSteps。
    #[test]
    fn obs_object_has_three_keys_and_consistent_steps() {
        use std::io::Read;
        let t = generate(2400, 1200, 7).unwrap();
        let obj = t.build_obs_object("UUID-X", 17837509, 1_700_000_000_000);
        let keys: Vec<&String> = obj.as_object().unwrap().keys().collect();
        assert_eq!(keys.len(), 3, "室内 OBS 应为 3 键: {keys:?}");
        for k in ["uuid", "step_json", "uid"] {
            assert!(obj[k].is_string(), "{k} 应为 gzipBase64 字符串");
        }
        assert!(!obj.as_object().unwrap().contains_key("rrid"), "室内 OBS 无 rrid");

        let un = |k: &str| -> String {
            let raw = crate::crypto::envelope::b64_decode(obj[k].as_str().unwrap()).unwrap();
            let mut g = flate2::read::GzDecoder::new(&raw[..]);
            let mut s = String::new();
            g.read_to_string(&mut s).unwrap();
            s
        };
        assert_eq!(un("uuid"), "UUID-X");
        assert_eq!(un("uid"), "17837509");

        let arr: Vec<serde_json::Value> = serde_json::from_str(&un("step_json")).unwrap();
        assert_eq!(arr.len(), t.windows.len());
        let mut sum = 0i64;
        for (i, w) in arr.iter().enumerate() {
            let keys: Vec<&String> = w.as_object().unwrap().keys().collect();
            assert_eq!(keys.len(), 5, "step_json 元素应为 5 键: {keys:?}");
            assert!(w["beginTime"].is_i64() || w["beginTime"].is_u64());
            assert!(w["endTime"].is_i64() || w["endTime"].is_u64());
            assert!(w["stepsNum"].is_number());
            assert_eq!(w["queueNum"], 0);
            assert_eq!(w["state"], 0);
            assert_eq!(
                w["beginTime"].as_i64().unwrap(),
                1_700_000_000_000 + i as i64 * 10000,
                "窗口起点应每 10 秒递进"
            );
            sum += w["stepsNum"].as_i64().unwrap();
        }
        assert_eq!(sum, t.total_steps, "step_json 的 ΣstepsNum 必须等于 totalSteps");
        // 末窗终点不得超过记录结束时刻
        let last_end = arr.last().unwrap()["endTime"].as_i64().unwrap();
        assert_eq!(last_end, 1_700_000_000_000 + t.total_time * 1000);
    }

    /// 非法输入明确报错，而不是产出可疑数据。
    #[test]
    fn rejects_non_positive_inputs() {
        assert!(generate(0, 600, 1).is_err());
        assert!(generate(100, 0, 1).is_err());
        assert!(generate(-5, 600, 1).is_err());
    }
}
