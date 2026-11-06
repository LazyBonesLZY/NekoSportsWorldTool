//! 卡路里 / 功率公式。
//!
//! 提交体 `calorie` 用真机公式：`(long)(总距离km × 体重 × 1.036)`。
//!
//! 依据：`+[SWRunConfigDataManager calorieWithDistance:]` @0x1000a38d0 与
//! `+[SWRunRawData calorieWithDistance:]` @0x1000a04e4 是**两份相同实现**：
//! `(int)(a3 / 1000.0 * userWeight * 1.036)`，即 km × 体重 × 1.036，**截断**取整。
//! 两份同公式同时存在，说明它确实是记录实体 `calorie` 的来源。
//!
//! 说明：用「某 setter 有没有 xref」来断言「属性从未被赋值」是不可靠的——
//! `objc_msgSend` 走 selref 间接调用，IDA 对部分 selector 不产生代码 xref
//! （实测 `setSpeed:` 有 66 个调用点，而确定在用的 `setTotalDis:` 显示 0 个）。
//! 因此这里按公式取值，而不是按「未赋值」取 0。
//!
//! 展示用卡路里是历史详情页本地算的 MET 公式（见 [`display_kcal`]），与上传无关。
//!
//! `avgPower = llround(体重 × 1.1 × 60 / 配速(min/km) / 3.6)`（`_avgPowerWithWeight:`），
//! 与 `1.1 × 体重 × v(m/s)` 代数等价。

/// 提交体 `calorie`：`(long)(km × 体重 × 1.036)`（截断，与 `calorieWithDistance:` 一致）。
pub fn submitted_calorie(weight: f64, total_dis: f64) -> i64 {
    if weight <= 0.0 || total_dis <= 0.0 || !weight.is_finite() || !total_dis.is_finite() {
        return 0;
    }
    (total_dis / 1000.0 * weight * 1.036) as i64
}

/// 展示用卡路里（历史详情页的 MET 公式，**不参与上传**）。
pub fn display_kcal(weight: f64, total_time: i64, total_dis: f64) -> i64 {
    let km = total_dis / 1000.0;
    let mut met = if km <= 0.0 {
        12.0
    } else {
        let pace = (total_time as f64 / 60.0) / km;
        if pace < 10.0 {
            33.0 / pace + 4.1
        } else if pace < 12.0 {
            102.0 / pace - 3.7
        } else {
            33.6 / pace + 1.0
        }
    };
    met = (met * 10.0).round() / 10.0; // 官方按 0.1 粒度取整
    (weight * met * (total_time as f64 / 3600.0)).round() as i64
}

/// 官方兜底功率（瓦）。
pub fn avg_power(weight: f64, total_dis: f64, total_time: i64) -> i64 {
    // `-[SWRunGradeBuilder _avgPowerWithWeight:]` @0x100279e14：
    //   weight < 1e-6 / totalTime <= 0 / totalDis < 1e-6 / pace < 1e-6  → 0
    //   否则 weight * 1.1 * 60 / pace(min/km) / 3.6
    // 其中 pace = sub_1000FEB7C(totalDis, totalTime) = totalTime/60 / (ceil(totalDis*100)/100/1000)。
    // 代数上等价于 1.1*weight*(dis/time)，但配速里那次 ceil 会带来极小差异，
    // 这里照真机走一遍以保持逐位一致。
    if weight < 1e-6 || total_time <= 0 || total_dis < 1e-6 {
        return 0;
    }
    let km = (total_dis * 100.0).ceil() / 100.0 / 1000.0;
    let pace = total_time as f64 / 60.0 / km;
    if pace < 1e-6 {
        return 0;
    }
    (weight * 1.1 * 60.0 / pace / 3.6).round() as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 展示用向量（与上传无关）。
    #[test]
    fn test_display_kcal_vectors() {
        assert_eq!(display_kcal(68.0, 1220, 3300.0), 219);
        assert_eq!(display_kcal(68.0, 600, 1000.0), 74);
    }

    /// avgPower = llround(体重 × 1.1 × 60 / 配速 / 3.6)：1200m/540s/68kg
    /// pace = 9/1.2 = 7.5 min/km → 68*1.1*60/7.5/3.6 = 166.2 → 166
    #[test]
    fn avg_power_matches_ipa_formula() {
        assert_eq!(avg_power(68.0, 1200.0, 540), 166);
        // 体重 0 时必须为 0（真机 guard）
        assert_eq!(avg_power(0.0, 1200.0, 540), 0);
        // 时长/距离非法 → 0
        assert_eq!(avg_power(68.0, 1200.0, 0), 0);
        assert_eq!(avg_power(68.0, 0.0, 540), 0);
    }

    /// 提交体 calorie = (long)(km × 体重 × 1.036)：1200m/68kg → 1.2×68×1.036 = 84.5 → 84（截断）
    #[test]
    fn submitted_calorie_matches_ipa_formula() {
        assert_eq!(submitted_calorie(68.0, 1200.0), 84);
        assert_eq!(submitted_calorie(68.0, 3300.0), 232);
        assert_eq!(submitted_calorie(0.0, 1200.0), 0);
        assert_eq!(submitted_calorie(68.0, 0.0), 0);
        // 截断而不是四舍五入
        assert_eq!(submitted_calorie(70.0, 1000.0), 72); // 70*1.036 = 72.52
    }

    /// 功率向量：round(1.1*68*(3300/1220)) = 202。
    #[test]
    fn test_avg_power_vector() {
        assert_eq!(avg_power(68.0, 3300.0, 1220), 202);
    }
}
