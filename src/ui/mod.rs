//! UI 层：App 状态机 + 顶栏 + 标签页 + 底部状态栏。
//!
//! 后台任务与消息处理在独立 impl App 块中。
//!
//! 线程模型：所有网络/长任务 spawn thread + mpsc channel<String> 回 UI，
//! 带 200ms 轮询重绘。带内消息协议（除进度/日志外一律是 `{"ok":bool,...}`）：
//! __IP_DONE__ / __LOGIN_DONE__ / __LOGOUT_DONE__ / __RUN_DONE__ / __AI_DONE__ /
//! __RECORDS__ / __AI_LIST__ / __SEMESTER__ / __CHEAT__ / __RANK__ / __USER__ /
//! __AI_RECORDS__ / __FENCE_DONE__ / __POINTS_DONE__
//!
//! 忙标志按任务类型记账（`in_flight`），每个任务只清自己那一项；线程 panic 由
//! `spawn_job` 的 `catch_unwind` 兜底发一条 `ok=false`，另有超时第二道保险。

pub mod about;
pub mod ai;
pub mod data;
pub mod device;
pub mod fonts;
pub mod jobs;
pub mod log;
pub mod map;
pub mod mobile;
pub mod msgs;
pub mod osm;
pub mod records;
pub mod run;
pub mod theme;
pub mod user;

use crate::api::model::{self, Config, HeaderIdentity, Session};
use eframe::egui;
use std::collections::HashMap;
use std::sync::mpsc::{Receiver, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

pub const IP: &str = "__IP_DONE__";
pub const LOGIN_DONE: &str = "__LOGIN_DONE__";
pub const LOGOUT_DONE: &str = "__LOGOUT_DONE__";
pub const RUN_DONE: &str = "__RUN_DONE__";
pub const AI_DONE: &str = "__AI_DONE__";
pub const RECORDS: &str = "__RECORDS__";
pub const AI_LIST: &str = "__AI_LIST__";
pub const AI_DETAIL: &str = "__AI_DETAIL__";
pub const SEMESTER: &str = "__SEMESTER__";
pub const CHEAT: &str = "__CHEAT__";
pub const RANK: &str = "__RANK__";
pub const USER: &str = "__USER__";
pub const AI_RECORDS: &str = "__AI_RECORDS__";
pub const RUN_DETAIL: &str = "__RUN_DETAIL__";
pub const UPDATE_CHK: &str = "__UPDATE_CHK__";
pub const UPDATE_PROG: &str = "__UPDATE_PROG__";
pub const UPDATE_DONE: &str = "__UPDATE_DONE__";
pub const FENCE_DONE: &str = "__FENCE_DONE__";
pub const POINTS_DONE: &str = "__POINTS_DONE__";

/// 提交结果弹窗。
pub struct PopupInfo {
    pub title: String,
    pub lines: Vec<String>,
}

/// 后台任务类型：忙标志、超时兜底与消息回收都以它为单位。
///
/// 旧实现用 6 个全局 bool，导致 `AI_LIST` 会清掉正在提交的 AI、`SEMESTER` 会
/// 提前放行排行榜查询。按任务类型记账后，每个任务只清自己那一项。
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum JobKind {
    Login,
    Logout,
    Ip,
    Run,
    AiSubmit,
    AiList,
    AiRecords,
    Records,
    RunDetail,
    AiDetail,
    User,
    DataSemester,
    DataCheat,
    Rank,
    Fence,
    Points,
    UpdateCheck,
    UpdateDownload,
}

impl JobKind {
    /// 该任务对应的完成消息前缀（panic 兜底也用它解锁 UI）。
    pub fn done_prefix(self) -> &'static str {
        match self {
            JobKind::Login => LOGIN_DONE,
            JobKind::Logout => LOGOUT_DONE,
            JobKind::Ip => IP,
            JobKind::Run => RUN_DONE,
            JobKind::AiSubmit => AI_DONE,
            JobKind::AiList => AI_LIST,
            JobKind::AiRecords => AI_RECORDS,
            JobKind::Records => RECORDS,
            JobKind::RunDetail => RUN_DETAIL,
            JobKind::AiDetail => AI_DETAIL,
            JobKind::User => USER,
            JobKind::DataSemester => SEMESTER,
            JobKind::DataCheat => CHEAT,
            JobKind::Rank => RANK,
            JobKind::Fence => FENCE_DONE,
            JobKind::Points => POINTS_DONE,
            JobKind::UpdateCheck => UPDATE_CHK,
            JobKind::UpdateDownload => UPDATE_DONE,
        }
    }

    /// 无响应超时：超过即解除界面锁定（线程卡死/panic 未捕获时的第二道保险）。
    pub fn timeout(self) -> Duration {
        match self {
            JobKind::Login | JobKind::AiSubmit | JobKind::UpdateDownload => Duration::from_secs(300),
            JobKind::Run => Duration::from_secs(240),
            JobKind::UpdateCheck | JobKind::Ip => Duration::from_secs(60),
            _ => Duration::from_secs(180),
        }
    }

    /// 界面文案用的任务名。
    pub fn label(self) -> &'static str {
        match self {
            JobKind::Login => "登录",
            JobKind::Logout => "登出",
            JobKind::Ip => "公网 IP 获取",
            JobKind::Run => "跑步提交",
            JobKind::AiSubmit => "AI 提交",
            JobKind::AiList => "AI 项目列表",
            JobKind::AiRecords => "AI 记录",
            JobKind::Records => "跑步记录",
            JobKind::RunDetail => "跑步详情",
            JobKind::AiDetail => "AI 记录详情",
            JobKind::User => "我的页数据",
            JobKind::DataSemester => "学期完成度",
            JobKind::DataCheat => "违规自查",
            JobKind::Rank => "排行榜",
            JobKind::Fence => "电子围栏",
            JobKind::Points => "实时点位",
            JobKind::UpdateCheck => "检查更新",
            JobKind::UpdateDownload => "下载更新",
        }
    }

    /// 更新前需要等待的「前台」任务（纯后台拉取不挡更新）。
    pub fn blocks_update(self) -> bool {
        !matches!(
            self,
            JobKind::Ip
                | JobKind::Fence
                | JobKind::Points
                | JobKind::UpdateCheck
                | JobKind::UpdateDownload
        )
    }
}

/// 顶层 App。
pub struct App {
    pub tx: Sender<String>,
    pub rx: Receiver<String>,
    pub log: log::LogStore,
    pub font_loaded: Option<String>,

    pub ip: String,
    pub identity: HeaderIdentity,
    pub device_buf: HeaderIdentity,
    pub session: Option<Session>,
    pub config: Config,
    /// 设备身份读取失败的原因；有值时禁用登录/提交，只允许「备份并重建身份」。
    pub identity_error: Option<String>,

    pub username: String,
    pub password: String,
    pub remember: bool,
    /// 按任务类型的在跑任务（值是开始时刻，供超时兜底）。
    pub in_flight: HashMap<JobKind, Instant>,
    pub status: String,
    pub popup: Option<PopupInfo>,
    pub ai_confirm: Option<ai::AiBatchPlan>,
    /// 批量补签的取消位（`None` = 没有批量任务在跑）。
    pub ai_cancel: Option<Arc<std::sync::atomic::AtomicBool>>,

    pub tab: usize,
    pub run_page: run::RunPage,
    pub ai_page: ai::AiPage,
    pub records_page: records::RecordsPage,
    pub data_page: data::DataPage,
    pub user_page: user::UserPage,
    pub device_page: device::DevicePage,
    pub update: about::UpdateUi,

    /// 已加载的 OSM 路网（真实道路路由 + 地图显示共用）。
    pub network: Option<Arc<route_planner::RoadGraph>>,
    pub osm_page: osm::OsmPage,
    net_tx: Sender<Result<Arc<route_planner::RoadGraph>, String>>,
    net_rx: Receiver<Result<Arc<route_planner::RoadGraph>, String>>,
}

impl eframe::App for App {
    #[cfg(target_os = "android")]
    fn clear_color(&self, visuals: &egui::Visuals) -> [f32; 4] {
        visuals.panel_fill.to_normalized_gamma_f32()
    }

    fn raw_input_hook(&mut self, ctx: &egui::Context, input: &mut egui::RawInput) {
        crate::platform::apply_safe_area(ctx, input);
    }

    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_messages();
        // 线程卡死/未捕获异常的第二道保险：超时即解锁界面。
        self.expire_stale_jobs();
        crate::platform::set_keep_screen_on(self.submission_busy() || self.busy(JobKind::Login));
        ctx.request_repaint_after(std::time::Duration::from_millis(200));

        // ── 顶栏 ────────────────────────────────────────────────
        egui::TopBottomPanel::top("top_bar").show(ctx, |ui| {
            ui.add_space(6.0);
            let compact = mobile::compact_ui(ui);
            let logged = self.session.is_some();
            let btn_text = if logged { "登出" } else { "登录" };
            let id_ok = self.identity_error.is_none();
            let mut login_clicked = false;
            let mut rebuild_clicked = false;
            // 设备身份读取失败：硬阻断登录与提交，只留「备份并重建身份」。
            if let Some(err) = self.identity_error.clone() {
                ui.horizontal_wrapped(|ui| {
                    ui.colored_label(theme::err(), format!("× 设备身份读取失败：{err}"));
                    if ui
                        .add_enabled(
                            !self.busy(JobKind::Login),
                            egui::Button::new("备份并重建身份"),
                        )
                        .clicked()
                    {
                        rebuild_clicked = true;
                    }
                });
                ui.colored_label(
                    theme::warn(),
                    "已停止一切网络操作：继续用空 DeviceId 请求会让服务端判定设备异常（10121）。",
                );
            }
            let draw_connection = |ui: &mut egui::Ui| {
                ui.label("IP：");
                if self.ip.is_empty() {
                    ui.colored_label(theme::warn(), "获取中…");
                } else if self.ip == "失败" {
                    ui.colored_label(theme::err(), "失败");
                } else {
                    ui.monospace(&self.ip);
                }
                match &self.session {
                    Some(s) => {
                        ui.colored_label(theme::ok(), format!("● {}（uid={}）", s.name, s.uid));
                    }
                    None => {
                        ui.colored_label(theme::err(), "● 未登录");
                    }
                }
            };
            if compact {
                ui.horizontal_wrapped(draw_connection);
                ui.label(format!("设备：{}", device_text(&self.identity.device_id)));
                ui.add_space(4.0);
                ui.label("账号：");
                mobile::text_edit(
                    ui,
                    "login_username",
                    &mut self.username,
                    crate::platform::InputKind::Text,
                    ui.available_width(),
                );
                ui.label("密码：");
                mobile::text_edit(
                    ui,
                    "login_password",
                    &mut self.password,
                    crate::platform::InputKind::Password,
                    ui.available_width(),
                );
                ui.horizontal_wrapped(|ui| {
                    ui.checkbox(&mut self.remember, "记住密码");
                    login_clicked = ui
                        .add_enabled(
                            !self.busy(JobKind::Login) && id_ok,
                            theme::primary_btn(btn_text),
                        )
                        .clicked();
                });
            } else {
                egui::Grid::new("top_row1")
                    .num_columns(6)
                    .spacing([10.0, 3.0])
                    .show(ui, |ui| {
                        draw_connection(ui);
                        ui.label("设备：");
                        ui.monospace(device_text(&self.identity.device_id));
                        ui.end_row();
                    });
                ui.add_space(3.0);
                egui::Grid::new("top_row2")
                    .num_columns(5)
                    .spacing([8.0, 3.0])
                    .show(ui, |ui| {
                        ui.label("账号：");
                        mobile::text_edit(
                            ui,
                            "login_username",
                            &mut self.username,
                            crate::platform::InputKind::Text,
                            220.0,
                        );
                        ui.label("密码：");
                        mobile::text_edit(
                            ui,
                            "login_password",
                            &mut self.password,
                            crate::platform::InputKind::Password,
                            200.0,
                        );
                        ui.checkbox(&mut self.remember, "记住密码");
                        login_clicked = ui
                            .add_enabled(
                                !self.busy(JobKind::Login) && id_ok,
                                theme::primary_btn(btn_text),
                            )
                            .clicked();
                        ui.end_row();
                    });
            }
            if rebuild_clicked {
                self.rebuild_identity();
            } else if login_clicked {
                if logged {
                    self.do_logout();
                } else {
                    self.do_login();
                }
            }
            ui.add_space(4.0);
        });

        // ── 底部状态栏 ──────────────────────────────────────────
        egui::TopBottomPanel::bottom("status_bar").show(ctx, |ui| {
            ui.add_space(4.0);
            ui.horizontal_wrapped(|ui| {
                ui.label(format!("日志 {} 行", self.log.len()));
                ui.separator();
                if let Some(s) = &self.data_page.semester {
                    ui.label(format!(
                        "学期 {}：有效 {}/{} 次",
                        s.sname, s.semester_valid_count, s.semester_count
                    ));
                }
                if let Some(err) = &self.data_page.cheat_error {
                    ui.colored_label(theme::err(), format!("自查失败：{err}"));
                } else if let Some(c) = &self.data_page.cheat {
                    if c.is_clean() {
                        ui.colored_label(theme::ok(), "自查正常");
                    } else {
                        ui.colored_label(theme::err(), format!("已标记：{}", c.self_brief()));
                    }
                }
                ui.separator();
                ui.label(&self.status);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    // 超链接但不用链接样式，纯文本观感
                    ui.hyperlink_to(
                        egui::RichText::new("YanamiNeko").color(theme::text_dim()),
                        "https://github.com/YanamiNeko",
                    );
                    ui.label(egui::RichText::new("Powered by").color(theme::text_dim()));
                    ui.separator();
                    if ui
                        .add_enabled(
                            !(self.update.checking || self.update.downloading),
                            egui::Button::new(
                                egui::RichText::new("检查更新")
                                    .small()
                                    .color(theme::text_dim()),
                            ),
                        )
                        .clicked()
                    {
                        self.check_update(true);
                    }
                    ui.label(
                        egui::RichText::new(format!("v{}", crate::platform::version_name()))
                            .small()
                            .color(theme::text_dim()),
                    );
                });
            });
            ui.add_space(2.0);
        });

        // ── 标签页 ──────────────────────────────────────────────
        egui::CentralPanel::default().show(ctx, |ui| {
            mobile::tab_bar(ui, &mut self.tab);
            ui.separator();
            match self.tab {
                0 => self.draw_run(ui),
                1 => self.draw_ai(ui),
                2 => self.draw_records(ui),
                3 => self.draw_data(ui),
                4 => self.draw_user(ui),
                5 => self.draw_device(ui),
                6 => self.log.render(ui),
                7 => self.draw_osm_page(ui),
                _ => self.draw_about(ui),
            }
        });

        if let Some(p) = &self.popup {
            let mut open = true;
            let mut close = false;
            egui::Window::new(egui::RichText::new(&p.title).strong())
                .open(&mut open)
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .order(egui::Order::Foreground)
                .show(ctx, |ui| {
                    ui.set_max_width((ctx.screen_rect().width() - 32.0).min(520.0));
                    for line in &p.lines {
                        ui.label(line);
                    }
                    ui.add_space(6.0);
                    if ui.button("关闭").clicked() {
                        close = true;
                    }
                });
            if !open || close {
                self.popup = None;
            }
        }
        self.draw_update_windows(ctx);
        crate::platform::sync_clipboard(ctx);
    }
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let (tx, rx) = std::sync::mpsc::channel();
        let (net_tx, net_rx) = std::sync::mpsc::channel();
        let font_loaded = fonts::install(&cc.egui_ctx);
        theme::apply(&cc.egui_ctx);
        // 严格读取：读不出来就硬阻断（空 DeviceId 会让每个请求换一个随机设备号）。
        let (identity, identity_error) = match model::try_load_identity() {
            Ok(id) => (id, None),
            Err(e) => (HeaderIdentity::default(), Some(e)),
        };
        let device_buf = identity.clone();
        let session = model::load_session();
        let config = model::load_config();
        let username = config.username.clone();
        let password = config.password.clone();
        let remember = config.remember;

        let mut app = Self {
            tx: tx.clone(),
            rx,
            log: log::LogStore::default(),
            font_loaded,
            ip: String::new(),
            identity,
            device_buf,
            identity_error,
            session: session.is_logged_in().then_some(session),
            config: config.clone(),
            username,
            password,
            remember,
            in_flight: HashMap::new(),
            status: "就绪".into(),
            popup: None,
            ai_confirm: None,
            ai_cancel: None,
            tab: 0,
            run_page: run::RunPage {
                dist_min: config.dist_min,
                dist_max: config.dist_max,
                pace_min: config.pace_min,
                pace_max: config.pace_max,
                manual_altitude: config
                    .manual_altitude_range
                    .map(|r| format!("{}-{}", r.min_m, r.max_m))
                    .or_else(|| config.manual_altitude.map(|v| v.to_string()))
                    .unwrap_or_default(),
                start_mode: config.start_mode.clamp(0, 1) as usize,
                days_ago: config.days_ago.clamp(0, 3),
                hour: config.hour.clamp(0, 23),
                minute: config.minute.clamp(0, 59),
                face_check: config.face_check,
                plan: None,
                route_mode: crate::track::generate_road::RouteMode::parse(&config.route_mode),
                map: map::MapState::default(),
                preview: None,
                preview_stale: true,
                map_fitted: false,
                pending_save: false,
            },
            ai_page: ai::AiPage {
                days: 1,
                per_day: 1,
                ..Default::default()
            },
            records_page: records::RecordsPage::default(),
            data_page: data::DataPage::default(),
            user_page: user::UserPage::default(),
            device_page: device::DevicePage::default(),
            update: about::UpdateUi::default(),
            network: None,
            osm_page: osm::OsmPage {
                path: config.osm_path.clone(),
                ..Default::default()
            },
            net_tx,
            net_rx,
        };
        if app.font_loaded.is_none() {
            app.log
                .push("未找到中文字体（msyh/simhei/simsun），界面中文可能显示为方块");
        }
        if let Some(err) = app.identity_error.clone() {
            app.log
                .push(&format!("× 设备身份读取失败：{err}（已停止网络操作，请在设备页「备份并重建身份」）"));
            app.status = "设备身份读取失败，已停止网络操作".into();
            app.tab = 5;
        }
        // 上次更新残留的 .old/.new 按「新版本是否已成功启动」决定是否清掉
        crate::update::cleanup_residue();
        // 启动检查更新（silent 静默 / ask 询问 / off 关闭）
        match app.config.update_check.as_str() {
            "off" => {}
            "ask" => app.update.ask_startup = true,
            _ => app.check_update(false),
        }
        app.fetch_ip();
        // AI 项目列表先上缓存，网络刷新后覆盖
        app.ai_page.list = crate::api::model::load_ai_sports().unwrap_or_default();
        if app.session.is_some() && app.identity_error.is_none() {
            app.refresh_data_page();
            app.refresh_records();
            app.refresh_user_page();
            app.refresh_ai_list();
            app.refresh_fence();
            app.refresh_points();
        }
        // 已配置 OSM 路网则启动时后台加载
        if !app.config.osm_path.is_empty() {
            let path = app.config.osm_path.clone();
            app.load_osm_async(path);
        }
        app
    }

    /// 后台任务（线程 + 日志 channel），带 panic 兜底。
    ///
    /// `f` panic 时线程会静默死掉，旧实现在这种情况下 busy 标志永远不解除。
    /// 这里用 `catch_unwind` 兜住，**无论成败都补发一条 `ok=false` 的完成消息**，
    /// 保证界面一定能解锁；另有一层 `expire_stale_jobs` 超时兜底。
    pub fn spawn_job<F>(&mut self, kind: JobKind, f: F)
    where
        F: FnOnce(Sender<String>) + Send + 'static,
    {
        self.mark(kind);
        let tx = self.tx.clone();
        let guard = tx.clone();
        std::thread::spawn(move || {
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(tx)));
            if outcome.is_err() {
                let payload = serde_json::json!({
                    "ok": false,
                    "message": "内部错误（后台线程异常退出），已解除界面锁定",
                });
                let _ = guard.send(format!("{}{}", kind.done_prefix(), payload));
                let _ = guard.send(format!("× {} 后台线程异常退出", kind.label()));
            }
        });
    }

    // ── 在跑任务记账 ────────────────────────────────────────────

    pub fn busy(&self, kind: JobKind) -> bool {
        self.in_flight.contains_key(&kind)
    }

    pub fn mark(&mut self, kind: JobKind) {
        self.in_flight.insert(kind, Instant::now());
    }

    pub fn clear(&mut self, kind: JobKind) {
        self.in_flight.remove(&kind);
    }

    /// 提交类任务互斥：跑步与 AI 提交不能同时进行（只读刷新不受影响）。
    pub fn submission_busy(&self) -> bool {
        self.busy(JobKind::Run) || self.busy(JobKind::AiSubmit)
    }

    /// 数据页三个动作共用一把锁：cheat 还在飞时不许查询排行榜。
    pub fn data_page_busy(&self) -> bool {
        self.busy(JobKind::DataSemester)
            || self.busy(JobKind::DataCheat)
            || self.busy(JobKind::Rank)
    }

    /// 记录页两路刷新共用一把锁。
    pub fn records_busy(&self) -> bool {
        self.busy(JobKind::Records) || self.busy(JobKind::AiRecords)
    }

    /// 任意前台任务在跑（更新前的打断保护）。
    pub fn any_busy(&self) -> bool {
        self.in_flight.keys().any(|k| k.blocks_update())
    }

    /// 超时兜底：超过该任务类型的超时时间仍无完成消息 → 解除锁定并提示。
    pub fn expire_stale_jobs(&mut self) {
        let now = Instant::now();
        let expired: Vec<JobKind> = self
            .in_flight
            .iter()
            .filter(|(kind, started)| now.duration_since(**started) > kind.timeout())
            .map(|(kind, _)| *kind)
            .collect();
        if expired.is_empty() {
            return;
        }
        for kind in expired {
            self.in_flight.remove(&kind);
            match kind {
                JobKind::UpdateCheck => self.update.checking = false,
                JobKind::UpdateDownload => self.update.downloading = false,
                JobKind::RunDetail => self.records_page.detail_loading = false,
                JobKind::AiDetail => self.records_page.ai_detail_loading = false,
                JobKind::AiSubmit => self.ai_cancel = None,
                _ => {}
            }
            let note = format!(
                "⚠ {} 超过 {}s 没有响应，已解除界面锁定（后台线程可能仍在运行，请勿重复提交）",
                kind.label(),
                kind.timeout().as_secs()
            );
            self.log.push(&note);
            self.status = format!("{} 超时，已解除锁定", kind.label());
        }
    }

    /// 设备身份损坏时的恢复路径：原文件改名为 `identity.json.corrupt-<时间戳>`，
    /// 再按「文件不存在」路径重新生成一套身份（DeviceId/IDFA 只生成一次并落盘）。
    pub fn rebuild_identity(&mut self) {
        let dir = crate::platform::data_dir();
        let src = dir.join("identity.json");
        let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
        let backup = dir.join(format!("identity.json.corrupt-{stamp}"));
        match std::fs::rename(&src, &backup) {
            Ok(()) => {
                self.log
                    .push(&format!("√ 已备份损坏的身份文件到 {}", backup.display()));
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                self.status = format!("× 备份身份文件失败：{e}");
                return;
            }
        }
        match model::try_load_identity() {
            Ok(id) => {
                self.identity = id.clone();
                self.device_buf = id;
                self.identity_error = None;
                self.log
                    .push("√ 已重建设备身份（DeviceId/IDFA 已重新生成并落盘）");
                self.log
                    .push("⚠ 城市与定位锚点是默认值，请到设备信息页填写真实位置后再跑步");
                self.status = "√ 设备身份已重建".into();
            }
            Err(e) => {
                self.identity_error = Some(e.clone());
                self.status = format!("× 重建身份失败：{e}");
            }
        }
    }

    pub fn logger(tx: Sender<String>) -> impl FnMut(&str) {
        move |s: &str| {
            tx.send(s.to_string()).ok();
        }
    }
}

/// 顶栏设备号展示：身份未读出来时给一句人话，而不是一片空白。
fn device_text(device_id: &str) -> &str {
    if device_id.trim().is_empty() {
        "（未读取，见上方错误）"
    } else {
        device_id
    }
}
