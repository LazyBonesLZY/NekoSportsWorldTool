//! CLI 子命令实现。

use super::{fmt_hms, get, jstr, logger, make_client, now_ms, parse_flags, parse_pace, print_rows};
use crate::api::ai::AiMode;
use crate::api::client::ApiClient;
use crate::api::model;

pub fn dispatch(args: Vec<String>) -> i32 {
    let cmd = args.first().map(|x| x.as_str()).unwrap_or("help");
    let rest = args.iter().skip(1).map(|x| x.as_str()).collect::<Vec<_>>();
    match cmd {
        "login" => cmd_login(&rest),
        "logout" => cmd_logout(),
        "run" => cmd_run(&rest),
        "indoor" => cmd_indoor(&rest),
        "protocol" => cmd_protocol(&rest),
        "policy" => cmd_policy(&rest),
        "template" => cmd_template(&rest),
        "ai" => cmd_ai(&rest),
        "ai-list" => cmd_ai_list(),
        "records" => cmd_records(),
        "records-raw" => cmd_records_raw(&rest),
        "record-info" => cmd_record_info(&rest),
        "obs-get" => cmd_obs_get(&rest),
        "obs-sample" => cmd_obs_sample(&rest),
        "ai-records" => cmd_ai_records(&rest),
        "ai-info" => cmd_ai_info(&rest),
        "semester" => cmd_semester(),
        "cheat" => cmd_cheat(&rest),
        "rank" => cmd_rank(&rest),
        "update" => cmd_update(&rest),
        "help" | "--help" | "-h" => {
            usage();
            0
        }
        _ => {
            eprintln!("未知命令: {cmd}");
            super::usage();
            1
        }
    }
}

fn usage() {
    super::usage();
}

/// 本地时区的「今天」毫秒区间 [00:00:00, 23:59:59.999]。
fn today_range_ms(now: i64) -> (i64, i64) {
    use chrono::{Datelike, Local, TimeZone};
    let Some(now_dt) = Local.timestamp_millis_opt(now).single() else {
        return (now, now);
    };
    let start = Local
        .with_ymd_and_hms(now_dt.year(), now_dt.month(), now_dt.day(), 0, 0, 0)
        .single()
        .map(|t| t.timestamp_millis())
        .unwrap_or(now);
    (start, start + 86_400_000 - 1)
}

fn cmd_login(rest: &[&str]) -> i32 {
    let flags = parse_flags(rest);
    let (Some(user), Some(pw)) = (get(&flags, "user"), get(&flags, "pass")) else {
        eprintln!("缺少 --user / --pass");
        return 1;
    };
    let identity = match model::try_load_identity() {
        Ok(id) => id,
        Err(e) => {
            eprintln!("设备身份不可用，已阻止登录：{e}");
            eprintln!("请备份并删除 identity.json 后重试（会重新生成 DeviceId/IDFA，触发一次设备变更）");
            return 1;
        }
    };
    let mut client = ApiClient::new(identity, None);
    let mut log = logger();
    match crate::api::login::login(&mut client, user, pw, &mut log) {
        Ok(s) => {
            if get(&flags, "remember").is_some() {
                let mut cfg = model::load_config();
                cfg.username = user.to_string();
                cfg.password = pw.to_string();
                cfg.remember = true;
                let _ = model::save_config(&cfg);
                println!("凭据已保存（会话失效时自动重登）");
            }
            println!("登录成功 uid={} unid={} name={}", s.uid, s.unid, s.name);
            0
        }
        Err(e) => {
            eprintln!("登录失败: {e}");
            1
        }
    }
}

fn cmd_logout() -> i32 {
    let identity = match model::try_load_identity() {
        Ok(id) => id,
        Err(e) => {
            // 身份文件坏了也不该拦着用户登出：清掉本地会话即可。
            eprintln!("警告：设备身份不可用（{e}），仅清理本地会话");
            model::clear_session();
            return 0;
        }
    };
    let sess = model::load_session();
    let mut client = ApiClient::new(identity, sess.is_logged_in().then_some(sess));
    let mut log = logger();
    crate::api::login::logout(&mut client, &mut log);
    0
}

fn cmd_run(rest: &[&str]) -> i32 {
    let flags = parse_flags(rest);
    let cfg = model::load_config();
    let mut client = match make_client() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{e}");
            return 1;
        }
    };
    let dist_km: f32 = get(&flags, "dist")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0.0);
    let pace: f32 = get(&flags, "pace").map(parse_pace).unwrap_or(0.0);
    let ago_min: i64 = get(&flags, "ago").and_then(|v| v.parse().ok()).unwrap_or(0);
    let days_ago: i64 = get(&flags, "days-ago")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0)
        .clamp(0, 3);
    let time_spec = get(&flags, "time").unwrap_or("");
    // 缺省沿用界面（config.json）里保存的 face_check，与 GUI 默认一致；
    // 实际发出值仍由 policy.faceVerify 决定（未开启核验的场地按实机发 0）。
    let face = get(&flags, "face")
        .map(|v| v == "1" || v == "true")
        .unwrap_or(cfg.face_check);
    let (manual_altitude, manual_altitude_range) = match get(&flags, "altitude") {
        Some(value) => match crate::track::altitude::parse_spec(value) {
            Ok(Some(crate::track::altitude::AltitudeSpec::Single(value))) => (Some(value), None),
            Ok(Some(crate::track::altitude::AltitudeSpec::Range(range))) => (None, Some(range)),
            Ok(None) => (None, None),
            Err(e) => {
                eprintln!("--altitude {e}");
                return 1;
            }
        },
        None => (None, None),
    };
    let seed: u64 = get(&flags, "seed")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let seed = if seed == 0 {
        (now_ms() % 2_147_483_647) as u64
    } else {
        seed
    };
    let route_mode = crate::track::generate_road::RouteMode::parse(
        get(&flags, "route").unwrap_or(&cfg.route_mode),
    );
    // 跑法：0 随机跑 / 1 顺序跑(校园跑) / 2 定向跑 / 3 约跑 / 4 自由跑 / 6 户外跑。
    let run_mode: i64 = get(&flags, "mode").and_then(|v| v.parse().ok()).unwrap_or(1);
    if !(0..=6).contains(&run_mode) || run_mode == 5 {
        eprintln!("--mode 只接受 0/1/2/3/4/6（5 是室内跑，用 indoor 子命令）");
        return 1;
    }
    let dry_run = flags.iter().any(|(k, _)| k == "dry-run");
    let out_path = get(&flags, "out").map(|v| v.to_string());
    let force = flags.iter().any(|(k, _)| k == "force");

    // 距离/配速缺省值必须**每次都不一样**：三条一模一样的 2000m/840s 记录摆在一起，
    // 是最容易被人眼和聚类同时发现的模式。默认给「目标 1km 多跑一点」的真实分布：
    // 1.00~1.28km、配速 5'20"~7'10"/km。
    let dist = if dist_km > 0.0 {
        dist_km as f64 * 1000.0
    } else {
        (1.0 + rand::random::<f64>() * 0.28) * 1000.0
    };
    let pace_s: f64 = if pace > 0.0 {
        pace as f64
    } else {
        320.0 + rand::random::<f64>() * 110.0
    };
    let dur = (dist / 1000.0 * pace_s) as i64;
    let now = now_ms();
    let start_ms = if !time_spec.is_empty() {
        // --time "HH:MM" 配合 --days-ago（0-3）；与界面「指定时刻」同语义：
        // 晚于当前时刻的时刻按当前时刻提交（旧实现会真的提交一条未来开跑）。
        let (h, m) = time_spec.split_once(':').unwrap_or((time_spec, "0"));
        let (h, m) = (
            h.parse::<i64>().unwrap_or(7).clamp(0, 23),
            m.parse::<i64>().unwrap_or(0).clamp(0, 59),
        );
        let typed = crate::cli::specified_typed_ms(now, days_ago, h, m);
        let start = crate::cli::specified_start_ms(now, days_ago, h, m, dur);
        if start != typed {
            println!(
                "提示：指定时刻 {} 会导致记录结束于未来，已前移到 {}（与界面一致）",
                fmt_hms(typed),
                fmt_hms(start)
            );
        }
        start
    } else if ago_min > 0 {
        // --ago N：补录，记录**结束**于 N 分钟前，保证 stopTime 永远落在过去。
        println!(
            "⚠ 补录模式：记录结束于 {ago_min} 分钟前，stopTime 与上传时刻相差 {ago_min} 分钟（可聚类特征）"
        );
        crate::cli::backfill_start_ms(now, ago_min, dur)
    } else {
        // 默认让 stopTime 贴近当前时刻（真人是跑完立即上传）。
        // 旧默认把起点放到 30-300 分钟前，stopTime 与上传时刻相差几十分钟到数小时，
        // 属于明显的非人类上传时序，容易被聚类。
        let tail = (rand::random::<f64>() * 90.0 * 1000.0) as i64 + 15_000; // 15-105s 收尾延迟
        now - dur * 1000 - tail
    };

    println!(
        "参数：{:.0}m / {}s / 配速 {}:{:02}/km / 开始 {}",
        dist,
        dur,
        pace_s as i64 / 60,
        pace_s as i64 % 60,
        fmt_hms(start_ms)
    );

    let mut log = logger();
    let params = crate::api::flow::RunParams {
        run_mode,
        dist,
        dur,
        start_ms,
        face_check: face as i64,
        manual_altitude,
        manual_altitude_range,
        seed,
        route_mode,
        dry_run,
        out_path,
    };
    // 重复闸门：今天已经有「距离/时长几乎相同」的记录时停下。
    // 11-06 那三条 2000m/840s 就是这样来的 —— 命令被重复执行，服务端照单全收。
    if !force {
        match crate::api::records::fetch_records(&mut client) {
            Ok(rows) => {
                let today = today_range_ms(now_ms());
                let mut same_day = 0;
                let mut dup: Option<String> = None;
                for r in &rows {
                    if r.start_time < today.0 || r.start_time > today.1 {
                        continue;
                    }
                    same_day += 1;
                    if (r.total_dis - dist).abs() < 100.0 && (r.total_time - dur).abs() < 60 {
                        dup = Some(format!(
                            "rrid={} {:.0}m/{}s",
                            r.rrid, r.total_dis, r.total_time
                        ));
                    }
                }
                if same_day > 0 {
                    println!("今天已有 {same_day} 条跑步记录；本次 {dist:.0}m/{}s", dur);
                }
                if let Some(d) = dup {
                    eprintln!(
                        "已停止：今天的记录里已有几乎相同的一条（{d}）。\
                         \n换一个距离/配速重跑，或加 --force 明确要再提交一条。未产生记录。"
                    );
                    return 1;
                }
            }
            Err(e) => println!("⚠ 记录列表拉取失败（{e}），跳过重复检查"),
        }
    }
    if dry_run {
        println!("--dry-run：只生成轨迹与自检，不提交。");
    }
    match crate::api::flow::run_full_flow(&mut client, &params, &mut log) {
        Ok(out) => {
            if dry_run {
                println!(
                    "--dry-run 完成：{:.0}m / {}s，未提交",
                    out.result.total_dis, out.result.total_time
                );
                return 0;
            }
            println!(
                "跑步提交成功 rrid={} uuid={} obs={}/2 verify={}",
                out.result.rrid,
                out.result.uuid,
                out.obs_ok,
                if out.detail_ok { "通过" } else { "未通过" }
            );
            0
        }
        Err(e) => {
            eprintln!("跑步提交失败: {e}");
            1
        }
    }
}

/// 只读策略探针：按 runMode 打印策略原文摘要（不提交任何记录）。
///
/// 用途：核对「户外跑 / 自由跑 / 随机跑 / 定向跑」与计分跑是否共用同一套策略字段。
/// IPA 里 `+[SWRunConfigDataManager _fetchRunPolicyInMode:params:handler:]` 只有
/// `mode == 5` 会切到室内端点，其余模式同一个 `/api/v70103/runModePolicy`。
fn cmd_policy(rest: &[&str]) -> i32 {
    let flags = parse_flags(rest);
    let mode: i64 = get(&flags, "mode").and_then(|v| v.parse().ok()).unwrap_or(1);
    let json_out = flags.iter().any(|(k, _)| k == "json");
    let mut client = match make_client() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{e}");
            return 1;
        }
    };
    // 只读：首页「已开放跑法」开关（随机跑/定向跑/自由跑）。用于判断户外跑在本账号是否开放。
    if flags.iter().any(|(k, _)| k == "open-modes") {
        let unid = client
            .login
            .as_ref()
            .map(|s| s.unid.clone())
            .unwrap_or_default();
        let method = "POST";
        {
            let body = format!("{{\"unid\":\"{unid}\"}}");
            match client.call(method, "/api/v14/home/openRunModes", &body, &[]) {
                Ok(v) => {
                    println!("== openRunModes（{method}）==");
                    println!("{}", serde_json::to_string_pretty(&v).unwrap_or_default());
                    return 0;
                }
                Err(e) => eprintln!("{method} 失败: {e}"),
            }
        }
        return 1;
    }
    match crate::api::policy::fetch_policy_raw(&mut client, mode) {
        Ok(envelope) => {
            // client.call 可能返回外层信封（error/message/data）也可能已剥到 data。
            let data = envelope.get("data").cloned().unwrap_or(envelope);
            if json_out {
                println!("{}", serde_json::to_string_pretty(&data).unwrap_or_default());
                return 0;
            }
            println!("== runModePolicy runMode={mode} ==");
            if let Some(obj) = data.as_object() {
                for (k, v) in obj {
                    if k == "runRuleModel" || k == "geoFenceModel" || k == "geofenceModel" {
                        continue;
                    }
                    let text = v.to_string();
                    println!("  {k} = {}", crate::textlog::truncate(&text, 120));
                }
            }
            if let Some(rule) = data.get("runRuleModel") {
                println!("-- runRuleModel --");
                if let Some(obj) = rule.as_object() {
                    for (k, v) in obj {
                        let text = v.to_string();
                        println!("  {k} = {}", crate::textlog::truncate(&text, 160));
                    }
                }
            }
            for key in ["geoFenceModel", "geofenceModel", "geoFence"] {
                if let Some(v) = data.get(key) {
                    let n = v
                        .as_array()
                        .map(|a| a.len())
                        .or_else(|| v.get("pointList").and_then(|p| p.as_array()).map(|a| a.len()))
                        .unwrap_or(0);
                    println!("-- {key}: {n} 点 --");
                    println!("  {}", crate::textlog::truncate(&v.to_string(), 400));
                }
            }
            0
        }
        Err(e) => {
            eprintln!("策略拉取失败(runMode={mode}): {e}");
            1
        }
    }
}

fn cmd_template(rest: &[&str]) -> i32 {
    let flags = parse_flags(rest);
    let Some(path) = get(&flags, "file") else {
        eprintln!("缺少 --file <GPX/JSON>");
        return 1;
    };
    match crate::template::load(path) {
        Ok(samples) => {
            let summary = crate::template::summarize(path, &samples);
            println!("本地模板分析（仅预览，不上传）");
            println!("文件：{}", summary.source);
            println!("采样点：{}", summary.samples);
            println!("海拔范围：{:.1}–{:.1} m", summary.min_m, summary.max_m);
            println!(
                "累计上升：{:.1} m；累计下降：{:.1} m",
                summary.gain_m, summary.loss_m
            );
            0
        }
        Err(e) => {
            eprintln!("模板读取失败：{e}");
            1
        }
    }
}

/// `indoor [--steps N] [--dur S] [--seed N] [--ago MIN]`
///
/// 室内跑：纯步数、无轨迹、**无对时闸门**。默认 20 分钟 / 2400 步。
fn cmd_indoor(rest: &[&str]) -> i32 {
    let flags = parse_flags(rest);
    let mut client = match make_client() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{e}");
            return 1;
        }
    };
    let steps: i64 = get(&flags, "steps").and_then(|v| v.parse().ok()).unwrap_or(2400);
    let dur: i64 = get(&flags, "dur").and_then(|v| v.parse().ok()).unwrap_or(1200);
    let seed: u64 = get(&flags, "seed")
        .and_then(|v| v.parse().ok())
        .unwrap_or_else(rand::random);
    let ago_min: i64 = get(&flags, "ago").and_then(|v| v.parse().ok()).unwrap_or(0);
    if steps <= 0 || dur <= 0 {
        eprintln!("--steps 与 --dur 必须为正");
        return 2;
    }
    let now = now_ms();
    let start_ms = if ago_min > 0 {
        crate::cli::backfill_start_ms(now, ago_min, dur)
    } else {
        let tail = (rand::random::<f64>() * 90.0 * 1000.0) as i64 + 15_000;
        now - dur * 1000 - tail
    };
    println!(
        "室内参数：{} 步 / {}s（{} 步/分）/ 开始 {}",
        steps,
        dur,
        steps * 60 / dur.max(1),
        fmt_hms(start_ms)
    );
    let mut log = logger();
    let params = crate::api::flow::IndoorRunParams { steps, dur, start_ms, seed };
    match crate::api::flow::run_indoor_flow(&mut client, &params, &mut log) {
        Ok(out) => {
            println!(
                "室内提交完成 steps={} time={}s 步频={} complete={} reason={}",
                out.total_steps, out.total_time, out.avg_step_freq, out.complete, out.uncomplete_reason
            );
            0
        }
        Err(e) => {
            eprintln!("室内提交失败：{e}");
            1
        }
    }
}

/// `protocol [--json]`
///
/// 打印本工具当前使用的**全部协议参数**。用途：抓一次真机包，与本输出逐项 diff，
/// 就能把台账第 7 节那些"无法静态判定"的项一次收敛掉。
fn cmd_protocol(rest: &[&str]) -> i32 {
    use crate::api::{indoor, obs, submit};
    use crate::crypto::header;
    let flags = parse_flags(rest);
    let json_out = flags.iter().any(|(k, _)| k == "json");

    let header_keys = header::IOS_HEADER_KEYS;
    let body_keys = submit::IOS_BODY_KEYS;
    let point_keys = crate::track::wire::POINT_KEYS;
    let obs_keys = crate::track::wire::OBS_KEYS;
    let indoor_keys = indoor::INDOOR_BODY_KEYS;

    if json_out {
        let v = serde_json::json!({
            "appVersion": header::IOS_APP_VERSION,
            "userAgent": format!("SWCampus/{} (iPhone; iOS <os>; Scale/3.00)", header::IOS_APP_VERSION),
            "acceptLanguage": crate::api::client::IOS_ACCEPT_LANGUAGE,
            "acceptEncoding": crate::api::client::IOS_ACCEPT_ENCODING,
            "contentType": crate::api::client::IOS_CONTENT_TYPE,
            "headerSignKeys": header_keys.to_vec(),
            "recordHeaderSignExtra": ["runes", "runef", "runec"],
            "bodyKeys": body_keys.to_vec(),
            "pointKeys": point_keys.to_vec(),
            "obsKeys": obs_keys.to_vec(),
            "indoorBodyKeys": indoor_keys.to_vec(),
            "obsBucket": obs::OBS_JSON_BUCKET,
            "obsMethod": obs::OBS_METHOD,
            "obsContentType": obs::OBS_CONTENT_TYPE,
        });
        println!("{}", serde_json::to_string_pretty(&v).unwrap_or_default());
        return 0;
    }

    println!("== 版本与通用头 ==");
    println!("appVersion        {}", header::IOS_APP_VERSION);
    println!(
        "User-Agent        SWCampus/{} (iPhone; iOS <os>; Scale/3.00)",
        header::IOS_APP_VERSION
    );
    println!("Accept-Language   {}", crate::api::client::IOS_ACCEPT_LANGUAGE);
    println!("Accept-Encoding   {}", crate::api::client::IOS_ACCEPT_ENCODING);
    println!("Content-Type      {}", crate::api::client::IOS_CONTENT_TYPE);
    println!("Accept            不发（App 侧不设；ureq 会注入 */*）");

    println!("\n== headerSign（普通业务请求，{} 键）==", header_keys.len());
    println!("{}", header_keys.join(" "));
    println!("\n== headerSign（记录提交，另加 3 键）==");
    println!("runes runef runec");

    println!("\n== 室外提交 body（{} 键）==", body_keys.len());
    println!("{}", body_keys.join(" "));
    println!("签名集：全键 − allLocJson/fivePointJson；签名后追加 5 键");

    println!("\n== 室内提交 body（{} 键）==", indoor_keys.len());
    println!("{}", indoor_keys.join(" "));
    println!("签名集：前 16 键（signature/stepsPerTenSec 在签名后写入）");

    println!("\n== 点位（{} 键）==", point_keys.len());
    println!("{}", point_keys.join(" "));

    println!("\n== OBS 顶层（{} 键）==", obs_keys.len());
    println!("{}", obs_keys.join(" "));
    println!("室内 OBS：uuid step_json uid（3 键，无 rrid）");
    println!("bucket={} method={} contentType={}", obs::OBS_JSON_BUCKET, obs::OBS_METHOD, obs::OBS_CONTENT_TYPE);

    println!("\n== 端点 ==");
    for (m, p) in [
        ("GET",  crate::api::client::SERVER_TIME_PATH),
        ("POST", crate::api::policy::POLICY_PATH),
        ("POST", crate::api::points::POINTS_PATH),
        ("POST", crate::api::fence::FENCE_PATH),
        ("POST", submit::RECORD_PATH),
        ("POST", indoor::INDOOR_RECORD_PATH),
        ("POST", obs::OBS_SIGN_PATH),
        ("POST", crate::api::login::LOGIN_PATH),
        ("POST", crate::api::records::RECORDS_PATH),
        ("GET",  crate::api::records::GET_ONE_PATH),
    ] {
        println!("{m:5} {p}");
    }
    println!("\n注：GET 表示真机未调 setHTTPMethod:（SWRequest 默认 0）；室内提交无对时闸门。");
    0
}

fn cmd_ai(rest: &[&str]) -> i32 {
    let flags = parse_flags(rest);
    let Some(sport) = get(&flags, "sport").and_then(|v| v.parse().ok()) else {
        eprintln!("缺少 --sport");
        return 1;
    };
    // 按分钟（--minutes 1-30）或按次（--count 5-1000 步长 5）
    let mode = match get(&flags, "mode").unwrap_or("min") {
        "count" => {
            let reps = get(&flags, "score")
                .or_else(|| get(&flags, "count"))
                .and_then(|v| v.parse::<i64>().ok())
                .unwrap_or(5)
                .clamp(5, 1000);
            AiMode::Count {
                reps: (reps / 5) * 5,
            }
        }
        _ => {
            let minutes = get(&flags, "score")
                .or_else(|| get(&flags, "minutes"))
                .and_then(|v| v.parse::<i64>().ok())
                .unwrap_or(1)
                .clamp(1, 30);
            AiMode::Minutes { minutes }
        }
    };
    let mut client = match make_client() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{e}");
            return 1;
        }
    };
    let mut log = logger();
    match crate::api::flow::run_ai_submit(&mut client, sport, mode, &mut log) {
        Ok(biz) => {
            println!(
                "AI 提交成功 服务器={} {}",
                biz.get("error").and_then(|e| e.as_i64()).unwrap_or(0),
                biz.get("message").and_then(|m| m.as_str()).unwrap_or("")
            );
            0
        }
        Err(e) => {
            eprintln!("AI 提交失败: {e}");
            1
        }
    }
}

fn cmd_ai_list() -> i32 {
    let mut client = match make_client() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{e}");
            return 1;
        }
    };
    let mut log = logger();
    match crate::api::flow::run_ai_list(&mut client, &mut log) {
        Ok(list) => {
            for s in list {
                println!("id={:<4} {}", s.id, s.name);
            }
            0
        }
        Err(e) => {
            eprintln!("拉取失败: {e}");
            1
        }
    }
}

fn cmd_records() -> i32 {
    let mut client = match make_client() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{e}");
            return 1;
        }
    };
    let mut log = logger();
    match crate::api::flow::run_records(&mut client, &mut log) {
        Ok(rows) => {
            print_rows(&rows);
            0
        }
        Err(e) => {
            eprintln!("拉取失败: {e}");
            1
        }
    }
}

/// 记录列表原始探测：支持自定义 body，打印条数与日期范围。
fn cmd_records_raw(rest: &[&str]) -> i32 {
    let flags = parse_flags(rest);
    let body = get(&flags, "body").unwrap_or("{}").to_string();
    let mut client = match make_client() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{e}");
            return 1;
        }
    };
    match client.call("POST", crate::api::records::RECORDS_PATH, &body, &[]) {
        Ok(biz) => {
            let data = crate::api::client::parse_data_field(&biz);
            let arr = data.as_array().cloned().unwrap_or_default();
            let n = arr.len();
            println!("条数: {n}");
            if let Some(out) = get(&flags, "out") {
                std::fs::write(out, serde_json::to_string_pretty(&arr).unwrap_or_default())
                    .map_err(|e| eprintln!("写入失败: {e}"))
                    .ok();
                println!("已保存 {out}");
            } else if n > 0 {
                use crate::api::client::safe_truncate;
                let first = arr[0].to_string();
                let last = arr[n - 1].to_string();
                println!("首条: {}", safe_truncate(&first, 300));
                println!("末条: {}", safe_truncate(&last, 300));
            } else {
                let text = data.to_string();
                println!("data: {}", crate::api::client::safe_truncate(&text, 500));
            }
            0
        }
        Err(e) => {
            eprintln!("拉取失败: {e}");
            1
        }
    }
}

/// 拉取 OBS 对象（签名 GET），解码 gzip+base64 字段后保存。
fn cmd_obs_get(rest: &[&str]) -> i32 {
    let flags = parse_flags(rest);
    let Some(key) = get(&flags, "key") else {
        eprintln!("缺少 --key");
        return 1;
    };
    let mut client = match make_client() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{e}");
            return 1;
        }
    };
    let mut log = |s: &str| eprintln!("{s}");
    match crate::api::obs::fetch_object(&mut client, key, &mut log) {
        Ok(v) => {
            let out = get(&flags, "out").unwrap_or("obs_real.json");
            // 解码 gzip+base64 值便于直接阅读
            let decoded = decode_gz_fields(&v);
            std::fs::write(
                out,
                serde_json::to_string_pretty(&decoded).unwrap_or_default(),
            )
            .map_err(|e| eprintln!("写入失败: {e}"))
            .ok();
            println!("已保存 {out}");
            0
        }
        Err(e) => {
            eprintln!("拉取失败: {e}");
            1
        }
    }
}

/// 解码 OBS 对象中 gzip+base64 的字段值（尽力而为）。
fn decode_gz_fields(v: &serde_json::Value) -> serde_json::Value {
    use base64::Engine;
    let mut out = v.clone();
    if let Some(obj) = out.as_object_mut() {
        for (_k, val) in obj.iter_mut() {
            if let Some(s) = val.as_str() {
                let decoded = base64::engine::general_purpose::STANDARD
                    .decode(s)
                    .ok()
                    .and_then(|raw| {
                        let mut dec = flate2::read::GzDecoder::new(raw.as_slice());
                        use std::io::Read;
                        let mut txt = String::new();
                        dec.read_to_string(&mut txt).ok()?;
                        Some(txt)
                    });
                if let Some(txt) = decoded {
                    let parsed = serde_json::from_str::<serde_json::Value>(&txt)
                        .map(|x| x.to_string())
                        .unwrap_or(txt);
                    *val = serde_json::Value::String(parsed);
                }
            }
        }
    }
    out
}

/// 本地生成一份 OBS 对象样本（不提交；结构对照用）。
fn cmd_obs_sample(rest: &[&str]) -> i32 {
    let flags = parse_flags(rest);
    let dist = get(&flags, "dist")
        .and_then(|v| v.parse::<f64>().ok())
        .unwrap_or(1050.0);
    let dur = get(&flags, "dur")
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or(480);
    let rrid = get(&flags, "rrid")
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or(1320000000);
    let seed = get(&flags, "seed")
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(7);
    let mut client = match make_client() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{e}");
            return 1;
        }
    };
    let mut log = |s: &str| eprintln!("{s}");
    let anchor = match client.identity.anchor_coordinate() {
        Ok(value) => value,
        Err(e) => {
            eprintln!("定位锚点无效: {e}");
            return 1;
        }
    };
    let pts = match crate::api::points::fetch_points(&mut client, anchor, &mut log) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("点位拉取失败: {e}");
            return 1;
        }
    };
    let pts_bd = crate::api::points::points_bd(&pts);
    let start_ms = crate::crypto::envelope::now_ms() - dur * 1000;
    let track = crate::track::generator::build(
        dist,
        dur,
        seed,
        (anchor.latitude, anchor.longitude),
        start_ms,
        &pts_bd,
    );
    let sess = client.login.clone().unwrap_or_default();
    let uuid = uuid::Uuid::new_v4().to_string().to_uppercase();
    let obj = crate::track::wire::build_obs_object(&track, rrid, &uuid, sess.uid, &pts, client.identity.record_seq.max(1));
    let out = get(&flags, "out").unwrap_or("obs_ours.json");
    let decoded = decode_gz_fields(&obj);
    std::fs::write(
        out,
        serde_json::to_string_pretty(&decoded).unwrap_or_default(),
    )
    .map_err(|e| eprintln!("写入失败: {e}"))
    .ok();
    println!("已保存 {out}");
    0
}

/// 单条跑步记录原始详情。
fn cmd_record_info(rest: &[&str]) -> i32 {
    let flags = parse_flags(rest);
    let Some(rrid) = get(&flags, "rrid").and_then(|v| v.parse::<i64>().ok()) else {
        eprintln!("缺少 --rrid");
        return 1;
    };
    let mut client = match make_client() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{e}");
            return 1;
        }
    };
    match crate::api::records::fetch_one_record(&mut client, rrid, "") {
        Ok(v) => println!("{}", serde_json::to_string_pretty(&v).unwrap_or_default()),
        Err(e) => {
            eprintln!("拉取失败: {e}");
            return 1;
        }
    }
    0
}

/// 单条 AI 记录全量详情。
fn cmd_ai_info(rest: &[&str]) -> i32 {
    let flags = parse_flags(rest);
    let Some(id) = get(&flags, "id").and_then(|v| v.parse::<i64>().ok()) else {
        eprintln!("缺少 --id");
        return 1;
    };
    let mut client = match make_client() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{e}");
            return 1;
        }
    };
    match crate::api::ai::fetch_record_detail(&mut client, id) {
        Ok(v) => println!("{}", serde_json::to_string_pretty(&v).unwrap_or_default()),
        Err(e) => {
            eprintln!("拉取失败: {e}");
            return 1;
        }
    }
    0
}

fn cmd_ai_records(rest: &[&str]) -> i32 {
    let flags = parse_flags(rest);
    let Some(sport) = get(&flags, "sport").and_then(|v| v.parse().ok()) else {
        eprintln!("缺少 --sport");
        return 1;
    };
    let mut client = match make_client() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{e}");
            return 1;
        }
    };
    match crate::api::ai::fetch_records(&mut client, sport, 50) {
        Ok(page) => {
            use chrono::TimeZone;
            for g in page.groups {
                let date = chrono::Local
                    .timestamp_millis_opt(g.score_date)
                    .single()
                    .map(|t| t.format("%Y-%m-%d").to_string())
                    .unwrap_or_default();
                println!("{date} ×{}：", g.frequency);
                for r in g.records {
                    let grade = if r.rtype == 2 {
                        format!("{:.1} 秒", r.score.parse::<f64>().unwrap_or(0.0) / 1000.0)
                    } else {
                        format!("{} 个", r.score)
                    };
                    let finish = chrono::Local
                        .timestamp_millis_opt(r.score_date)
                        .single()
                        .map(|t| t.format("%H:%M:%S").to_string())
                        .unwrap_or_default();
                    let video = if r.has_video { "有" } else { "-" };
                    println!("  {} {} 完成 {finish} 视频 {video}", r.name, grade);
                }
            }
            0
        }
        Err(e) => {
            eprintln!("拉取失败: {e}");
            1
        }
    }
}

fn cmd_semester() -> i32 {
    let mut client = match make_client() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{e}");
            return 1;
        }
    };
    let mut log = logger();
    match crate::api::semester::query(&mut client, &mut log) {
        Ok(r) => {
            if let Some(s) = r.summary {
                println!(
                    "学期：{}  有效次数：{}/{}  有效里程：{:.2} km（总 {:.2} km）",
                    s.sname,
                    s.semester_valid_count,
                    s.semester_count,
                    s.semester_valid_dis / 1000.0,
                    s.semester_dis / 1000.0
                );
            }
            if !r.personal_raw.is_null() {
                println!("个人完成度：{}", r.personal_raw);
            }
            0
        }
        Err(e) => {
            eprintln!("拉取失败: {e}");
            1
        }
    }
}

fn cmd_cheat(rest: &[&str]) -> i32 {
    let flags = parse_flags(rest);
    let page: i64 = get(&flags, "page")
        .and_then(|v| v.parse().ok())
        .unwrap_or(1);
    let mut client = match make_client() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{e}");
            return 1;
        }
    };
    let mut log = logger();
    match crate::api::cheat::query(&mut client, page, &mut log) {
        Ok(r) => {
            if r.is_clean() {
                println!("自查：干净（self=null）");
            } else {
                println!("已被标记：{}", r.self_brief());
            }
            println!("全校违规 {} 条", r.list.len());
            for item in r.list.iter().take(20) {
                println!(
                    "  {} | {} | {}",
                    jstr(item, &["name", "userName"]),
                    jstr(item, &["reason", "punishReason", "cause"]),
                    jstr(item, &["createTime", "time", "date"]),
                );
            }
            0
        }
        Err(e) => {
            eprintln!("检查失败: {e}");
            1
        }
    }
}

fn cmd_rank(rest: &[&str]) -> i32 {
    let kind = rest.first().copied().unwrap_or("main");
    let flags = parse_flags(&rest.iter().skip(1).copied().collect::<Vec<_>>());
    let mut client = match make_client() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{e}");
            return 1;
        }
    };
    let res = match kind {
        "indoor" => {
            let range: i64 = get(&flags, "range")
                .and_then(|v| v.parse().ok())
                .unwrap_or(1);
            let gender: i64 = get(&flags, "gender")
                .and_then(|v| v.parse().ok())
                .unwrap_or(-1);
            crate::api::rank::indoor_rank(&mut client, range, gender)
        }
        "history" => {
            let sort: i64 = get(&flags, "sort")
                .and_then(|v| v.parse().ok())
                .unwrap_or(1);
            let gender: i64 = get(&flags, "gender")
                .and_then(|v| v.parse().ok())
                .unwrap_or(-1);
            crate::api::rank::history_rank(&mut client, sort, gender)
        }
        _ => {
            let rtype: i64 = get(&flags, "type")
                .and_then(|v| v.parse().ok())
                .unwrap_or(1);
            let sort: i64 = get(&flags, "sort")
                .and_then(|v| v.parse().ok())
                .unwrap_or(1);
            // 主榜协议里没有 gender（见 api::rank::main_rank 的取证）
            let date = get(&flags, "date").map(|v| v.to_string());
            crate::api::rank::main_rank(&mut client, rtype, sort, date)
        }
    };
    match res {
        Ok(rows) => {
            for r in rows {
                println!("{:<4} {}  {:.2} km", r.sort, r.name, r.length / 1000.0);
            }
            0
        }
        Err(e) => {
            eprintln!("查询失败: {e}");
            1
        }
    }
}

fn cmd_update(rest: &[&str]) -> i32 {
    let flags = parse_flags(rest);
    let check_only = get(&flags, "check").is_some();
    crate::update::cleanup_residue();
    let mut log = logger();
    let release = match crate::update::check_latest(&mut log) {
        Ok(r) => r,
        Err(e) => {
            println!("× 检查更新失败: {e}");
            return 1;
        }
    };
    let Some(rel) = release else {
        println!("√ 已是最新版本（v{}）", crate::update::current_version());
        return 0;
    };
    println!(
        "发现新版本 {}（当前 v{}，{:.1} MB）",
        rel.tag,
        crate::update::current_version(),
        rel.asset_size as f64 / 1024.0 / 1024.0
    );
    if !rel.notes.is_empty() {
        for line in rel.notes.lines().take(8) {
            println!("  {line}");
        }
    }
    if check_only {
        println!("仅检查（--check），未下载。下载地址：{}", rel.asset_url);
        return 0;
    }
    #[cfg(target_os = "android")]
    {
        println!("Android 版请在界面（关于页）中下载并安装更新");
        0
    }
    #[cfg(not(target_os = "android"))]
    {
        println!("开始下载 {}…", rel.asset_name);
        let mut last_pct = u64::MAX;
        let bytes = match crate::update::download(&rel.asset_url, |done, total| {
            if let Some(t) = total.filter(|t| *t > 0) {
                let pct = done * 100 / t;
                if pct != last_pct && pct % 5 == 0 {
                    println!("  {pct}%");
                    last_pct = pct;
                }
            }
        }) {
            Ok(b) => b,
            Err(e) => {
                println!("× 下载失败: {e}");
                return 1;
            }
        };
        if rel.asset_size > 0 && bytes.len() as u64 != rel.asset_size {
            println!(
                "× 下载大小不符（期望 {} 字节，实际 {} 字节），已放弃替换",
                rel.asset_size,
                bytes.len()
            );
            return 1;
        }
        println!("下载完成（{} 字节），解包替换…", bytes.len());
        let bin = match crate::update::extract(&rel.asset_name, &bytes) {
            Ok(b) => b,
            Err(e) => {
                println!("× {e}");
                return 1;
            }
        };
        match crate::update::apply(&bin) {
            Ok(()) => {
                println!("√ 已更新到 {}，请重新运行命令", rel.tag);
                0
            }
            Err(e) => {
                println!("× 替换失败: {e}");
                1
            }
        }
    }
}
