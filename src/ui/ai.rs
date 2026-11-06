//! AI 运动页：多项目选择 / 按分钟与按次 / 单次提交 / 批量补签。

use super::{mobile, theme, App, JobKind};
use crate::api::ai::AiMode;
use eframe::egui;

#[derive(Default)]
pub struct AiPage {
    pub list: Vec<crate::api::ai::AiSport>,
    pub selected: usize,
    /// 0=按分钟 1=按次
    pub mode: usize,
    /// 批量补签：天数（1-60）与每天次数
    pub days: i64,
    pub per_day: i64,
    /// 勾选的项目 id
    pub picked: Vec<i64>,
    /// 时长/个数改动后待落盘（拖动中不写盘，松手后一次性写入 config.json）
    pub pending_save: bool,
}

/// 批量计划（确认后执行）。
pub struct AiBatchPlan {
    pub sport_ids: Vec<i64>,
    pub days: i64,
    pub per_day: i64,
    pub mode: AiMode,
}

impl App {
    pub fn draw_ai(&mut self, ui: &mut egui::Ui) {
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| self.draw_ai_content(ui));
    }

    fn draw_ai_content(&mut self, ui: &mut egui::Ui) {
        // 时长/个数直接写 self.config，这里做一次「改动 → 落盘」，否则关掉程序就丢。
        let before = (self.config.ai_minutes, self.config.ai_reps);
        ui.add_space(6.0);

        // 项目多选
        ui.label("项目（可多选）：");
        egui::ScrollArea::horizontal().show(ui, |ui| {
            ui.horizontal(|ui| {
                for s in &self.ai_page.list {
                    let picked = self.ai_page.picked.contains(&s.id);
                    let response = if mobile::compact_ui(ui) {
                        ui.add_sized(
                            [96.0, mobile::TOUCH_HEIGHT],
                            egui::SelectableLabel::new(picked, &s.name),
                        )
                    } else {
                        ui.selectable_label(picked, &s.name)
                    };
                    if response.clicked() {
                        if picked {
                            self.ai_page.picked.retain(|&x| x != s.id);
                        } else {
                            self.ai_page.picked.push(s.id);
                        }
                    }
                }
            });
        });

        ui.add_space(6.0);
        mobile::row(ui, |ui| {
            ui.radio_value(&mut self.ai_page.mode, 0, "按分钟");
            ui.radio_value(&mut self.ai_page.mode, 1, "按次");
        });

        egui::Grid::new("ai_grid").num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
            if self.ai_page.mode == 0 {
                ui.label("时长（分钟）：");
                mobile::drag_i64(ui, "ai_minutes", &mut self.config.ai_minutes, 1..=30, 1.0, "", " 分钟");
                ui.end_row();
            } else {
                ui.label("个数：");
                mobile::drag_i64(ui, "ai_reps", &mut self.config.ai_reps, 5..=1000, 5.0, "", " 个");
                ui.end_row();
            }
            ui.label("补签天数（含今天）：");
            mobile::drag_i64(ui, "ai_days", &mut self.ai_page.days, 1..=60, 1.0, "", " 天");
            ui.end_row();
            ui.label("每天次数：");
            mobile::drag_i64(ui, "ai_per_day", &mut self.ai_page.per_day, 1..=10, 1.0, "", "");
            ui.end_row();
        });
        if before != (self.config.ai_minutes, self.config.ai_reps) {
            self.ai_page.pending_save = true;
        }

        ui.add_space(8.0);
        let logged = self.session.is_some();
        let blocked = self.identity_error.is_some();
        let one_enabled = !self.submission_busy()
            && logged
            && !blocked
            && !self.ai_page.list.is_empty();
        mobile::row(ui, |ui| {
            if ui.add_enabled(one_enabled, theme::primary_btn("提交成绩")).clicked() {
                let sport_id = self
                    .ai_page
                    .list
                    .get(self.ai_page.selected)
                    .map(|s| s.id)
                    .unwrap_or(0);
                self.submit_ai_once(sport_id);
            }
            if self.busy(JobKind::AiSubmit) {
                ui.label("提交中…");
            } else if self.busy(JobKind::Run) {
                ui.label("（跑步提交进行中，暂不可同时提交 AI）");
            } else if blocked {
                ui.label("（设备身份不可用）");
            }
        });

        self.draw_single_selector(ui);

        // ── 批量补签 ────────────────────────────────────────────
        ui.add_space(10.0);
        ui.separator();
        ui.label("批量补签（单线程顺序提交，每天随机时刻）");
        let picked = self.ai_page.picked.clone();
        let days = self.ai_page.days.max(1);
        let per = self.ai_page.per_day.max(1);
        let total = picked.len() as i64 * days * per;
        mobile::row(ui, |ui| {
            let batch_label = format!(
                "批量提交（{}天 × {}次 × {}项 = {} 条）",
                days,
                per,
                picked.len(),
                total
            );
            let btn = theme::primary_btn(&batch_label);
            let can_batch = !self.submission_busy() && logged && !blocked && !picked.is_empty();
            let response = if mobile::compact_ui(ui) {
                ui.add_enabled_ui(can_batch, |ui| {
                    ui.add_sized([ui.available_width(), mobile::TOUCH_HEIGHT], btn)
                }).inner
            } else {
                ui.add_enabled(can_batch, btn)
            };
            if response.clicked() {
                self.ai_confirm = Some(AiBatchPlan {
                    sport_ids: picked,
                    days,
                    per_day: per,
                    mode: self.ai_mode(),
                });
            }
        });

        if self.busy(JobKind::AiSubmit) {
            if let Some(flag) = &self.ai_cancel {
                if !flag.load(std::sync::atomic::Ordering::Relaxed)
                    && ui.button("取消批量补签").clicked()
                {
                    flag.store(true, std::sync::atomic::Ordering::Relaxed);
                    self.status = "正在取消批量补签（当前这一条会跑完）…".into();
                }
            }
        }

        if let Some(plan) = self.ai_confirm.take() {
            let total = plan.sport_ids.len() as i64 * plan.days * plan.per_day;
            let mut confirmed = false;
            let mut cancelled = false;
            egui::Window::new(
                egui::RichText::new("确认批量提交").strong().color(theme::text()),
            )
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .order(egui::Order::Foreground)
                .show(ui.ctx(), |ui| {
                    ui.label(
                        egui::RichText::new(format!(
                            "将顺序提交 {} 条：{} 天 × 每天 {} 次 × {} 个项目",
                            total, plan.days, plan.per_day, plan.sport_ids.len()
                        ))
                        .color(theme::text()),
                    );
                    ui.add_space(6.0);
                    mobile::row(ui, |ui| {
                        if ui.button("取消").clicked() {
                            cancelled = true;
                        }
                        if ui.add(theme::primary_btn("确认提交")).clicked() {
                            confirmed = true;
                        }
                    });
                });
            if confirmed {
                self.run_ai_batch(plan);
            } else if cancelled {
                self.status = "已取消批量提交".into();
            } else {
                self.ai_confirm = Some(plan);
            }
        }

        // 指针松开后一次性落盘：拖动过程中不每帧写文件。
        if self.ai_page.pending_save && !ui.ctx().input(|i| i.pointer.any_down()) {
            self.ai_page.pending_save = false;
            if let Err(e) = crate::api::model::save_config(&self.config) {
                self.status = format!("× AI 参数保存失败：{e}");
            }
        }
    }

    /// 当前提交意图。
    fn ai_mode(&mut self) -> AiMode {
        if self.ai_page.mode == 0 {
            let minutes = self.config.ai_minutes.clamp(1, 30);
            AiMode::Minutes { minutes }
        } else {
            let reps = (self.config.ai_reps.clamp(5, 1000) / 5) * 5;
            AiMode::Count { reps }
        }
    }

    fn submit_ai_once(&mut self, sport_id: i64) {
        if let Some(err) = &self.identity_error {
            self.status = format!("设备身份不可用，已阻止提交：{err}");
            return;
        }
        if self.submission_busy() {
            self.status = "已有提交任务在进行，请等它结束".into();
            return;
        }
        let mode = self.ai_mode();
        let (mode_label, score_label, secs, per_min, consume) = ai_display(mode);
        let identity = self.identity.clone();
        let session = self.session.clone().unwrap_or_default();
        self.status = "AI 成绩提交中…".into();
        self.spawn_job(JobKind::AiSubmit, move |tx| {
            let mut log = App::logger(tx.clone());
            let mut client = crate::api::client::ApiClient::new(identity, Some(session));
            let payload =
                match crate::api::flow::run_ai_submit(&mut client, sport_id, mode, &mut log) {
                    Ok(biz) => {
                        log(&format!("[ai] 响应：{}", truncate_json(&biz)));
                        serde_json::json!({
                            "ok": true,
                            "sport_id": sport_id,
                            "mode": mode_label,
                            "score": score_label,
                            "secs": secs, "per_min": per_min, "consume": consume,
                            "resp_error": biz.get("error").cloned().unwrap_or(serde_json::Value::Null),
                            "resp_msg": biz.get("message").and_then(|m| m.as_str()).unwrap_or(""),
                        })
                    }
                    Err(e) => {
                        log(&format!("× AI 提交失败: {e}"));
                        serde_json::json!({ "ok": false, "message": e })
                    }
                };
            tx.send(format!("__AI_DONE__{payload}")).ok();
        });
    }

    /// 批量补签：单线程顺序提交，过去天数取当天 7:00-21:00 随机时刻。
    fn run_ai_batch(&mut self, plan: AiBatchPlan) {
        if let Some(err) = &self.identity_error {
            self.status = format!("设备身份不可用，已阻止提交：{err}");
            return;
        }
        if self.submission_busy() {
            self.status = "已有提交任务在进行，请等它结束".into();
            return;
        }
        let identity = self.identity.clone();
        let session = self.session.clone().unwrap_or_default();
        let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        self.ai_cancel = Some(cancel.clone());
        self.status = "批量补签进行中…".into();
        self.spawn_job(JobKind::AiSubmit, move |tx| {
            let mut log = App::logger(tx.clone());
            let mut client = crate::api::client::ApiClient::new(identity, Some(session));
            let total = plan.sport_ids.len() as i64 * plan.days * plan.per_day;
            let mut ok = 0i64;
            let mut done = 0i64;
            let mut consecutive_fail = 0i64;
            let mut cancelled = false;
            for day in 0..plan.days {
                if cancelled {
                    break;
                }
                for _ in 0..plan.per_day {
                    if cancelled {
                        break;
                    }
                    for &sport in &plan.sport_ids {
                        if cancel.load(std::sync::atomic::Ordering::Relaxed) {
                            cancelled = true;
                            log("⚠ 已按要求取消批量补签");
                            break;
                        }
                        let at = if day == 0 {
                            None
                        } else {
                            Some(random_time_days_ago(day))
                        };
                        done += 1;
                        let tag = if day == 0 { "今天".to_string() } else { format!("{day} 天前") };
                        match crate::api::ai::upload(&mut client, sport, plan.mode, at) {
                            Ok(biz) if biz.get("error").and_then(|e| e.as_i64()) == Some(10000) => {
                                ok += 1;
                                consecutive_fail = 0;
                                log(&format!("√ [ai] {tag} sport={sport} 成功（{done}/{total}）"));
                            }
                            Ok(biz) => {
                                consecutive_fail += 1;
                                let msg = biz.get("message").and_then(|m| m.as_str()).unwrap_or("");
                                log(&format!("× [ai] {tag} sport={sport} 失败: {msg}（{done}/{total}）"));
                            }
                            Err(e) => {
                                consecutive_fail += 1;
                                log(&format!("× [ai] {tag} sport={sport} 失败: {e}（{done}/{total}）"));
                            }
                        }
                        if consecutive_fail >= 5 {
                            log("× [ai] 连续 5 次失败（可能会话失效或触发风控），已中止批量补签");
                            cancelled = true;
                            break;
                        }
                        std::thread::sleep(std::time::Duration::from_millis(1200));
                    }
                }
            }
            let payload = serde_json::json!({
                "ok": true, "batch": true,
                "success": ok, "total": total,
                "days": plan.days, "per_day": plan.per_day,
                "sports": plan.sport_ids.len(),
                "cancelled": cancelled,
                "done": done,
            });
            tx.send(format!("__AI_DONE__{payload}")).ok();
        });
    }

    fn draw_single_selector(&mut self, ui: &mut egui::Ui) {
        let names: Vec<String> = self
            .ai_page
            .list
            .iter()
            .map(|s| format!("id={}  {}", s.id, s.name))
            .collect();
        mobile::row(ui, |ui| {
            ui.label("单发项目：");
            let sel_text = names
                .get(self.ai_page.selected)
                .cloned()
                .unwrap_or_else(|| "-".into());
            egui::ComboBox::from_id_salt("ai_combo")
                .selected_text(sel_text)
                .show_ui(ui, |ui| {
                    for (i, name) in names.iter().enumerate() {
                        ui.selectable_value(&mut self.ai_page.selected, i, name.clone());
                    }
                });
        });
    }
}

fn ai_display(mode: AiMode) -> (&'static str, String, f64, i64, f64) {
    // 展示预估：与提交侧同源参数（计次 90 个/分，计时 240 毫秒/个）
    match mode {
        AiMode::Minutes { minutes } => (
            "按分钟",
            format!("{minutes} 分钟"),
            (minutes * 60) as f64,
            (90.0 * minutes as f64).round() as i64,
            (minutes * 60) as f64 * 0.2,
        ),
        AiMode::Count { reps } => {
            let secs = ((reps as f64 / 90.0) * 60.0).round().max(30.0);
            (
                "按次",
                format!("{reps} 个"),
                secs,
                (reps as f64 / secs * 60.0).round() as i64,
                reps as f64 * 0.07,
            )
        }
    }
}

/// days_ago 天前的随机时刻（7:00-21:00）。
fn random_time_days_ago(days_ago: i64) -> i64 {
    use chrono::{Datelike, Duration, Local, TimeZone};
    let base = Local::now() - Duration::days(days_ago);
    let h = 7 + rand::random::<u32>() % 15;
    Local
        .with_ymd_and_hms(base.year(), base.month(), base.day(), h, rand::random::<u32>() % 60, rand::random::<u32>() % 60)
        .single()
        .map(|t| t.timestamp_millis())
        .unwrap_or_else(crate::crypto::envelope::now_ms)
}

fn truncate_json(v: &serde_json::Value) -> String {
    let s = v.to_string();
    s.chars().take(200).collect()
}
