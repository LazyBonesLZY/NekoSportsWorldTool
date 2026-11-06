//! 本地跑步达标判定。
//!
//! 权威来源：`-[SWRunGradeReviewRandomImpl reasonForIncompletion]` @0x10014f078
//! （sportType=1 自由跑用的是 `RandomImpl` 这一支）。
//! App 在提交前用本地判定结果填充 body 的 `complete` / `unCompleteReason`，
//! 服务端随后独立复核；两者不一致会被记为异常记录。
//!
//! 判定顺序（与实机逐条对应，短路返回第一个失败原因）：
//! ```text
//! 1. getRunDuration() < 30                              → 7
//! 2. !didMustPassPointUpToStandard   （无 isFixed&&isPass 的点） → 2
//! 3. !didNormalPointUpToStandard     （!isFixed&&isPass 的点 <= 1）→ 3
//! 4. !isRunDistanceEligible          （totalDis < minDistance）→ 1
//! 5.  isRunDurationEligible          （minRunTime==0 或 >= minRunTime）→ 0
//! 6. 否则                                                → 10
//! ```
//!
//! **注意第 2、3 条的判据是五点实体的标志位，不是轨迹几何**：
//! `didMustPassPointUpToStandard` 是 `fivePoints.contain(^(p){ p.isFixed && p.isPass })`，
//! `didNormalPointUpToStandard` 是 `fivePoints.select(^(p){ !p.isFixed && p.isPass }).count > 1`
//! （谓词块实体在 `sub_10014F24C` / `sub_10014F34C`）。
//! 本工具发出的五点实体统一 `isPass = true`，因此实际变量只有 `isFixed`。

/// 不可上传的原因码（实机 `canUpload == NO`）。
pub const REASON_TOO_SHORT_TO_UPLOAD: i64 = 7;

/// 判定结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GradeReview {
    pub reason: i64,
}

impl GradeReview {
    /// 实机 `< 30s` 的记录不允许上传。
    pub fn can_upload(&self) -> bool {
        self.reason != REASON_TOO_SHORT_TO_UPLOAD
    }
    /// `reason == 0` 才算达标。
    pub fn complete(&self) -> bool {
        self.reason == 0
    }
}

/// 判定输入。
pub struct GradeInput {
    pub total_time: i64,
    pub total_dis: f64,
    /// `runRuleModel.minRunTime`
    pub min_run_time: i64,
    /// `runRuleModel.minDistance`
    pub min_distance: i64,
    /// 是否存在至少一个 `isFixed && isPass` 的五点实体
    /// （对应 `didMustPassPointUpToStandard`）。
    pub must_points_ok: bool,
    /// `!isFixed && isPass` 的五点实体是否多于 1 个
    /// （对应 `didNormalPointUpToStandard` 的 `.count > 1`）。
    pub normal_points_ok: bool,
}

/// 按实机顺序做本地达标判定。
pub fn review(i: &GradeInput) -> GradeReview {
    let reason = if i.total_time < 30 {
        REASON_TOO_SHORT_TO_UPLOAD
    } else if !i.must_points_ok {
        2
    } else if !i.normal_points_ok {
        3
    } else if i.total_dis < i.min_distance as f64 {
        1
    } else if i.total_time < i.min_run_time {
        10
    } else {
        0
    };
    GradeReview { reason }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> GradeInput {
        GradeInput {
            total_time: 540,
            total_dis: 1200.0,
            min_run_time: 300,
            min_distance: 1000,
            must_points_ok: true,
            normal_points_ok: true,
        }
    }

    #[test]
    fn compliant_run_is_complete() {
        let r = review(&base());
        assert_eq!(r.reason, 0);
        assert!(r.complete() && r.can_upload());
    }

    /// 判定顺序：<30s 优先于其他一切（实机短路）。
    #[test]
    fn too_short_wins_over_everything() {
        let i = GradeInput {
            total_time: 20,
            total_dis: 0.0,
            must_points_ok: false,
            normal_points_ok: false,
            ..base()
        };
        let r = review(&i);
        assert_eq!(r.reason, REASON_TOO_SHORT_TO_UPLOAD);
        assert!(!r.can_upload(), "实机 <30s 不允许上传");
    }

    #[test]
    fn reasons_follow_ipa_order() {
        // 必经点失败优先于距离
        let i = GradeInput { must_points_ok: false, total_dis: 10.0, ..base() };
        assert_eq!(review(&i).reason, 2);
        // 普通点失败优先于距离
        let i = GradeInput { normal_points_ok: false, total_dis: 10.0, ..base() };
        assert_eq!(review(&i).reason, 3);
        // 距离不足
        let i = GradeInput { total_dis: 900.0, ..base() };
        assert_eq!(review(&i).reason, 1);
        // 时长不足
        let i = GradeInput { total_time: 200, total_dis: 1200.0, ..base() };
        assert_eq!(review(&i).reason, 10);
    }

    #[test]
    fn boundary_values_match_ipa_comparisons() {
        // 实机用 `<` 比较：恰好等于阈值时达标
        let i = GradeInput { total_time: 300, total_dis: 1000.0, ..base() };
        assert_eq!(review(&i).reason, 0);
        // 29s 触发 7；30s 不触发
        assert_eq!(review(&GradeInput { total_time: 29, ..base() }).reason, 7);
        assert_ne!(review(&GradeInput { total_time: 30, ..base() }).reason, 7);
    }
}
