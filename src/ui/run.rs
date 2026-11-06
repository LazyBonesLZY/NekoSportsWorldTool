//! 跑步页：恒 sportType=1；距离/配速范围 + 开始时间（随机或指定，最多前 3 天）。
//!
//! 开始时间：「日期」下拉（今天 / 1-3 天前）两种模式共用；随机模式在该日 7:00-20:00
//! 内抽样。**两种模式都硬保证「开始 + 用时 + 抖动」不越过当前时刻**（未来记录会被
//! 服务端判异常），指定时刻被前移时会在运行日志里写明。「换一版」只重掷运动量。

use super::map::MapState;
use super::{mobile, theme, App};
use crate::track::generate_road::RouteMode;
use chrono::{Datelike, Duration, Local, TimeZone, Timelike};
use eframe::egui;

/// 开始时刻的三种语义全部来自 `crate::cli`（CLI 的 `run` 用同一份实现），
/// 避免界面与命令行各写一份公式后再次分叉。
use crate::cli::{RAND_HOUR_HI, RAND_HOUR_LO};
/// 仅测试用于断言「整段跑步不越过当前时刻」。
#[cfg(test)]
use crate::cli::JITTER_MARGIN_MS;

/// 指定日期（days_ago 天前）7:00-20:00 内抽样，且不晚于 `latest_ms`。
fn random_time_ago(days_ago: i64, latest_ms: i64) -> i64 {
    crate::cli::random_start_ms(crate::crypto::envelope::now_ms(), days_ago, latest_ms)
}

/// 指定时刻：days_ago(0=今天) + 时/分；保证整段记录不落在未来。
fn specified_time(days_ago: i64, hour: i64, minute: i64, dur: i64) -> i64 {
    crate::cli::specified_start_ms(
        crate::crypto::envelope::now_ms(),
        days_ago,
        hour,
        minute,
        dur,
    )
}

/// 方案有效期：超过即认为开始时刻已陈旧，下次进页面/提交前重抽。
const PLAN_TTL_MS: i64 = 10 * 60 * 1000;

/// 与 `api::flow` 同源的时长修正：平均速度超出生成器有效窗口时，时长会被改写。
///
/// UI 必须先按修正后的时长钳制开始时刻，否则会出现「界面写 8 分钟、实际提交 16 分钟」，
/// 严重时 stopTime 会落到未来。
fn corrected_dur(dist_m: f64, dur_s: i64) -> i64 {
    if dur_s <= 0 || dist_m <= 0.0 {
        return dur_s;
    }
    let avg = dist_m / dur_s as f64;
    let floor = crate::track::generator::SPEED_FLOOR + 0.1;
    let ceil = crate::track::generator::SPEED_CEIL - 0.1;
    let fixed = avg.clamp(floor, ceil);
    if (fixed - avg).abs() > 1e-6 {
        (dist_m / fixed).round() as i64
    } else {
        dur_s
    }
}

/// 日期下拉的显示文本。
fn days_ago_label(days_ago: i64) -> String {
    match days_ago {
        0 => "今天".into(),
        1 => "昨天".into(),
        n => format!("{n} 天前"),
    }
}

#[derive(Default)]
pub struct RunPage {
    pub dist_min: f32,
    pub dist_max: f32,
    pub pace_min: f32,
    pub pace_max: f32,
    /// 手动绝对海拔（米）；空白表示使用生成器默认海拔。
    pub manual_altitude: String,
    /// 0=随机时刻 1=指定时刻
    pub start_mode: usize,
    pub days_ago: i64,
    pub hour: i64,
    pub minute: i64,
    pub face_check: bool,
    /// 预计算方案：参数变更时重抽样，提交直接使用
    pub plan: Option<RunPlan>,
    /// 路线算法模式
    pub route_mode: RouteMode,
    /// 地图视图状态
    pub map: MapState,
    /// 路网预览（真实道路路由）
    pub preview: Option<crate::track::generate_road::RoadPlan>,
    /// 预览是否过期（参数变更后置真）
    pub preview_stale: bool,
    /// 地图视野是否已适配
    pub map_fitted: bool,
    /// 参数改动后待落盘（拖动中不写盘，松手后一次性写入 config.json）
    pub pending_save: bool,
}

/// 一次提交的确定方案（进入页面/参数变更时抽样生成）。
#[derive(Debug, Clone)]
pub struct RunPlan {
    pub dist_min: f32,
    pub dist_max: f32,
    pub pace_min: f32,
    pub pace_max: f32,
    pub start_mode: usize,
    pub days_ago: i64,
    pub hour: i64,
    pub minute: i64,
    /// 公里
    pub dist: f64,
    /// 秒/km
    pub pace: f32,
    /// 秒
    pub dur: i64,
    pub start_ms: i64,
    /// 本次方案的随机种子（预览与提交共用，保证所见即所得）
    pub seed: u64,
    /// 方案生成时刻（毫秒）：超过 PLAN_TTL_MS 即重抽开始时刻。
    pub created_at: i64,
}

impl RunPage {
    /// 距离/配速是否与方案一致（不一致需重掷运动参数）。
    fn shape_matches(&self, p: &RunPlan) -> bool {
        p.dist_min == self.dist_min
            && p.dist_max == self.dist_max
            && p.pace_min == self.pace_min
            && p.pace_max == self.pace_max
    }

    /// 开始时间控件是否与方案一致。
    ///
    /// 随机模式的时刻是抽样结果（不是输入），只比对日期；指定模式的时/分是用户输入，需回比。
    fn time_matches(&self, p: &RunPlan) -> bool {
        if p.start_mode != self.start_mode || p.days_ago != self.days_ago {
            return false;
        }
        self.start_mode == 0 || (p.hour == self.hour && p.minute == self.minute)
    }

    /// 开跑时刻上限：当前时刻 - 用时 - 抖动余量（flow.rs 提交时还会再加 0-4s）。
    fn latest_start_ms(dur: i64) -> i64 {
        crate::cli::latest_start_ms(crate::crypto::envelope::now_ms(), dur)
    }

    /// 按当前参数抽样运动量（距离 / 配速 / 用时）。
    fn sample_shape(&self) -> (f64, f32, i64) {
        let (lo, hi) = (
            self.dist_min.min(self.dist_max),
            self.dist_min.max(self.dist_max),
        );
        let (plo, phi) = (
            self.pace_min.min(self.pace_max),
            self.pace_min.max(self.pace_max),
        );
        let pace = plo + (phi - plo) * rand::random::<f32>();
        let dist = (lo + (hi - lo) * rand::random::<f32>()) as f64;
        let dur = (dist * pace as f64).round() as i64;
        (dist, pace, dur)
    }

    /// 按当前模式与日期取一个开始时刻（随机模式抽样，指定模式采纳输入框）。
    fn pick_time(&self) -> i64 {
        self.pick_time_for(self.dur_hint())
    }

    /// 指定用时下的开始时刻。
    ///
    /// 重抽运动量时必须传**新**用时：用旧用时钳制会让「用时变长」的方案结束在未来。
    fn pick_time_for(&self, dur: i64) -> i64 {
        if self.start_mode == 0 {
            random_time_ago(self.days_ago, Self::latest_start_ms(dur))
        } else {
            specified_time(self.days_ago, self.hour, self.minute, dur)
        }
    }

    /// 用于随机抽样的时长上限：已抽样的方案优先，否则按当前配速区间上界估一个。
    fn dur_hint(&self) -> i64 {
        match &self.plan {
            Some(p) => p.dur,
            None => {
                let hi = self.dist_min.max(self.dist_max) as f64;
                let phi = self.pace_min.max(self.pace_max) as f64;
                (hi * phi).round() as i64
            }
        }
    }

    /// 运动参数变更时重抽距离/配速/用时，保留已选开始时刻（指定模式的时刻是用户输入，不动）。
    fn regen_shape(&mut self) {
        let (dist, pace, dur) = self.sample_shape();
        let start_ms = match (&self.plan, self.start_mode) {
            // 随机模式沿用已抽样的时刻（仅按新用时收紧上限），避免拖动距离时时刻乱跳
            (Some(p), 0) => p.start_ms.min(Self::latest_start_ms(dur)),
            _ => self.pick_time_for(dur),
        };
        self.write_plan(dist, pace, dur, start_ms);
    }

    /// 只重算开始时刻，保留已抽样的运动量（日期 / 模式变更时用）。
    fn resync_time(&mut self) {
        let start_ms = self.pick_time();
        if let Some(p) = self.plan.as_mut() {
            p.start_mode = self.start_mode;
            p.days_ago = self.days_ago;
            p.hour = self.hour;
            p.minute = self.minute;
            p.start_ms = start_ms;
        }
    }

    /// 把当前参数与已算好的量写成方案。
    fn write_plan(&mut self, dist: f64, pace: f32, dur: i64, start_ms: i64) {
        self.plan = Some(RunPlan {
            dist_min: self.dist_min,
            dist_max: self.dist_max,
            pace_min: self.pace_min,
            pace_max: self.pace_max,
            start_mode: self.start_mode,
            days_ago: self.days_ago,
            hour: self.hour,
            minute: self.minute,
            dist,
            pace,
            dur,
            start_ms,
            seed: rand::random::<u64>(),
            created_at: crate::crypto::envelope::now_ms(),
        });
        self.preview_stale = true;
    }

    /// 「换一版」：重掷运动量；随机模式下同时换一个开始时刻。
    ///
    /// 指定模式的时刻由用户填写，按钮不碰它（用户填什么就是什么）。
    pub fn regen_plan(&mut self) {
        let (dist, pace, dur) = self.sample_shape();
        let start_ms = if self.start_mode == 0 {
            random_time_ago(self.days_ago, Self::latest_start_ms(dur))
        } else {
            specified_time(self.days_ago, self.hour, self.minute, dur)
        };
        self.write_plan(dist, pace, dur, start_ms);
    }

    /// 确保方案与当前参数一致（参数变更补齐 / 提交前兜底）。
    ///
    /// 两个一致性判定都在改写方案「之前」求值：否则 regen_shape 会把当前参数写进方案，
    /// 同帧内「距离 + 日期」一起改时日期变化将被漏掉。
    pub fn ensure_plan(&mut self) {
        let shape_ok = self.plan.as_ref().map(|p| self.shape_matches(p)) == Some(true);
        let time_ok = self.plan.as_ref().map(|p| self.time_matches(p)) == Some(true);
        // 陈旧方案（长开页面）：随机时刻会脱离当前时间，必须重抽。
        let stale = self
            .plan
            .as_ref()
            .map(|p| crate::crypto::envelope::now_ms() - p.created_at > PLAN_TTL_MS)
            .unwrap_or(false);
        if !shape_ok {
            self.regen_shape();
        }
        if !time_ok || stale {
            self.resync_time();
        }
    }

    /// 提交前强制重算开始时刻：随机模式重新抽样，指定模式重新钳制。
    ///
    /// 返回 (旧值, 新值)，便于调用方在日志里说明时刻被动过。
    pub fn refresh_time_for_submit(&mut self) -> Option<(i64, i64)> {
        let before = self.plan.as_ref()?.start_ms;
        self.resync_time();
        if let Some(p) = self.plan.as_mut() {
            p.created_at = crate::crypto::envelope::now_ms();
        }
        let after = self.plan.as_ref().map(|p| p.start_ms)?;
        Some((before, after))
    }

    /// 随机模式的抽样结果是否落在所选日期的 7:00-20:00 窗口内。
    ///
    /// 凌晨选「今天」时窗口尚未到达，`random_time_ago` 会退化到贴近当前时刻，
    /// 此时界面需要说明，避免用户以为刻意选了凌晨。
    fn random_window_ok(&self) -> bool {
        let Some(p) = &self.plan else { return true };
        let Some(t) = Local.timestamp_millis_opt(p.start_ms).single() else {
            return true;
        };
        let day = (Local::now() - Duration::days(self.days_ago.clamp(0, 3))).date_naive();
        t.date_naive() == day && (RAND_HOUR_LO..=RAND_HOUR_HI).contains(&t.hour())
    }
}

impl App {
    pub fn draw_run(&mut self, ui: &mut egui::Ui) {
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                self.draw_run_content(ui);
            });
    }

    fn draw_run_content(&mut self, ui: &mut egui::Ui) {
        let prev_route = self.run_page.route_mode;
        let before = self.run_settings_snapshot();
        {
            let page = &mut self.run_page;
            let compact = mobile::compact_ui(ui);
            let mut draw_distance_inputs = |ui: &mut egui::Ui| {
                mobile::drag_f32(
                    ui,
                    "run_dist_min",
                    &mut page.dist_min,
                    0.5..=20.0,
                    0.05,
                    2,
                    " km",
                );
                ui.label("至");
                mobile::drag_f32(
                    ui,
                    "run_dist_max",
                    &mut page.dist_max,
                    0.5..=20.0,
                    0.05,
                    2,
                    " km",
                );
            };
            if compact {
                ui.label("距离范围（km）：");
                ui.horizontal(draw_distance_inputs);
            } else {
                ui.horizontal(|ui| {
                    ui.label("距离范围（km）：");
                    draw_distance_inputs(ui);
                });
            }
            let mut draw_pace_inputs = |ui: &mut egui::Ui| {
                mobile::drag_f32(
                    ui,
                    "run_pace_min",
                    &mut page.pace_min,
                    180.0..=520.0,
                    5.0,
                    0,
                    "",
                );
                ui.label("至");
                mobile::drag_f32(
                    ui,
                    "run_pace_max",
                    &mut page.pace_max,
                    180.0..=520.0,
                    5.0,
                    0,
                    "",
                );
            };
            if compact {
                ui.label("配速范围（秒/km）：");
                ui.horizontal(draw_pace_inputs);
            } else {
                ui.horizontal(|ui| {
                    ui.label("配速范围（秒/km）：");
                    draw_pace_inputs(ui);
                });
            }
            mobile::row(ui, |ui| {
                ui.label("手动海拔（米/区间）：");
                mobile::text_edit(
                    ui,
                    "run_manual_altitude",
                    &mut page.manual_altitude,
                    crate::platform::InputKind::Text,
                    120.0,
                );
                ui.label("留空自动；可填 17.2 或 11.6-22.8");
            });
            mobile::row(ui, |ui| {
                ui.label("开始时间：");
                ui.radio_value(&mut page.start_mode, 0, "随机时刻");
                ui.radio_value(&mut page.start_mode, 1, "指定时刻");
                ui.label("日期：");
                egui::ComboBox::from_id_salt("run_days_ago")
                    .width(96.0)
                    .selected_text(days_ago_label(page.days_ago))
                    .show_ui(ui, |ui| {
                        for d in 0..=3 {
                            ui.selectable_value(&mut page.days_ago, d, days_ago_label(d));
                        }
                    });
                if page.start_mode == 0 {
                    ui.label(format!("（{RAND_HOUR_LO}:00-{RAND_HOUR_HI}:00 内随机）"));
                } else {
                    ui.label("时刻：");
                    mobile::drag_i64(ui, "run_hour", &mut page.hour, 0..=23, 1.0, "", " 点");
                    ui.label(":");
                    mobile::drag_i64(ui, "run_minute", &mut page.minute, 0..=59, 1.0, "", "");
                }
            });
            mobile::row(ui, |ui| {
                ui.label("人脸校验标记：");
                ui.checkbox(&mut page.face_check, "faceCheck=1");
                ui.colored_label(
                    theme::warn(),
                    "（仅在策略 faceVerify=true 时生效；场地未开启核验时实机发 0）",
                );
            });
            ui.horizontal(|ui| {
                ui.label("路线算法：");
                ui.selectable_value(&mut page.route_mode, RouteMode::Legacy, "经典打卡点环");
                ui.selectable_value(&mut page.route_mode, RouteMode::Road, "真实道路路由");
                if page.route_mode == RouteMode::Road && self.network.is_none() {
                    ui.colored_label(theme::warn(), "（需先在「路网」页导入 OSM）");
                }
            });
        }

        if self.run_settings_snapshot() != before {
            self.run_page.pending_save = true;
        }

        // 切换到真实道路路由时按需拉取电子围栏与实时点位（经典模式不触发这些端点）。
        if self.run_page.route_mode == RouteMode::Road && prev_route != RouteMode::Road {
            self.refresh_fence();
            self.refresh_points();
        }

        ui.add_space(8.0);
        let fmt_pace = |s: f32| format!("{}:{:02}", (s / 60.0) as i64, (s as i64) % 60);
        let fmt_dur = |s: i64| {
            if s >= 3600 {
                format!("{}:{:02}:{:02}", s / 3600, s % 3600 / 60, s % 60)
            } else {
                format!("{}:{:02}", s / 60, s % 60)
            }
        };
        // 参数变更时补方案；显示本次提交的确定方案
        self.run_page.ensure_plan();
        self.draw_run_warnings(ui);
        let plan_label = match &self.run_page.plan {
            Some(p) => {
                let start = chrono::Local
                    .timestamp_millis_opt(p.start_ms)
                    .single()
                    .map(|t| t.format("%Y-%m-%d %H:%M").to_string())
                    .unwrap_or_default();
                let (dist, pace, dur) = (p.dist, p.pace, p.dur);
                format!(
                    "本次方案：距离 {dist:.2} km · 配速 {}/km · 用时 {} · 开始 {start}",
                    fmt_pace(pace),
                    fmt_dur(dur)
                )
            }
            None => String::new(),
        };
        mobile::row(ui, |ui| {
            ui.colored_label(theme::plain(), plan_label);
            if ui.small_button("换一版").clicked() {
                self.run_page.regen_plan();
            }
        });

        self.draw_run_map(ui);

        ui.add_space(8.0);
        // 与 flow 同源的时长修正提示：界面显示的时长和真正提交的可能不同
        if let Some(p) = &self.run_page.plan {
            let dist_m = p.dist * 1000.0;
            let fixed = corrected_dur(dist_m, p.dur);
            if fixed != p.dur {
                ui.colored_label(
                    theme::warn(),
                    format!(
                        "平均配速超出有效窗口，提交时用时会修正为 {}（界面值 {}）",
                        fmt_dur(fixed),
                        fmt_dur(p.dur)
                    ),
                );
            }
        }
        let enabled = !self.submission_busy()
            && self.session.is_some()
            && self.identity_error.is_none();
        let btn = if self.busy(super::JobKind::Run) {
            theme::primary_btn("提交中…")
        } else if self.busy(super::JobKind::AiSubmit) {
            theme::primary_btn("AI 提交中…")
        } else {
            theme::primary_btn("开始跑步")
        };
        mobile::row(ui, |ui| {
            if ui.add_enabled(enabled, btn).clicked() {
                self.start_run();
            }
            if self.session.is_none() {
                ui.label("（请先登录）");
            } else if self.identity_error.is_some() {
                ui.label("（设备身份不可用）");
            } else if self.busy(super::JobKind::AiSubmit) {
                ui.label("（AI 提交进行中，暂不可同时跑步）");
            }
        });

        // 参数改动在指针松开后一次性落盘（拖动过程中不每帧写文件）。
        if self.run_page.pending_save && !ui.ctx().input(|i| i.pointer.any_down()) {
            self.run_page.pending_save = false;
            self.persist_run_settings();
        }
    }

    /// 参与持久化的界面参数快照（用于判断是否有未保存的改动）。
    fn run_settings_snapshot(&self) -> (f32, f32, f32, f32, bool, String, RouteMode) {
        let p = &self.run_page;
        (
            p.dist_min,
            p.dist_max,
            p.pace_min,
            p.pace_max,
            p.face_check,
            p.manual_altitude.clone(),
            p.route_mode,
        )
    }

    /// 把跑步页参数写进 config.json（海拔解析失败时保留旧值，不在打字途中报错）。
    fn persist_run_settings(&mut self) {
        let page = &self.run_page;
        self.config.dist_min = page.dist_min;
        self.config.dist_max = page.dist_max;
        self.config.pace_min = page.pace_min;
        self.config.pace_max = page.pace_max;
        self.config.face_check = page.face_check;
        self.config.route_mode = if page.route_mode == RouteMode::Road {
            "road".into()
        } else {
            "legacy".into()
        };
        // 开始时间设置也要落盘：否则「指定时刻 + 昨天」每次重启都回到 12:00 今天。
        self.config.start_mode = page.start_mode as i64;
        self.config.days_ago = page.days_ago;
        self.config.hour = page.hour;
        self.config.minute = page.minute;
        if let Ok(spec) = crate::track::altitude::parse_spec(&page.manual_altitude) {
            match spec {
                None => {
                    self.config.manual_altitude = None;
                    self.config.manual_altitude_range = None;
                }
                Some(crate::track::altitude::AltitudeSpec::Single(v)) => {
                    self.config.manual_altitude = Some(v);
                    self.config.manual_altitude_range = None;
                }
                Some(crate::track::altitude::AltitudeSpec::Range(r)) => {
                    self.config.manual_altitude = None;
                    self.config.manual_altitude_range = Some(r);
                }
            }
        }
        if let Err(e) = crate::api::model::save_config(&self.config) {
            self.status = format!("× 跑步参数保存失败：{e}");
        }
    }

    /// 开始时间相关的提示。须在 `ensure_plan()` 之后调用，才对得上本次提交时刻。
    fn draw_run_warnings(&self, ui: &mut egui::Ui) {
        let page = &self.run_page;
        if page.start_mode == 0 {
            if !page.random_window_ok() {
                ui.colored_label(
                    theme::warn(),
                    format!(
                        "所选日期的 {RAND_HOUR_LO}:00-{RAND_HOUR_HI}:00 尚未到达，本次只能贴近当前时刻",
                    ),
                );
            }
            return;
        }
        // 今天 + 指定时刻：按「整段记录不落在未来」判定是否会被前移。
        if page.days_ago != 0 {
            return;
        }
        let now = Local::now();
        if let Some(t) = Local
            .with_ymd_and_hms(
                now.year(),
                now.month(),
                now.day(),
                page.hour.clamp(0, 23) as u32,
                page.minute.clamp(0, 59) as u32,
                0,
            )
            .single()
        {
            let dur = page
                .plan
                .as_ref()
                .map(|p| corrected_dur(p.dist * 1000.0, p.dur))
                .unwrap_or_else(|| page.dur_hint());
            let latest = crate::cli::latest_start_ms(crate::crypto::envelope::now_ms(), dur);
            if t.timestamp_millis() > latest {
                ui.colored_label(
                    theme::warn(),
                    format!(
                        "该时刻会让整段记录结束在未来，提交时开始时刻将前移到 {}",
                        fmt_ts(latest)
                    ),
                );
            }
        }
    }

    fn draw_run_map(&mut self, ui: &mut egui::Ui) {
        if self.run_page.route_mode != RouteMode::Road {
            return;
        }
        let Some(net) = self.network.clone() else {
            ui.colored_label(
                theme::warn(),
                "未加载 OSM 路网，无法预览（请先到「路网」页导入）",
            );
            return;
        };
        let Some(plan) = self.run_page.plan.clone() else {
            return;
        };

        if self.run_page.preview_stale {
            self.run_page.preview = None;
            self.run_page.map_fitted = false;
            self.run_page.preview_stale = false;
            // 点位缓存按锚点隔离（main 的串城市防护）；锚点无效时视为无缓存
            let cached = self
                .identity
                .anchor_coordinate()
                .ok()
                .and_then(crate::api::model::load_points_cache_for);
            if let Some((_ts, pts)) = cached {
                let pts_bd = crate::api::points::points_bd(&pts);
                if !pts_bd.is_empty() {
                    let fences = crate::api::model::load_fence_cache().unwrap_or_default();
                    match crate::track::generate_road::plan_road_view(
                        &net,
                        &pts_bd,
                        plan.dist * 1000.0,
                        plan.seed,
                        &fences,
                    ) {
                        Ok(p) => self.run_page.preview = Some(p),
                        Err(e) => self.status = format!("路线预览失败：{e}"),
                    }
                } else {
                    self.status = "无打卡点缓存，提交后可回显轨迹".into();
                }
            } else {
                self.status = "无打卡点缓存，提交后可回显轨迹".into();
            }
        }

        if self.run_page.preview.is_some() {
            super::map::legend(
                ui,
                &[
                    ("道路", egui::Color32::from_rgb(200, 208, 204)),
                    ("建筑", egui::Color32::from_rgb(224, 194, 170)),
                    ("路线", egui::Color32::from_rgb(30, 111, 216)),
                    ("打卡点", egui::Color32::from_rgb(240, 180, 0)),
                    ("起点", egui::Color32::from_rgb(22, 160, 90)),
                    ("终点", egui::Color32::from_rgb(220, 38, 38)),
                ],
            );
        }

        ui.add_space(4.0);
        let rect = ui.available_rect_before_wrap();
        let (response, painter) = ui.allocate_painter(
            egui::Vec2::new(rect.width().max(200.0), 240.0),
            egui::Sense::drag(),
        );
        let canvas = response.rect;
        painter.rect_filled(
            canvas,
            egui::Rounding::ZERO,
            egui::Color32::from_rgb(250, 252, 251),
        );

        // 首次预览后适配视野：优先以电子围栏为中点/范围，无围栏时退化为路线+打卡点+道路
        if !self.run_page.map_fitted {
            let mut bounds: Vec<(f64, f64)> = Vec::new();
            if let Some(p) = &self.run_page.preview {
                for f in &p.fences {
                    bounds.extend(f.iter().copied());
                }
                if bounds.is_empty() {
                    bounds = super::map::collect_bounds(&p.edges);
                    bounds.extend(p.route.iter().copied());
                    bounds.extend(p.checkpoints.iter().copied());
                }
            }
            if !bounds.is_empty() {
                self.run_page.map.fit(canvas, &bounds);
                self.run_page.map_fitted = true;
            }
        }

        if let Some(p) = &self.run_page.preview {
            let road = egui::Color32::from_rgb(200, 208, 204);
            for e in &p.edges {
                self.run_page
                    .map
                    .draw_polyline(&painter, canvas, e, road, 1.0);
            }
            let bld = egui::Color32::from_rgb(224, 194, 170);
            for b in &p.buildings {
                self.run_page
                    .map
                    .draw_polygon(&painter, canvas, b, bld, 1.0);
            }
            let fence_c = egui::Color32::from_rgb(180, 118, 0);
            for f in &p.fences {
                self.run_page
                    .map
                    .draw_polygon(&painter, canvas, f, fence_c, 2.0);
            }
            let route_c = egui::Color32::from_rgb(30, 111, 216);
            self.run_page
                .map
                .draw_polyline(&painter, canvas, &p.route, route_c, 2.5);
            let cp = egui::Color32::from_rgb(240, 180, 0);
            for &(la, lo) in &p.checkpoints {
                self.run_page
                    .map
                    .draw_point(&painter, canvas, la, lo, cp, 3.5);
            }
            if let Some(first) = p.route.first() {
                self.run_page.map.draw_point(
                    &painter,
                    canvas,
                    first.0,
                    first.1,
                    egui::Color32::from_rgb(22, 160, 90),
                    4.5,
                );
            }
            if let Some(last) = p.route.last() {
                self.run_page.map.draw_point(
                    &painter,
                    canvas,
                    last.0,
                    last.1,
                    egui::Color32::from_rgb(220, 38, 38),
                    4.5,
                );
            }
            let label = if p.length_m > 0.0 {
                let loops = plan.dist * 1000.0 / p.length_m;
                if loops > 1.15 {
                    format!(
                        "本次方案 {:.2} km · 单圈 {:.0} m × {:.1} 圈 · {} 打卡点",
                        plan.dist,
                        p.length_m,
                        loops,
                        p.checkpoints.len()
                    )
                } else {
                    format!(
                        "本次方案 {:.2} km · 路线 {:.0} m · {} 打卡点",
                        plan.dist,
                        p.length_m,
                        p.checkpoints.len()
                    )
                }
            } else {
                format!(
                    "本次方案 {:.2} km · {} 打卡点",
                    plan.dist,
                    p.checkpoints.len()
                )
            };
            painter.text(
                egui::Pos2::new(canvas.left() + 8.0, canvas.top() + 8.0),
                egui::Align2::LEFT_TOP,
                label,
                egui::FontId::proportional(12.0),
                egui::Color32::from_rgb(88, 104, 99),
            );
        } else {
            painter.text(
                canvas.center(),
                egui::Align2::CENTER_CENTER,
                "（无预览）",
                egui::FontId::proportional(14.0),
                egui::Color32::from_rgb(150, 160, 156),
            );
        }

        self.run_page.map.interact(ui, canvas);
    }

    fn start_run(&mut self) {
        if let Some(err) = &self.identity_error {
            self.status = format!("设备身份不可用，已阻止提交：{err}");
            return;
        }
        if self.session.is_none() {
            self.status = "请先登录".into();
            return;
        }
        // 提交类任务互斥：跑步与 AI 提交不能同时进行（只读刷新不受影响）。
        if self.submission_busy() {
            self.status = "已有提交任务在进行，请等它结束".into();
            return;
        }
        // 参数变更时补齐方案；提交直接使用预计算值
        self.run_page.ensure_plan();
        let altitude_spec = match crate::track::altitude::parse_spec(&self.run_page.manual_altitude) {
            Ok(spec) => spec,
            Err(e) => {
                self.status = e;
                return;
            }
        };
        let (manual_altitude, manual_altitude_range) = match altitude_spec {
            None => (None, None),
            Some(crate::track::altitude::AltitudeSpec::Single(value)) => (Some(value), None),
            Some(crate::track::altitude::AltitudeSpec::Range(range)) => (None, Some(range)),
        };
        // 提交前重算开始时刻：长开页面不能复用陈旧方案（随机模式重抽、指定模式重钳）。
        let time_change = self.run_page.refresh_time_for_submit();
        let Some(plan) = self.run_page.plan.clone() else {
            self.status = "参数未就绪，请重试".into();
            return;
        };
        let dist = plan.dist * 1000.0; // 米
        if dist <= 0.0 || plan.dur <= 0 {
            self.status = format!(
                "距离/时长无效（dist={dist:.0}m dur={}s），请检查距离与配速范围",
                plan.dur
            );
            return;
        }
        // 与 flow 同源的时长修正：超窗时 flow 会改写 dur，这里先算出来钳时刻。
        let dur = corrected_dur(dist, plan.dur);
        if dur != plan.dur {
            self.log.push(&format!(
                "⚠ 平均配速超出有效窗口，提交用时由 {} 修正为 {}",
                fmt_dur_secs(plan.dur),
                fmt_dur_secs(dur)
            ));
        }
        // 硬钳制：记录结束时刻不得晚于当前时刻（flow 还会再加 0-4s 抖动）。
        let latest = crate::cli::latest_start_ms(crate::crypto::envelope::now_ms(), dur);
        let start_ms = plan.start_ms.min(latest);
        if start_ms != plan.start_ms {
            self.log.push(&format!(
                "⚠ {} 会导致记录结束于未来，已前移到 {}",
                if self.run_page.start_mode == 1 {
                    "指定时刻"
                } else {
                    "抽样时刻"
                },
                fmt_ts(start_ms)
            ));
        } else if let Some((before, after)) = time_change {
            if before != after {
                self.log.push(&format!(
                    "已按当前时间重抽开始时刻：{} → {}",
                    fmt_ts(before),
                    fmt_ts(after)
                ));
            }
        }
        // 让界面上的「本次方案」与实际提交保持一致。
        if let Some(p) = self.run_page.plan.as_mut() {
            p.start_ms = start_ms;
            p.dur = dur;
            p.created_at = crate::crypto::envelope::now_ms();
        }

        let face_check = if self.run_page.face_check { 1 } else { 0 };
        let route_mode = self.run_page.route_mode;
        let page = &self.run_page;
        self.config.dist_min = page.dist_min;
        self.config.dist_max = page.dist_max;
        self.config.pace_min = page.pace_min;
        self.config.pace_max = page.pace_max;
        self.config.face_check = page.face_check;
        self.config.manual_altitude = manual_altitude;
        self.config.manual_altitude_range = manual_altitude_range;
        self.config.route_mode = if route_mode == RouteMode::Road {
            "road".into()
        } else {
            "legacy".into()
        };
        let _ = crate::api::model::save_config(&self.config);

        let identity = self.identity.clone();
        let Some(session) = self.session.clone() else {
            return;
        };
        self.status = "跑步提交中…".into();
        let seed = plan.seed;
        self.spawn_job(super::JobKind::Run, move |tx| {
            let mut log = App::logger(tx.clone());
            let mut client = crate::api::client::ApiClient::new(identity, Some(session));
            let params = crate::api::flow::RunParams {
                run_mode: 1,
                dist,
                dur,
                start_ms,
                face_check,
                manual_altitude,
                manual_altitude_range,
                seed,
                route_mode,
                dry_run: false,
                out_path: None,
            };
            let payload = match crate::api::flow::run_full_flow(&mut client, &params, &mut log) {
                Ok(out) => {
                    log(&format!(
                        "全链完成 rrid={} obs={}/2 verify={} uuid={}",
                        out.result.rrid, out.obs_ok, out.detail_ok, out.result.uuid
                    ));
                    serde_json::json!({
                        "ok": true, "rrid": out.result.rrid,
                        "obs_ok": out.obs_ok, "verify": out.detail_ok,
                        "uuid": out.result.uuid,
                        "dist": out.result.total_dis, "dur": out.result.total_time,
                        "steps": out.result.total_steps, "avg_step_freq": out.result.avg_step_freq,
                        "calorie": out.result.calorie, "avg_power": out.result.avg_power,
                        "sel_distance": out.result.sel_distance, "start": out.result.start_ms,
                    })
                }
                Err(e) => {
                    log(&format!("跑步提交失败: {e}"));
                    serde_json::json!({ "ok": false, "message": e })
                }
            };
            tx.send(format!("__RUN_DONE__{payload}")).ok();
        });
    }
}

/// 毫秒时间戳 → 本地时间（日志用）。
fn fmt_ts(ms: i64) -> String {
    chrono::Local
        .timestamp_millis_opt(ms)
        .single()
        .map(|t| t.format("%Y-%m-%d %H:%M:%S").to_string())
        .unwrap_or_default()
}

/// 秒数 → `H:MM:SS` / `M:SS`（日志用）。
fn fmt_dur_secs(s: i64) -> String {
    if s >= 3600 {
        format!("{}:{:02}:{:02}", s / 3600, s % 3600 / 60, s % 60)
    } else {
        format!("{}:{:02}", s / 60, s % 60)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 一份参数合理的页面状态（配速 6:00/km，距离 2 km 附近）。
    fn page(start_mode: usize, days_ago: i64) -> RunPage {
        RunPage {
            dist_min: 2.0,
            dist_max: 2.2,
            pace_min: 350.0,
            pace_max: 370.0,
            manual_altitude: String::new(),
            start_mode,
            days_ago,
            hour: 12,
            minute: 0,
            face_check: false,
            plan: None,
            ..Default::default()
        }
    }

    /// 开跑时刻本身不得落在未来（两种模式都成立）。
    fn assert_start_not_future(p: &RunPlan) {
        let now = crate::crypto::envelope::now_ms();
        assert!(p.start_ms <= now, "开始时刻 {} 落在未来", p.start_ms);
    }

    /// 随机模式：整段跑步（含 flow.rs 的 0-4s 抖动）都不得越过当前时刻。
    ///
    /// 指定模式不适用：用户指定今天 12:00 而此刻已 12:05 时，原行为就是照常提交。
    fn assert_random_not_future(p: &RunPlan) {
        let now = crate::crypto::envelope::now_ms();
        assert!(
            p.start_ms + p.dur * 1000 + JITTER_MARGIN_MS <= now,
            "随机时刻 {} + 用时 {}s 越过当前时刻",
            p.start_ms,
            p.dur
        );
    }

    /// 硬契约：任何模式下「开始 + 用时 + 抖动」都不得越过当前时刻。
    fn assert_stop_not_future(p: &RunPlan) {
        let now = crate::crypto::envelope::now_ms();
        assert!(
            p.start_ms + p.dur * 1000 + JITTER_MARGIN_MS <= now,
            "记录结束时刻 {} 落在未来（start={} dur={}）",
            p.start_ms + p.dur * 1000,
            p.start_ms,
            p.dur
        );
    }

    /// 指定模式下「换一版」只换运动量，用户填的时刻必须原样保留。
    #[test]
    fn shuffle_keeps_user_time_in_specified_mode() {
        let mut p = page(1, 1);
        p.hour = 9;
        p.minute = 15;
        p.ensure_plan();
        for _ in 0..40 {
            p.regen_plan();
            let plan = p.plan.clone().unwrap();
            assert_eq!((p.hour, p.minute), (9, 15), "「换一版」改动了用户填的时刻");
            assert_eq!((plan.hour, plan.minute), (9, 15), "方案时刻与输入框不一致");
        }
    }

    /// 指定模式下「换一版」仍应换掉距离/配速（否则按钮看起来没反应）。
    #[test]
    fn shuffle_changes_shape_in_specified_mode() {
        let mut p = page(1, 1);
        p.hour = 9;
        p.minute = 15;
        p.ensure_plan();
        let mut seen = std::collections::HashSet::new();
        for _ in 0..40 {
            p.regen_plan();
            seen.insert(p.plan.as_ref().unwrap().dist.to_bits());
        }
        assert!(seen.len() > 1, "「换一版」40 次仍未改变距离");
    }

    /// 日期由用户决定，「换一版」不应改动它。
    #[test]
    fn shuffle_keeps_days_ago_in_specified_mode() {
        let mut p = page(1, 2);
        for _ in 0..20 {
            p.regen_plan();
            assert_eq!(p.days_ago, 2);
            assert_eq!(p.plan.as_ref().unwrap().days_ago, 2);
        }
    }

    /// 随机模式：时刻应落在该日 7:00-20:00 内，且「换一版」每次都换新的。
    #[test]
    fn random_mode_within_window_and_varies() {
        let mut p = page(0, 1);
        let mut seen = std::collections::HashSet::new();
        for _ in 0..40 {
            p.regen_plan();
            let plan = p.plan.clone().unwrap();
            let t = Local.timestamp_millis_opt(plan.start_ms).single().unwrap();
            assert!(
                (RAND_HOUR_LO..=RAND_HOUR_HI).contains(&t.hour()),
                "随机时刻 {}:{} 越出 {RAND_HOUR_LO}:00-{RAND_HOUR_HI}:00",
                t.hour(),
                t.minute()
            );
            seen.insert((t.hour(), t.minute()));
        }
        assert!(seen.len() > 1, "随机模式 40 次仍未改变时刻");
    }

    /// 任何模式下开始时刻都不得落在未来；随机模式还要求整段跑步不越过当前时刻。
    #[test]
    fn never_schedules_future_start() {
        for mode in [0, 1] {
            for days_ago in 0..=3 {
                let mut p = page(mode, days_ago);
                for _ in 0..25 {
                    p.regen_plan();
                    assert_start_not_future(p.plan.as_ref().unwrap());
                    if mode == 0 {
                        assert_random_not_future(p.plan.as_ref().unwrap());
                    }
                }
                // 改参数走 ensure_plan 的路径同样成立
                p.dist_max = 5.0;
                p.pace_max = 500.0;
                p.ensure_plan();
                let plan = p.plan.as_ref().unwrap();
                assert_start_not_future(plan);
                assert_stop_not_future(plan);
                if mode == 0 {
                    assert_random_not_future(plan);
                }
            }
        }
    }

    /// 今天 + 已过去的指定时刻：按原时刻提交，不动它。
    #[test]
    fn specified_today_past_hour_is_kept() {
        let mut p = page(1, 0);
        p.regen_plan();
        // 用户在界面上填时刻（regen_plan 之后填，才是用户输入而非抽样结果）
        p.hour = 0;
        p.minute = 30;
        p.ensure_plan();
        let plan = p.plan.clone().unwrap();
        let today = Local::now().date_naive();
        let want = Local
            .with_ymd_and_hms(today.year(), today.month(), today.day(), 0, 30, 0)
            .single()
            .unwrap()
            .timestamp_millis();
        // 离现在足够远（整段记录早已结束）时必须原样保留
        let now_ms = crate::crypto::envelope::now_ms();
        if want <= crate::cli::latest_start_ms(now_ms, plan.dur) {
            assert_eq!(plan.start_ms, want, "已过去的指定时刻不应被改动");
        }
        assert_stop_not_future(&plan);
    }

    /// 未来的指定时刻必须被钳到「整段记录都在过去」。
    ///
    /// 契约：`min(填入时刻, now - dur - 5s)`（只钳到 now 会让 stopTime 落到未来）。
    #[test]
    fn specified_time_is_clamped_before_now() {
        let now = Local::now();
        let mut p = page(1, 0);
        p.hour = 23;
        p.minute = 59;
        p.regen_plan();
        let plan = p.plan.clone().unwrap();

        let today = now.date_naive();
        let want = Local
            .with_ymd_and_hms(today.year(), today.month(), today.day(), 23, 59, 0)
            .single()
            .unwrap()
            .timestamp_millis();
        let now_ms = crate::crypto::envelope::now_ms();
        assert_eq!(
            plan.start_ms,
            want.min(crate::cli::latest_start_ms(now_ms, plan.dur)),
            "指定时刻未按 min(填入, now-dur-5s) 处理"
        );
        assert_stop_not_future(&plan);
    }

    /// 改距离/配速只重抽运动量，不应打乱用户指定的时刻。
    #[test]
    fn shape_change_keeps_specified_time() {
        let mut p = page(1, 1);
        p.regen_plan();
        // 用户在界面上填时刻
        p.hour = 9;
        p.minute = 15;
        p.ensure_plan();
        let before = p.plan.clone().unwrap();
        assert_eq!((before.hour, before.minute), (9, 15));

        p.dist_min = 3.0;
        p.dist_max = 3.5;
        p.ensure_plan();
        let after = p.plan.clone().unwrap();
        assert_ne!(
            (after.dist, after.pace),
            (before.dist, before.pace),
            "运动量未重抽"
        );
        assert_eq!(after.start_ms, before.start_ms, "改距离不应改动指定时刻");
        assert_eq!((p.hour, p.minute), (9, 15));
    }

    /// 改小时/分钟应立刻反映到方案，且不重抽运动量。
    #[test]
    fn editing_hhmm_resyncs_plan() {
        let mut p = page(1, 1);
        p.regen_plan();
        let before = p.plan.clone().unwrap();

        p.hour = 8;
        p.minute = 45;
        p.ensure_plan();
        let after = p.plan.clone().unwrap();
        assert_eq!((after.hour, after.minute), (8, 45));
        assert_eq!(
            (after.dist, after.pace, after.dur),
            (before.dist, before.pace, before.dur)
        );
        assert_start_not_future(&after);
    }

    /// 切换模式应立即换掉时刻，且不重抽运动量。
    #[test]
    fn mode_switch_resyncs_time_only() {
        let mut p = page(1, 1);
        p.regen_plan();
        let before = p.plan.clone().unwrap();

        p.start_mode = 0;
        p.ensure_plan();
        let after = p.plan.clone().unwrap();
        assert_eq!(after.start_mode, 0);
        assert_eq!(
            (after.dist, after.pace, after.dur),
            (before.dist, before.pace, before.dur)
        );
        assert_random_not_future(&after);
    }

    /// 日期下拉作用于两种模式。
    #[test]
    fn date_change_applies_to_both_modes() {
        for mode in [0, 1] {
            let mut p = page(mode, 0);
            p.regen_plan();
            p.days_ago = 3;
            p.ensure_plan();
            let plan = p.plan.clone().unwrap();
            assert_eq!(plan.days_ago, 3);
            let want = (Local::now() - Duration::days(3)).date_naive();
            let t = Local.timestamp_millis_opt(plan.start_ms).single().unwrap();
            assert_eq!(t.date_naive(), want, "mode={mode} 未落到 3 天前");
        }
    }

    /// 日期下拉文本。
    #[test]
    fn days_ago_labels() {
        assert_eq!(days_ago_label(0), "今天");
        assert_eq!(days_ago_label(1), "昨天");
        assert_eq!(days_ago_label(3), "3 天前");
    }

    /// 同一帧内「距离 + 日期」一起改：两处变化都不能漏。
    #[test]
    fn simultaneous_shape_and_date_change() {
        let mut p = page(0, 0);
        p.regen_plan();
        p.dist_min = 4.0;
        p.dist_max = 4.5;
        p.days_ago = 2;
        p.ensure_plan();
        let plan = p.plan.clone().unwrap();
        assert_eq!(plan.days_ago, 2, "日期变化被漏掉");
        assert!(plan.dist >= 4.0, "距离变化被漏掉");
        let want = (Local::now() - Duration::days(2)).date_naive();
        let t = Local.timestamp_millis_opt(plan.start_ms).single().unwrap();
        assert_eq!(t.date_naive(), want);
        assert_random_not_future(&plan);
    }

    /// 同一帧内「时刻 + 距离」一起改。
    #[test]
    fn simultaneous_shape_and_hhmm_change() {
        let mut p = page(1, 1);
        p.regen_plan();
        p.dist_min = 4.0;
        p.dist_max = 4.5;
        p.hour = 8;
        p.minute = 20;
        p.ensure_plan();
        let plan = p.plan.clone().unwrap();
        assert_eq!((plan.hour, plan.minute), (8, 20), "时刻变化被漏掉");
        assert!(plan.dist >= 4.0, "距离变化被漏掉");
        assert_eq!(plan.start_ms, specified_time(1, 8, 20, plan.dur));
        assert_start_not_future(&plan);
    }

    /// 今天 + 指定时刻且该时刻已过：照常提交，不再回退（用户指定的时刻说了算）。
    #[test]
    fn specified_past_time_is_committed_as_is() {
        let now = Local::now();
        // 取一个今天已过去足够久的整点，保证整段用时都已结束
        let h = now.hour().saturating_sub(3) as i64;
        let mut p = page(1, 0);
        p.hour = h;
        p.minute = 0;
        p.regen_plan();
        let plan = p.plan.clone().unwrap();
        let today = now.date_naive();
        let want = Local
            .with_ymd_and_hms(today.year(), today.month(), today.day(), h as u32, 0, 0)
            .single()
            .unwrap()
            .timestamp_millis();
        assert_eq!(plan.start_ms, want, "用户填的过去时刻被改动了");
        assert_start_not_future(&plan);
    }

    /// 今天的未来指定时刻必然被前移，且整段记录不落在未来。
    #[test]
    fn future_specified_time_is_clamped_and_detectable() {
        let mut p = page(1, 0);
        p.hour = 23;
        p.minute = 59;
        p.ensure_plan();
        let plan = p.plan.clone().unwrap();

        let now = Local::now();
        let today = now.date_naive();
        let typed = Local
            .with_ymd_and_hms(today.year(), today.month(), today.day(), 23, 59, 0)
            .single()
            .unwrap()
            .timestamp_millis();
        let now_ms = crate::crypto::envelope::now_ms();
        assert_eq!(
            plan.start_ms,
            typed.min(crate::cli::latest_start_ms(now_ms, plan.dur))
        );
        assert_stop_not_future(&plan);
        if typed > crate::cli::latest_start_ms(now_ms, plan.dur) {
            assert!(plan.start_ms < typed, "未来时刻未被钳制");
        }
    }

    /// 时长修正必须与 flow 同源：超窗时改写，窗口内不动。
    #[test]
    fn corrected_dur_matches_flow_speed_window() {
        let dist = 1000.0;
        // 10 m/s 远超上界，必须被拉长
        let fixed = corrected_dur(dist, 100);
        assert!(fixed > 100, "超速未被修正");
        assert!(dist / fixed as f64 <= crate::track::generator::SPEED_CEIL);
        // 6:00/km 在窗口内，不得改动
        assert_eq!(corrected_dur(dist, 360), 360);
    }

    /// 反复调用 ensure_plan 应当是幂等的（不改动任何已定值）。
    #[test]
    fn ensure_plan_is_idempotent() {
        for mode in [0, 1] {
            let mut p = page(mode, 1);
            p.regen_plan();
            let first = p.plan.clone().unwrap();
            for _ in 0..5 {
                p.ensure_plan();
            }
            let again = p.plan.clone().unwrap();
            assert_eq!(
                (again.dist, again.pace, again.dur, again.start_ms),
                (first.dist, first.pace, first.dur, first.start_ms),
                "mode={mode} ensure_plan 不稳定"
            );
        }
    }

    /// 随机模式抽样结果应落在所选日期的窗口内；凌晨选「今天」时窗口未到，
    /// 此时允许退化，但必须能被 random_window_ok 识别出来（界面据此提示）。
    #[test]
    fn random_window_detection() {
        let mut p = page(0, 1);
        p.regen_plan();
        assert!(p.random_window_ok(), "昨天的随机时刻应当落在窗口内");

        // 构造一个「今天但早于 7:00」的方案，窗口判定应为 false
        let now = Local::now();
        let early = Local
            .with_ymd_and_hms(now.year(), now.month(), now.day(), 3, 0, 0)
            .single()
            .unwrap()
            .timestamp_millis();
        p.days_ago = 0;
        if let Some(plan) = p.plan.as_mut() {
            plan.days_ago = 0;
            plan.start_ms = early;
        }
        assert!(
            !p.random_window_ok(),
            "今天 03:00 不应被判为在 7:00-20:00 窗口内"
        );
    }
}
