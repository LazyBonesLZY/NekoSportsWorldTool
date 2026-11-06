//! 消息协议分发与弹窗构建。
//!
//! 协议约定：除 `__UPDATE_PROG__`（进度）与普通日志外，所有后台任务的完成消息
//! 都是 `<PREFIX>{"ok":bool,...}`。失败必须带 `message`，UI 按 `ok` 分支渲染，
//! 不允许「空数据 = 成功」。

use super::about::FinishAction;
use super::{
    App, PopupInfo, AI_DETAIL, AI_DONE, AI_LIST, AI_RECORDS, CHEAT, FENCE_DONE, IP, LOGIN_DONE,
    LOGOUT_DONE, POINTS_DONE, RANK, RECORDS, RUN_DETAIL, RUN_DONE, SEMESTER, UPDATE_CHK,
    UPDATE_DONE, UPDATE_PROG, USER, JobKind,
};
use crate::api::model;
use chrono::TimeZone;

/// 把一条 channel 消息拆成 (任务类型, 载荷)；不是协议消息时返回 None（当日志处理）。
fn split_protocol(msg: &str) -> Option<(JobKind, String)> {
    const TABLE: &[(JobKind, &str)] = &[
        (JobKind::Login, LOGIN_DONE),
        (JobKind::Logout, LOGOUT_DONE),
        (JobKind::Ip, IP),
        (JobKind::Run, RUN_DONE),
        (JobKind::AiSubmit, AI_DONE),
        (JobKind::AiList, AI_LIST),
        (JobKind::AiRecords, AI_RECORDS),
        (JobKind::Records, RECORDS),
        (JobKind::RunDetail, RUN_DETAIL),
        (JobKind::AiDetail, AI_DETAIL),
        (JobKind::User, USER),
        (JobKind::DataSemester, SEMESTER),
        (JobKind::DataCheat, CHEAT),
        (JobKind::Rank, RANK),
        (JobKind::Fence, FENCE_DONE),
        (JobKind::Points, POINTS_DONE),
        (JobKind::UpdateCheck, UPDATE_CHK),
        (JobKind::UpdateDownload, UPDATE_DONE),
    ];
    for (kind, prefix) in TABLE {
        if let Some(v) = msg.strip_prefix(prefix) {
            return Some((*kind, v.to_string()));
        }
    }
    None
}

impl App {
    pub(crate) fn poll_messages(&mut self) {
        // 先全部取出再处理：处理过程中可能再次 spawn_job，避免与 rx 借用冲突。
        let mut batch: Vec<(JobKind, String)> = Vec::new();
        let mut logs: Vec<String> = Vec::new();
        let mut progress: Option<String> = None;
        while let Ok(msg) = self.rx.try_recv() {
            if let Some(v) = msg.strip_prefix(UPDATE_PROG) {
                progress = Some(v.to_string());
            } else if let Some((kind, payload)) = split_protocol(&msg) {
                batch.push((kind, payload));
            } else {
                logs.push(msg);
            }
        }
        for line in logs {
            self.log.push(&line);
        }
        for (kind, payload) in batch {
            self.clear(kind);
            self.handle_message(kind, &payload);
        }
        if let Some(v) = progress {
            if let Some((done, total)) = v.split_once('/') {
                self.update.done = done.parse().unwrap_or(0);
                let t: u64 = total.parse().unwrap_or(0);
                self.update.total = (t > 0).then_some(t);
            }
        }

        // OSM 路网加载回传（独立线程，独立 channel）
        while let Ok(res) = self.net_rx.try_recv() {
            self.osm_page.busy = false;
            match res {
                Ok(net) => {
                    let nodes = net.graph.node_count();
                    let bld = net.buildings.len();
                    self.osm_page.fitted = false;
                    self.osm_page.status = format!("√ 路网加载成功：{nodes} 节点 / {bld} 建筑");
                    self.log
                        .push(&format!("[osm] 路网加载成功：{nodes} 节点 / {bld} 建筑"));
                    self.network = Some(net);
                }
                Err(e) => {
                    self.osm_page.status = format!("路网加载失败：{e}");
                    self.log.push(&format!("[osm] 路网加载失败：{e}"));
                    self.status = format!("路网加载失败：{e}");
                }
            }
        }
    }

    fn handle_message(&mut self, kind: JobKind, payload: &str) {
        let val: serde_json::Value = serde_json::from_str(payload).unwrap_or_default();
        let ok = val.get("ok").and_then(|b| b.as_bool()).unwrap_or(false);
        let message = || {
            val.get("message")
                .and_then(|m| m.as_str())
                .unwrap_or("未知错误")
                .to_string()
        };
        match kind {
            JobKind::Login => self.on_login(ok, &val, message()),
            JobKind::Logout => {}
            JobKind::Ip => {
                if ok {
                    self.ip = val
                        .get("ip")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                } else {
                    self.ip = "失败".into();
                }
            }
            JobKind::Run => self.on_run(ok, &val, message()),
            JobKind::AiSubmit => self.on_ai_submit(ok, &val, message()),
            JobKind::AiList => {
                if ok {
                    match serde_json::from_value::<Vec<crate::api::ai::AiSport>>(
                        val.get("list").cloned().unwrap_or_else(|| serde_json::json!([])),
                    ) {
                        Ok(list) => {
                            self.status = format!("已拉取 {} 个 AI 项目", list.len());
                            self.ai_page.list = list;
                            self.ai_page.selected = 0;
                        }
                        Err(_) => self.status = "AI 列表解析失败".into(),
                    }
                } else {
                    self.status = format!("AI 列表拉取失败：{}", message());
                }
            }
            JobKind::Records => {
                if ok {
                    match serde_json::from_value::<Vec<crate::api::records::RecordRow>>(
                        val.get("rows").cloned().unwrap_or_else(|| serde_json::json!([])),
                    ) {
                        Ok(rows) => {
                            self.status = format!("已刷新 {} 条记录", rows.len());
                            self.records_page.rows = rows;
                        }
                        Err(_) => self.status = "记录数据解析失败".into(),
                    }
                } else {
                    self.status = format!("记录拉取失败：{}", message());
                }
            }
            JobKind::AiRecords => {
                if ok {
                    self.records_page.ai_groups = val
                        .get("groups")
                        .and_then(|x| serde_json::from_value(x.clone()).ok())
                        .unwrap_or_default();
                    self.records_page.ai_total =
                        val.get("total").and_then(|x| x.as_i64()).unwrap_or(0);
                    let failed = val.get("failed_sports").and_then(|x| x.as_i64()).unwrap_or(0);
                    self.status = if failed > 0 {
                        format!(
                            "AI 记录已更新（{} 条，{} 个项目拉取失败）",
                            self.records_page.ai_total, failed
                        )
                    } else {
                        format!("AI 记录已更新（{} 条）", self.records_page.ai_total)
                    };
                } else {
                    self.status = format!("AI 记录拉取失败：{}", message());
                }
            }
            JobKind::RunDetail => {
                let rrid = val.get("rrid").and_then(|x| x.as_i64());
                // 只接受当前正在看的那一条：防止 A 的响应覆盖 B 的详情页。
                let current = self.records_page.detail_rrid;
                let applies = match (rrid, current) {
                    (Some(r), Some(c)) => r == c,
                    (None, Some(_)) => true, // panic 兜底消息没有 rrid，也要解锁
                    _ => false,
                };
                if applies {
                    if ok {
                        self.records_page.detail_raw = val.get("detail").cloned();
                    } else {
                        self.records_page.detail_raw = Some(serde_json::json!({
                            "statusInfo": format!("详情拉取失败：{}", message()),
                        }));
                    }
                    self.records_page.detail_loading = false;
                }
            }
            JobKind::AiDetail => {
                let id = val.get("id").and_then(|x| x.as_i64());
                let current = self.records_page.ai_detail_id;
                let applies = match (id, current) {
                    (Some(r), Some(c)) => r == c,
                    (None, Some(_)) => true,
                    _ => false,
                };
                if applies {
                    self.records_page.ai_detail_raw = if ok {
                        val.get("detail").cloned()
                    } else {
                        Some(serde_json::json!({ "fetchError": message() }))
                    };
                    self.records_page.ai_detail_loading = false;
                }
            }
            JobKind::User => {
                if ok {
                    self.user_page.info = Some(crate::api::user::MyInfo {
                        profile: val
                            .get("profile")
                            .cloned()
                            .unwrap_or(serde_json::Value::Null),
                        home_page: val
                            .get("home_page")
                            .cloned()
                            .unwrap_or(serde_json::Value::Null),
                        personal_semester: val
                            .get("personal_semester")
                            .cloned()
                            .unwrap_or(serde_json::Value::Null),
                        summary: val
                            .get("summary")
                            .cloned()
                            .unwrap_or(serde_json::Value::Null),
                        completed: val
                            .get("completed")
                            .cloned()
                            .unwrap_or(serde_json::Value::Null),
                    });
                    self.status = "我的页数据已更新".into();
                } else {
                    self.status = format!("我的页数据拉取失败：{}", message());
                }
            }
            JobKind::DataSemester => {
                if ok {
                    match serde_json::from_value::<crate::api::semester::SemesterSummary>(
                        val.get("summary").cloned().unwrap_or(serde_json::Value::Null),
                    ) {
                        Ok(sm) => {
                            self.data_page.semester = Some(sm);
                            self.data_page.semester_error = None;
                        }
                        Err(e) => {
                            self.data_page.semester_error = Some(format!("学期数据解析失败：{e}"));
                            self.status = "学期数据解析失败".into();
                        }
                    }
                } else {
                    let msg = message();
                    self.data_page.semester_error = Some(msg.clone());
                    self.status = format!("学期完成度拉取失败：{msg}");
                }
            }
            JobKind::DataCheat => {
                if ok {
                    self.data_page.cheat = Some(crate::api::cheat::CheatReport {
                        self_info: val
                            .get("self")
                            .cloned()
                            .unwrap_or(serde_json::Value::Null),
                        list: val
                            .get("list")
                            .and_then(|l| l.as_array())
                            .cloned()
                            .unwrap_or_default(),
                    });
                    self.data_page.cheat_error = None;
                } else {
                    let msg = message();
                    self.data_page.cheat = None;
                    self.data_page.cheat_error = Some(msg.clone());
                    self.status = format!("违规自查失败：{msg}");
                }
            }
            JobKind::Rank => {
                if ok {
                    match serde_json::from_value::<Vec<crate::api::rank::RankRow>>(
                        val.get("rows").cloned().unwrap_or_else(|| serde_json::json!([])),
                    ) {
                        Ok(rows) => {
                            self.status = format!("排行榜已更新（{} 行）", rows.len());
                            self.data_page.rank_rows = rows;
                        }
                        Err(_) => self.status = "排行榜解析失败".into(),
                    }
                } else {
                    self.status = format!("排行榜查询失败：{}", message());
                }
            }
            JobKind::Fence => {
                if ok {
                    self.run_page.preview_stale = true;
                } else {
                    self.status = format!("电子围栏获取失败：{}", message());
                }
            }
            JobKind::Points => {
                if ok {
                    self.run_page.preview_stale = true;
                } else {
                    self.status = format!("实时点位获取失败：{}", message());
                }
            }
            JobKind::UpdateCheck => {
                self.update.checking = false;
                if ok {
                    if val.get("newer").and_then(|b| b.as_bool()).unwrap_or(false) {
                        if let Ok(rel) = serde_json::from_value::<crate::update::ReleaseInfo>(
                            val.get("release")
                                .cloned()
                                .unwrap_or(serde_json::Value::Null),
                        ) {
                            self.status = format!("发现新版本 {}", rel.tag);
                            self.update.latest = Some(rel.clone());
                            self.update.up_to_date = false;
                            self.update.confirm = Some(rel);
                        }
                    } else {
                        self.update.up_to_date = true;
                        self.status = "已是最新版本".into();
                    }
                } else {
                    let msg = message();
                    self.update.check_error = Some(msg.clone());
                    self.status = format!("检查更新失败：{msg}");
                }
            }
            JobKind::UpdateDownload => {
                self.update.downloading = false;
                let tag = val
                    .get("tag")
                    .and_then(|t| t.as_str())
                    .unwrap_or("")
                    .to_string();
                if ok {
                    self.status = format!("√ 已更新到 {tag}");
                    #[cfg(target_os = "android")]
                    {
                        self.update.finish = Some(FinishAction::InstallApk { tag });
                    }
                    #[cfg(not(target_os = "android"))]
                    {
                        self.update.finish = Some(FinishAction::Restart { tag });
                    }
                } else {
                    let msg = message();
                    self.update.check_error = Some(msg.clone());
                    self.status = format!("× 更新失败：{msg}");
                    self.popup = Some(PopupInfo {
                        title: "更新失败".into(),
                        lines: vec![msg],
                    });
                }
            }
        }
    }

    fn on_login(&mut self, ok: bool, val: &serde_json::Value, message: String) {
        if !ok {
            self.status = format!("登录失败：{message}");
            return;
        }
        // 直接从消息构造会话（写盘可能被占用，不依赖回读）
        if let Some(sess) = val
            .get("session")
            .and_then(|s| serde_json::from_value::<model::Session>(s.clone()).ok())
        {
            self.session = Some(sess);
        } else {
            // 兜底：从磁盘读（老版本消息格式）
            let sess = model::load_session();
            if sess.is_logged_in() {
                self.session = Some(sess);
            } else {
                self.status = "登录成功但本地会话缺失".into();
                return;
            }
        }
        let name = self
            .session
            .as_ref()
            .map(|s| s.name.clone())
            .unwrap_or_default();
        self.status = format!("已登录：{name}");
        // 登录成功后补齐全套数据（首次登录时项目列表等均为空）
        self.refresh_data_page();
        self.refresh_records();
        self.refresh_user_page();
        self.refresh_ai_list();
        self.refresh_fence();
        self.refresh_points();
    }

    fn on_run(&mut self, ok: bool, val: &serde_json::Value, message: String) {
        if !ok {
            self.status = format!("跑步提交失败：{message}");
            return;
        }
        self.status = format!(
            "跑步提交成功 rrid={}（OBS {}/2）",
            val["rrid"].as_i64().unwrap_or(0),
            val["obs_ok"].as_i64().unwrap_or(0)
        );
        self.popup = Some(run_popup(val));
        self.refresh_data_page();
        // 提交末尾可能落盘了新身份；读失败时**保留内存中的旧身份**，
        // 绝不用 default() 覆盖（空 DeviceId 会让每个请求换一个随机设备号）。
        match model::try_load_identity() {
            Ok(id) => {
                self.identity = id;
                self.identity_error = None;
            }
            Err(e) => {
                self.log.push(&format!(
                    "⚠ 提交后重载设备身份失败：{e}（继续使用内存中的身份，不覆盖）"
                ));
            }
        }
        self.run_page.preview_stale = true;
    }

    fn on_ai_submit(&mut self, ok: bool, val: &serde_json::Value, message: String) {
        self.ai_cancel = None;
        if !ok {
            self.status = format!("AI 提交失败：{message}");
            return;
        }
        self.status = "AI 运动提交成功".into();
        self.popup = Some(ai_popup(val));
    }
}

fn fmt_hms(ms: i64) -> String {
    chrono::Local
        .timestamp_millis_opt(ms)
        .single()
        .map(|t| t.format("%Y-%m-%d %H:%M:%S").to_string())
        .unwrap_or_default()
}

fn fmt_dur(sec: i64) -> String {
    format!("{}:{:02}:{:02}", sec / 3600, sec % 3600 / 60, sec % 60)
}

fn run_popup(v: &serde_json::Value) -> PopupInfo {
    let dist = v["dist"].as_f64().unwrap_or(0.0);
    let dur = v["dur"].as_i64().unwrap_or(0);
    let pace = if dist > 0.0 {
        dur as f64 / (dist / 1000.0)
    } else {
        0.0
    };
    let mut lines = vec![
        format!("记录号：{}", v["rrid"].as_i64().unwrap_or(0)),
        format!("UUID：{}", v["uuid"].as_str().unwrap_or("")),
        format!(
            "距离：{:.2} km（达标线 {} m）",
            dist / 1000.0,
            v["sel_distance"].as_i64().unwrap_or(0)
        ),
        format!(
            "时长：{} · 配速 {}:{:02}/km",
            fmt_dur(dur),
            (pace / 60.0) as i64,
            (pace as i64) % 60
        ),
        format!(
            "步数：{}（步频 {} spm）",
            v["steps"].as_i64().unwrap_or(0),
            v["avg_step_freq"].as_i64().unwrap_or(0)
        ),
        format!(
            "卡路里：{} kcal · 功率 {} W",
            v["calorie"].as_i64().unwrap_or(0),
            v["avg_power"].as_i64().unwrap_or(0)
        ),
        format!("开始时间：{}", fmt_hms(v["start"].as_i64().unwrap_or(0))),
        format!(
            "OBS 上传：{}/2 · 详情验证：{}",
            v["obs_ok"].as_i64().unwrap_or(0),
            if v["verify"].as_bool().unwrap_or(false) {
                "通过"
            } else {
                "未通过"
            }
        ),
    ];
    // 达标判定明细（详情接口 reasonList）。flow 在每轮开始时已清空该槽位，
    // 因此这里读到的一定是本次提交的详情；锁中毒时静默跳过而不是 panic。
    if let Ok(detail) = crate::api::flow::VERIFY_DETAIL.lock() {
        if let Some(d) = detail.as_ref() {
            if let Some(list) = d.get("reasonList").and_then(|x| x.as_array()) {
                for r in list {
                    let ok = r
                        .get("complete")
                        .and_then(|x| x.as_bool())
                        .unwrap_or(false);
                    let reason = r.get("reason").and_then(|x| x.as_str()).unwrap_or("");
                    lines.push(format!(
                        "判定：{} {}",
                        if ok { "达标" } else { "未达标" },
                        reason
                    ));
                }
            }
        }
    }
    PopupInfo {
        title: "跑步结果".into(),
        lines,
    }
}

fn ai_popup(v: &serde_json::Value) -> PopupInfo {
    if v["batch"].as_bool().unwrap_or(false) {
        let mut lines = vec![
            format!(
                "成功：{}/{}",
                v["success"].as_i64().unwrap_or(0),
                v["total"].as_i64().unwrap_or(0)
            ),
            format!(
                "覆盖：{} 天 × 每天 {} 次 × {} 个项目",
                v["days"].as_i64().unwrap_or(0),
                v["per_day"].as_i64().unwrap_or(0),
                v["sports"].as_i64().unwrap_or(0)
            ),
        ];
        if v["cancelled"].as_bool().unwrap_or(false) {
            lines.push(format!(
                "已取消：实际完成 {} 条",
                v["done"].as_i64().unwrap_or(0)
            ));
        }
        return PopupInfo {
            title: "AI 批量补签结果".into(),
            lines,
        };
    }
    PopupInfo {
        title: "AI 运动提交结果".into(),
        lines: vec![
            format!("项目 sportId：{}", v["sport_id"].as_i64().unwrap_or(0)),
            format!("模式：{}", v["mode"].as_str().unwrap_or("")),
            format!("成绩：{}", v["score"].as_str().unwrap_or("")),
            format!(
                "用时：{:.1} 秒 · 速度 {}/分 · 消耗 {:.1}",
                v["secs"].as_f64().unwrap_or(0.0),
                v["per_min"].as_i64().unwrap_or(0),
                v["consume"].as_f64().unwrap_or(0.0),
            ),
            format!(
                "服务器：{} {}",
                v["resp_error"].as_i64().unwrap_or(0),
                v["resp_msg"].as_str().unwrap_or("")
            ),
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 每个完成前缀必须只映射到自己那一类任务，否则会重演「AI_LIST 清掉 AI 提交」。
    #[test]
    fn protocol_prefixes_map_to_their_own_job_kind() {
        assert_eq!(split_protocol("__AI_LIST__{}").unwrap().0, JobKind::AiList);
        assert_eq!(split_protocol("__AI_DONE__{}").unwrap().0, JobKind::AiSubmit);
        assert_eq!(
            split_protocol("__AI_RECORDS__{}").unwrap().0,
            JobKind::AiRecords
        );
        assert_eq!(split_protocol("__LOGIN_DONE__{}").unwrap().0, JobKind::Login);
        assert_eq!(split_protocol("__RUN_DONE__{}").unwrap().0, JobKind::Run);
        assert_eq!(
            split_protocol("__SEMESTER__{}").unwrap().0,
            JobKind::DataSemester
        );
        assert_eq!(split_protocol("__CHEAT__{}").unwrap().0, JobKind::DataCheat);
        assert_eq!(
            split_protocol("__RUN_DETAIL__{}").unwrap().0,
            JobKind::RunDetail
        );
        assert_eq!(
            split_protocol("__AI_DETAIL__{}").unwrap().0,
            JobKind::AiDetail
        );
        // 普通日志不能被识别成协议消息
        assert!(split_protocol("[points] 12 个点位").is_none());
        assert!(split_protocol("__IP_DONE__{\"ok\":true}").is_some());
    }
}
