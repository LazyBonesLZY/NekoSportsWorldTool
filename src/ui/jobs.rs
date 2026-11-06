//! 后台任务与消息处理：IP 获取 / 登录 / 刷新 / 消息协议分发。
//!
//! 所有任务都经 `App::spawn_job(kind, ..)` 启动：按 `JobKind` 记账忙标志，
//! panic 有兜底、超时有第二道保险。**完成消息一律是 `{"ok":bool,...}`**，
//! 失败路径必须带上可读的 `message`，不允许用空数据冒充成功。

use super::{
    App, AI_DETAIL, AI_LIST, AI_RECORDS, CHEAT, FENCE_DONE, IP, LOGIN_DONE, LOGOUT_DONE,
    POINTS_DONE, RANK, RECORDS, RUN_DETAIL, SEMESTER, UPDATE_CHK, UPDATE_DONE, UPDATE_PROG, USER,
    JobKind,
};
use crate::api::model;
use crate::track::generate_road::RouteMode;
use crate::update::ReleaseInfo;

impl App {
    /// 检查更新；manual=true 时切到关于页并显示状态。
    pub fn check_update(&mut self, manual: bool) {
        if self.busy(JobKind::UpdateCheck) || self.busy(JobKind::UpdateDownload) {
            return;
        }
        self.update.checking = true;
        self.update.check_error = None;
        if manual {
            self.tab = 8;
            self.status = "正在检查更新…".into();
        }
        self.spawn_job(JobKind::UpdateCheck, move |tx| {
            let mut log = |s: &str| {
                tx.send(s.to_string()).ok();
            };
            let payload = match crate::update::check_latest(&mut log) {
                Ok(Some(rel)) => serde_json::json!({ "ok": true, "newer": true, "release": rel }),
                Ok(None) => serde_json::json!({ "ok": true, "newer": false }),
                Err(e) => serde_json::json!({ "ok": false, "message": e }),
            };
            tx.send(format!("{UPDATE_CHK}{payload}")).ok();
        });
    }

    /// 下载并应用更新（桌面：解包替换 exe；Android：APK 落私有目录）。
    pub fn start_update_download(&mut self, rel: ReleaseInfo) {
        if self.busy(JobKind::UpdateDownload) {
            return;
        }
        self.update.downloading = true;
        self.update.done = 0;
        self.update.total = None;
        self.status = format!("正在下载 {}…", rel.tag);
        self.spawn_job(JobKind::UpdateDownload, move |tx| {
            let outcome: Result<serde_json::Value, String> = (|| {
                let mut report = |done: u64, total: Option<u64>| {
                    tx.send(format!("{UPDATE_PROG}{}/{}", done, total.unwrap_or(0)))
                        .ok();
                };
                let bytes = crate::update::download(&rel.asset_url, &mut report)?;
                // Release 元数据里有资产大小：对不上说明下载被截断或资产被替换，不能拿它覆盖 exe。
                if rel.asset_size > 0 && bytes.len() as u64 != rel.asset_size {
                    return Err(format!(
                        "下载大小不符（期望 {} 字节，实际 {} 字节），已放弃替换",
                        rel.asset_size,
                        bytes.len()
                    ));
                }
                #[cfg(target_os = "android")]
                {
                    let path = crate::platform::data_dir().join(crate::update::APK_NAME);
                    std::fs::write(&path, &bytes).map_err(|e| format!("保存安装包失败: {e}"))?;
                    Ok(serde_json::json!({ "ok": true, "tag": rel.tag }))
                }
                #[cfg(not(target_os = "android"))]
                {
                    let bin = crate::update::extract(&rel.asset_name, &bytes)?;
                    crate::update::apply(&bin)?;
                    Ok(serde_json::json!({ "ok": true, "tag": rel.tag }))
                }
            })();
            let payload = match outcome {
                Ok(v) => v,
                Err(e) => serde_json::json!({ "ok": false, "message": e }),
            };
            tx.send(format!("{UPDATE_DONE}{payload}")).ok();
        });
    }

    /// 后台拉取电子围栏并落盘（供预览裁剪/居中复用），完成后触发预览刷新。
    /// 仅在真实道路路由模式下才需要围栏，避免经典模式用户也进入该端点请求指纹。
    pub(crate) fn refresh_fence(&mut self) {
        if self.run_page.route_mode != RouteMode::Road || self.busy(JobKind::Fence) {
            return;
        }
        let Some(session) = self.session.clone() else {
            return;
        };
        let identity = self.identity.clone();
        self.spawn_job(JobKind::Fence, move |tx| {
            let mut log = App::logger(tx.clone());
            let mut client = crate::api::client::ApiClient::new(identity, Some(session));
            let payload = match crate::api::fence::fetch_geo_fence(&mut client) {
                Ok(f) => {
                    let _ = crate::api::model::save_fence_cache(&f);
                    log(&format!("√ [fence] 电子围栏已缓存 {} 个", f.len()));
                    serde_json::json!({ "ok": true })
                }
                Err(e) => {
                    log(&format!("⚠ [fence] 电子围栏获取失败: {e}"));
                    serde_json::json!({ "ok": false, "message": e })
                }
            };
            tx.send(format!("{FENCE_DONE}{payload}")).ok();
        });
    }

    /// 后台拉取实时点位并落盘（供路线预览使用），完成后触发预览刷新。
    /// 复用 fetch_points 的 TTL 缓存，仅在 Road 模式且锚点已配置时拉取。
    pub(crate) fn refresh_points(&mut self) {
        if self.run_page.route_mode != RouteMode::Road || self.busy(JobKind::Points) {
            return;
        }
        let Some(session) = self.session.clone() else {
            return;
        };
        let identity = self.identity.clone();
        if identity.has_unconfigured_default_location() {
            return;
        }
        let Ok(anchor) = identity.anchor_coordinate() else {
            return;
        };
        self.spawn_job(JobKind::Points, move |tx| {
            let mut log = App::logger(tx.clone());
            let mut client = crate::api::client::ApiClient::new(identity, Some(session));
            let payload = match crate::api::points::fetch_points(&mut client, anchor, &mut log) {
                Ok(pts) => {
                    log(&format!(
                        "√ [points] 点位已缓存 {} 个（供路线预览）",
                        pts.len()
                    ));
                    serde_json::json!({ "ok": true })
                }
                Err(e) => {
                    log(&format!("⚠ [points] 点位获取失败: {e}"));
                    serde_json::json!({ "ok": false, "message": e })
                }
            };
            tx.send(format!("{POINTS_DONE}{payload}")).ok();
        });
    }

    pub(crate) fn fetch_ip(&mut self) {
        if self.busy(JobKind::Ip) {
            return;
        }
        self.spawn_job(JobKind::Ip, |tx| {
            let agent = crate::api::client::make_agent();
            let mut log = App::logger(tx.clone());
            let mut ip = String::new();
            for url in [
                "https://api.ipify.org?format=json",
                "https://httpbin.org/ip",
            ] {
                if let Ok(resp) = agent.get(url).call() {
                    if let Ok(text) = resp.into_string() {
                        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
                            let got = v
                                .get("ip")
                                .or_else(|| v.get("origin"))
                                .and_then(|x| x.as_str())
                                .unwrap_or("")
                                .to_string();
                            if !got.is_empty() {
                                ip = got;
                                break;
                            }
                        }
                    }
                }
            }
            if ip.is_empty() {
                log("公网 IP 获取失败");
                tx.send(format!(
                    "{IP}{}",
                    serde_json::json!({ "ok": false, "message": "获取失败" })
                ))
                .ok();
            } else {
                log(&format!("公网 IP：{ip}"));
                tx.send(format!("{IP}{}", serde_json::json!({ "ok": true, "ip": ip })))
                    .ok();
            }
        });
    }

    pub(crate) fn do_login(&mut self) {
        if let Some(err) = &self.identity_error {
            self.status = format!("设备身份不可用，已阻止登录：{err}");
            return;
        }
        if self.busy(JobKind::Login) {
            return;
        }
        if self.username.is_empty() || self.password.is_empty() {
            self.status = "请填写账号和密码".into();
            return;
        }
        let username = self.username.trim().to_string();
        let password = self.password.clone();
        let identity = self.identity.clone();
        let remember = self.remember;
        self.config.username = username.clone();
        self.config.password = if remember {
            password.clone()
        } else {
            String::new()
        };
        self.config.remember = remember;
        let _ = model::save_config(&self.config);
        self.status = format!("登录中（{username}）…");
        self.spawn_job(JobKind::Login, move |tx| {
            let mut log = App::logger(tx.clone());
            let mut client = crate::api::client::ApiClient::new(identity, None);
            let msg = match crate::api::login::login(&mut client, &username, &password, &mut log) {
                Ok(sess) => {
                    log(&format!(
                        "√ 登录成功 uid={} unid={} name={}",
                        sess.uid, sess.unid, sess.name
                    ));
                    let payload = serde_json::json!({
                        "ok": true,
                        "session": {
                            "uid": sess.uid, "token": sess.token,
                            "unid": sess.unid, "name": sess.name,
                            "weight": sess.display_weight(), "username": username,
                            "device_id": sess.device_id,
                            "profile": sess.profile,
                        },
                    });
                    format!("{LOGIN_DONE}{payload}")
                }
                Err(e) => {
                    log(&format!("× 登录失败: {e}"));
                    format!(
                        "{LOGIN_DONE}{}",
                        serde_json::json!({"ok": false, "message": e})
                    )
                }
            };
            tx.send(msg).ok();
        });
    }

    pub(crate) fn do_logout(&mut self) {
        if self.any_busy() {
            self.status = "有任务进行中，请等它结束或超时后再登出".into();
            return;
        }
        let identity = self.identity.clone();
        let session = self.session.clone();
        self.session = None;
        self.status = "已登出".into();
        self.spawn_job(JobKind::Logout, move |tx| {
            let mut log = App::logger(tx.clone());
            if let Some(sess) = session {
                let mut client = crate::api::client::ApiClient::new(identity, Some(sess));
                crate::api::login::logout(&mut client, &mut log);
            } else {
                // 没有会话也要清掉磁盘上的残留，避免下次启动又「登录」上。
                model::clear_session();
                log("[logout] 本地会话已删除");
            }
            tx.send(format!("{LOGOUT_DONE}{}", serde_json::json!({ "ok": true })))
                .ok();
        });
    }

    pub fn refresh_ai_list(&mut self) {
        let Some(session) = self.session.clone() else {
            self.status = "请先登录".into();
            return;
        };
        if self.busy(JobKind::AiList) {
            return;
        }
        let identity = self.identity.clone();
        self.status = "AI 列表拉取中…".into();
        self.spawn_job(JobKind::AiList, move |tx| {
            let mut log = App::logger(tx.clone());
            let mut client = crate::api::client::ApiClient::new(identity, Some(session));
            let payload = match crate::api::flow::run_ai_list(&mut client, &mut log) {
                Ok(list) => serde_json::json!({ "ok": true, "list": list }),
                Err(e) => {
                    log(&format!("× AI 列表拉取失败: {e}"));
                    serde_json::json!({ "ok": false, "message": e })
                }
            };
            tx.send(format!("{AI_LIST}{payload}")).ok();
        });
    }

    pub fn refresh_records(&mut self) {
        let Some(session) = self.session.clone() else {
            self.status = "请先登录".into();
            return;
        };
        if self.busy(JobKind::Records) {
            return;
        }
        let identity = self.identity.clone();
        self.status = "记录拉取中…".into();
        self.spawn_job(JobKind::Records, move |tx| {
            let mut log = App::logger(tx.clone());
            let mut client = crate::api::client::ApiClient::new(identity, Some(session));
            let payload = match crate::api::flow::run_records(&mut client, &mut log) {
                Ok(rows) => serde_json::json!({ "ok": true, "rows": rows }),
                Err(e) => {
                    log(&format!("× 记录拉取失败: {e}"));
                    serde_json::json!({ "ok": false, "message": e })
                }
            };
            tx.send(format!("{RECORDS}{payload}")).ok();
        });
    }

    /// 我的页数据聚合。
    pub fn refresh_user_page(&mut self) {
        let Some(session) = self.session.clone() else {
            self.status = "请先登录".into();
            return;
        };
        if self.busy(JobKind::User) {
            return;
        }
        let identity = self.identity.clone();
        self.status = "我的页数据拉取中…".into();
        self.spawn_job(JobKind::User, move |tx| {
            let mut log = App::logger(tx.clone());
            let mut client = crate::api::client::ApiClient::new(identity, Some(session));
            let info = crate::api::user::fetch_my_info(&mut client, &mut log);
            let empty = info.profile.is_null()
                && info.home_page.is_null()
                && info.personal_semester.is_null()
                && info.summary.is_null();
            let payload = if empty {
                serde_json::json!({
                    "ok": false,
                    "message": "我的页接口全部失败，请检查网络或重新登录",
                })
            } else {
                serde_json::json!({
                    "ok": true,
                    "profile": info.profile,
                    "home_page": info.home_page,
                    "personal_semester": info.personal_semester,
                    "summary": info.summary,
                    "completed": info.completed,
                })
            };
            if payload.get("ok").and_then(|b| b.as_bool()) == Some(true) {
                log("√ [user] 我的页数据已更新");
            } else {
                log("× [user] 我的页数据全部拉取失败");
            }
            tx.send(format!("{USER}{payload}")).ok();
        });
    }

    /// 拉取单条跑步详情。
    pub fn fetch_run_detail(&mut self, rrid: i64) {
        let Some(session) = self.session.clone() else {
            return;
        };
        let identity = self.identity.clone();
        self.records_page.detail_loading = true;
        // 详情接口要的是这条记录自己的 uuid（不是设备 id），从已加载的列表里取。
        let record_uuid = self
            .records_page
            .rows
            .iter()
            .find(|r| r.rrid == rrid)
            .map(|r| r.uuid.clone())
            .unwrap_or_default();
        self.spawn_job(JobKind::RunDetail, move |tx| {
            let mut client = crate::api::client::ApiClient::new(identity, Some(session));
            let payload = match crate::api::records::fetch_detail(&mut client, rrid, &record_uuid) {
                Ok(d) => serde_json::json!({ "ok": true, "rrid": rrid, "detail": d }),
                Err(e) => serde_json::json!({
                    "ok": false,
                    "rrid": rrid,
                    "message": format!("详情拉取失败：{e}"),
                }),
            };
            tx.send(format!("{RUN_DETAIL}{payload}")).ok();
        });
    }

    /// 拉取单条 AI 记录全量详情。
    pub fn fetch_ai_detail(&mut self, id: i64) {
        let Some(session) = self.session.clone() else {
            return;
        };
        let identity = self.identity.clone();
        self.records_page.ai_detail_loading = true;
        self.spawn_job(JobKind::AiDetail, move |tx| {
            let mut client = crate::api::client::ApiClient::new(identity, Some(session));
            let payload = match crate::api::ai::fetch_record_detail(&mut client, id) {
                Ok(d) => serde_json::json!({ "ok": true, "id": id, "detail": d }),
                Err(e) => serde_json::json!({ "ok": false, "id": id, "message": e }),
            };
            tx.send(format!("{AI_DETAIL}{payload}")).ok();
        });
    }

    /// AI 记录：遍历全部项目合并（按天排序）。
    pub fn refresh_ai_records(&mut self) {
        let Some(session) = self.session.clone() else {
            self.status = "请先登录".into();
            return;
        };
        if self.busy(JobKind::AiRecords) {
            return;
        }
        let identity = self.identity.clone();
        self.status = "AI 记录拉取中…".into();
        self.spawn_job(JobKind::AiRecords, move |tx| {
            let mut log = App::logger(tx.clone());
            let mut client = crate::api::client::ApiClient::new(identity, Some(session));
            let list = match crate::api::ai::fetch_list(&mut client) {
                Ok(l) => l,
                Err(e) => {
                    log(&format!("× [ai] 项目列表拉取失败: {e}"));
                    tx.send(format!(
                        "{AI_RECORDS}{}",
                        serde_json::json!({ "ok": false, "message": e })
                    ))
                    .ok();
                    return;
                }
            };
            let mut all_groups: Vec<crate::api::ai::AiRecordGroup> = Vec::new();
            let mut total = 0i64;
            let mut failed = 0usize;
            for sport in &list {
                match crate::api::ai::fetch_records(&mut client, sport.id, 50) {
                    Ok(page) => {
                        for mut g in page.groups {
                            for r in &mut g.records {
                                r.name = format!("{} {}", sport.name, r.name);
                            }
                            all_groups.push(g);
                        }
                        total += page.total_count;
                    }
                    Err(e) => {
                        failed += 1;
                        log(&format!("× [ai] {} 拉取失败: {e}", sport.name));
                    }
                }
                std::thread::sleep(std::time::Duration::from_millis(300));
            }
            all_groups.sort_by_key(|g| std::cmp::Reverse(g.score_date));
            let count: usize = all_groups.iter().map(|g| g.records.len()).sum();
            if failed == list.len() && !list.is_empty() {
                log("× [ai] 所有项目的记录都拉取失败");
                tx.send(format!(
                    "{AI_RECORDS}{}",
                    serde_json::json!({ "ok": false, "message": "所有项目的记录都拉取失败" })
                ))
                .ok();
                return;
            }
            log(&format!("√ [ai] {} 个项目共 {} 条记录", list.len(), count));
            let payload = serde_json::json!({
                "ok": true,
                "groups": all_groups,
                "total": total,
                "failed_sports": failed,
            });
            tx.send(format!("{AI_RECORDS}{payload}")).ok();
        });
    }

    /// 学期 + 违规自查（登录 / 跑步提交成功后自动调用）。
    pub fn refresh_data_page(&mut self) {
        self.refresh_semester_only();
        self.refresh_cheat_only();
    }

    /// 学期完成度（独立任务，失败不再伪装成「解析失败」）。
    pub fn refresh_semester_only(&mut self) {
        let Some(session) = self.session.clone() else {
            self.status = "请先登录".into();
            return;
        };
        if self.busy(JobKind::DataSemester) {
            return;
        }
        let identity = self.identity.clone();
        self.spawn_job(JobKind::DataSemester, move |tx| {
            let mut log = App::logger(tx.clone());
            let mut client = crate::api::client::ApiClient::new(identity, Some(session));
            let payload = match crate::api::semester::query(&mut client, &mut log) {
                Ok(r) => match r.summary {
                    Some(s) => {
                        log(&format!(
                            "√ [semester] {} 有效 {}/{} 次，有效里程 {:.2} km",
                            s.sname,
                            s.semester_valid_count,
                            s.semester_count,
                            s.semester_valid_dis / 1000.0
                        ));
                        serde_json::json!({ "ok": true, "summary": s })
                    }
                    None => serde_json::json!({
                        "ok": false,
                        "message": "学期汇总为空（该账号可能暂无学期数据）",
                    }),
                },
                Err(e) => {
                    log(&format!("× [semester] 拉取失败: {e}"));
                    serde_json::json!({ "ok": false, "message": e })
                }
            };
            tx.send(format!("{SEMESTER}{payload}")).ok();
        });
    }

    /// 违规自查（失败时发 ok=false，UI 显示「未知/失败」而不是「正常」）。
    pub fn refresh_cheat_only(&mut self) {
        let Some(session) = self.session.clone() else {
            self.status = "请先登录".into();
            return;
        };
        if self.busy(JobKind::DataCheat) {
            return;
        }
        let identity = self.identity.clone();
        self.spawn_job(JobKind::DataCheat, move |tx| {
            let mut log = App::logger(tx.clone());
            let mut client = crate::api::client::ApiClient::new(identity, Some(session));
            let payload = match crate::api::cheat::query(&mut client, 1, &mut log) {
                Ok(r) => serde_json::json!({
                    "ok": true,
                    "self": r.self_info,
                    "list": r.list,
                }),
                Err(e) => {
                    log(&format!("× [cheat] 检查失败: {e}"));
                    serde_json::json!({ "ok": false, "message": e })
                }
            };
            tx.send(format!("{CHEAT}{payload}")).ok();
        });
    }

    /// 榜单查询：kind=main|indoor|history。
    pub fn refresh_rank(&mut self, kind: &str, subtype: i64) {
        let Some(session) = self.session.clone() else {
            self.status = "请先登录".into();
            return;
        };
        if self.busy(JobKind::Rank) {
            return;
        }
        let identity = self.identity.clone();
        let kind = kind.to_string();
        self.status = "排行榜查询中…".into();
        self.spawn_job(JobKind::Rank, move |tx| {
            let mut log = App::logger(tx.clone());
            let mut client = crate::api::client::ApiClient::new(identity, Some(session));
            let res: Result<Vec<crate::api::rank::RankRow>, String> = (|| {
                match kind.as_str() {
                    "main" => {
                        // 当天无数据时自动回退最近 3 天（当日累计榜凌晨常为空）
                        let (rtype, sort_type) = (subtype / 10, subtype % 10);
                        let mut rows =
                            crate::api::rank::main_rank(&mut client, rtype, sort_type, None)?;
                        let mut back = 0;
                        while rows.is_empty() && back < 3 {
                            back += 1;
                            let d = (chrono::Local::now() - chrono::Duration::days(back))
                                .format("%Y-%m-%d")
                                .to_string();
                            log(&format!("[rank] 当天暂无数据，回退查询 {d}"));
                            rows =
                                crate::api::rank::main_rank(&mut client, rtype, sort_type, Some(d))?;
                        }
                        Ok(rows)
                    }
                    "indoor" => crate::api::rank::indoor_rank(&mut client, subtype, -1),
                    _ => crate::api::rank::history_rank(&mut client, subtype, -1),
                }
            })();
            let payload = match res {
                Ok(rows) => {
                    log(&format!("[rank] {} 行", rows.len()));
                    serde_json::json!({ "ok": true, "rows": rows })
                }
                Err(e) => {
                    log(&format!("× [rank] 查询失败: {e}"));
                    serde_json::json!({ "ok": false, "message": e })
                }
            };
            tx.send(format!("{RANK}{payload}")).ok();
        });
    }
}
