//! 命令行入口：子命令分发 + 共享助手。
//! 子命令实现见 `cmds.rs`；数据文件与 GUI 共用（exe 同目录）。
//!
//! 青龙定时任务：首次 `login --user .. --pass .. --remember`，之后定时 `run`
//! 即可（会话失效且已记住凭据时自动重登）。

mod cmds;

use crate::api::client::ApiClient;
use crate::api::model::{self, Session};
use crate::api::records::RecordRow;

pub fn main(args: Vec<String>) -> i32 {
    cmds::dispatch(args)
}

fn usage() {
    println!(
        r#"NekoSportsWorldTool CLI（full 构建无参数启动 GUI）

  login   --user <手机号> --pass <密码> [--remember]
                                           登录并保存会话；--remember 同时保存凭据供自动重登
  logout                                   登出并清理本地会话
  run    [--mode N] [--dist km] [--pace 秒/km] [--altitude 米或min-max] [--ago 分钟]
         [--days-ago 0-3 --time HH:MM] [--face 0|1] [--seed n] [--route legacy|road]
         [--dry-run] [--force]
                                           --mode：0随机跑 1顺序跑(校园跑,默认) 2定向跑 3约跑
                                           4自由跑 6户外跑（5 是室内，用 indoor 子命令）
                                           --dry-run 只生成轨迹+自检不提交；--force 跳过当天重复检查
  indoor [--steps N] [--dur S] [--seed N] [--ago MIN]  室内跑（纯步数，无轨迹/无对时闸门）
  protocol [--json]  打印全部协议参数（用于与真机抓包逐项 diff）
  policy [--mode N] [--json]
                                           只读拉取 /api/v70103/runModePolicy 原文摘要
                                           （N=1 计分/校园跑，5 室内；用来核对其他模式是否同构）
                                           跑步全链：策略-点位-轨迹-提交-OBS-验证（--route 选择路线算法）
                                           默认跑完即上传；--time 的未来时刻按当前时刻提交；--ago N 表示记录结束于 N 分钟前（补录，会告警）
                                           --face 缺省沿用界面保存的 face_check；仅当策略 faceVerify=true 时生效，未开启核验的场地按实机发 0
  template --file <GPX/JSON>               本地读取真实记录，分析海拔（不会上传）
  ai     --sport <id> [--mode min|count] [--score n] [--minutes n] [--count n]
                                           AI 运动：min 按分钟 1-30；count 按次 5-1000（步长 5）；--minutes/--count 为别名
  ai-list                                  AI 运动项目列表
  ai-records --sport <id>                  AI 运动记录（按天分组）
  ai-info --id <记录 id>                   AI 记录全量详情
  records                                  跑步记录列表
  records-raw [--body json] [--out 文件]   记录列表原始探测（自定义请求体）
  record-info --rrid <id>                  单条跑步记录原始详情
  obs-get --key <obs key> [--out 文件]     拉取 OBS 对象并解码 gzip+base64
  obs-sample [--dist m] [--dur s] [--rrid n] [--seed n] [--out 文件]
                                           本地生成一份 OBS 对象样本（不提交）
  semester                                 学期完成度
  cheat  [--page n]                        违规自查
  rank   main   --type 1|2|3 --sort 1|2 [--date YYYY-MM-DD]   （主榜协议无 gender）
                                           排行榜（1个人 2班级 3院系；1日 2月）
  rank   indoor --range 1|2|3 [--gender 0|1]
                                           室内榜（1日 2周 3月）
  rank   history --sort 1|2 [--gender 0|1]  历史榜
  update [--check]                        检查更新；默认下载并自替换（--check 仅检查）

当天榜单通常在有效里程产生后才有数据；run 默认随机 1.0~1.5 km / 6~8 分配速、跑完即上传。
服务端仅允许 06:30-22:00 计分跑（policy.validTime），窗口外会返回 11016。"#
    );
}

pub(crate) fn parse_flags(rest: &[&str]) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < rest.len() {
        let k = rest[i].trim_start_matches('-').to_string();
        let v = rest
            .get(i + 1)
            .filter(|n| !n.starts_with('-') && *n != &"")
            .map(|s| s.to_string());
        match v {
            Some(v) => {
                out.push((k, v));
                i += 2;
            }
            None => {
                out.push((k, "1".into()));
                i += 1;
            }
        }
    }
    out
}

pub(crate) fn get<'a>(flags: &'a [(String, String)], name: &str) -> Option<&'a str> {
    flags
        .iter()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.as_str())
}

pub(crate) fn logger() -> impl FnMut(&str) {
    |s: &str| println!("{}", crate::textlog::clean(s))
}

pub(crate) fn silent() -> impl FnMut(&str) {
    |_s: &str| {}
}

/// 自动登录（GUI 记住密码 / login --remember 落盘的凭据）。
pub(crate) fn auto_login(identity: &model::HeaderIdentity) -> Result<Session, String> {
    let cfg = model::load_config();
    if cfg.remember && !cfg.username.is_empty() && !cfg.password.is_empty() {
        println!("[login] 使用已保存的账号自动登录…");
        let mut client = ApiClient::new(identity.clone(), None);
        return crate::api::login::login(&mut client, &cfg.username, &cfg.password, &mut silent());
    }
    Err("未登录：先执行 login 子命令，或在 GUI 勾选记住密码".into())
}

/// 构造已登录客户端；本地会话失效时自动重登一次。
pub(crate) fn make_client() -> Result<ApiClient, String> {
    // 身份读不出来就停：空 DeviceId 会让每个请求换一个随机设备号（10121）。
    let identity = model::try_load_identity().map_err(|e| {
        format!("设备身份不可用：{e}\n请备份并删除 identity.json 后重试（会重新生成 DeviceId/IDFA）")
    })?;
    let client = {
        let sess = model::load_session();
        if sess.is_logged_in() {
            ApiClient::new(identity, Some(sess))
        } else {
            let s = auto_login(&identity)?;
            ApiClient::new(identity, Some(s))
        }
    };
    // 不做前置健康检查：业务错误（如新账号无学期数据）不等于会话过期，
    // 贸然重登会把好 session 删掉并触发 10121。会话真正过期时业务接口
    // 自然返回 401，由调用方处理。
    Ok(client)
}

pub(crate) fn now_ms() -> i64 {
    crate::crypto::envelope::now_ms()
}

pub(crate) fn fmt_hms(ms: i64) -> String {
    use chrono::TimeZone;
    chrono::Local
        .timestamp_millis_opt(ms)
        .single()
        .map(|t| t.format("%Y-%m-%d %H:%M:%S").to_string())
        .unwrap_or_default()
}

pub(crate) fn print_rows(rows: &[RecordRow]) {
    println!(
        "{:<16} {:>8} {:>8} {:>6} {:>6} {:>4} rrid",
        "时间", "距离m", "时长", "配速", "步频", "达标"
    );
    for r in rows {
        let full = fmt_hms(r.start_time);
        let t = full.get(5..).unwrap_or("").to_string();
        let pace = if r.total_dis > 0.0 && r.total_time > 0 {
            let p = r.total_time as f64 / (r.total_dis / 1000.0);
            format!("{}:{:02}", (p / 60.0) as i64, (p as i64) % 60)
        } else {
            "-".into()
        };
        println!(
            "{:<16} {:>8.0} {:>8} {:>6} {:>6} {:>4} {}",
            t,
            r.total_dis,
            format!(
                "{}:{:02}:{:02}",
                r.total_time / 3600,
                r.total_time % 3600 / 60,
                r.total_time % 60
            ),
            pace,
            r.avg_step_freq,
            if r.complete { "是" } else { "否" },
            r.rrid
        );
    }
}

pub(crate) fn jstr(v: &serde_json::Value, keys: &[&str]) -> String {
    for k in keys {
        match v.get(k) {
            Some(serde_json::Value::String(s)) => return s.clone(),
            Some(serde_json::Value::Number(n)) => return n.to_string(),
            _ => {}
        }
    }
    "-".into()
}

// ── 开始时刻语义：UI（跑步页）与 CLI（run）共用，禁止各写一份 ──────────
//
// 两处曾经各有一份公式，结果 GUI 会把「指定时刻」钳制到当前时刻、CLI 不会，
// 于是同一份配置从命令行跑出来是「未来开跑」。统一放这里，调用方只传 `now`。

/// 随机开跑时刻窗口（含端点）：7:00-20:00。
pub const RAND_HOUR_LO: u32 = 7;
pub const RAND_HOUR_HI: u32 = 20;
/// flow 提交时会给 startTime 加 0-4s 抖动，抽样/钳制时预留，避免「开始 + 用时」越过当前时刻。
pub const JITTER_MARGIN_MS: i64 = 5_000;

/// 开跑时刻上限：当前时刻 - 用时 - 抖动余量。
pub fn latest_start_ms(now: i64, dur_s: i64) -> i64 {
    now - dur_s * 1000 - JITTER_MARGIN_MS
}

/// 指定日期（days_ago 天前）7:00-20:00 内均匀抽样，且不晚于 `latest_ms`。
///
/// 该日窗口整体晚于 `latest_ms`（例如凌晨选「今天」）时退化为 `latest_ms`：
/// 宁可贴近当前时间，也不产生「未来开跑」。
pub fn random_start_ms(now: i64, days_ago: i64, latest_ms: i64) -> i64 {
    use chrono::{Datelike, Duration, Local, TimeZone};
    let day = Local
        .timestamp_millis_opt(now)
        .single()
        .unwrap_or_else(Local::now)
        - Duration::days(days_ago.clamp(0, 3));
    let lo = Local
        .with_ymd_and_hms(day.year(), day.month(), day.day(), RAND_HOUR_LO, 0, 0)
        .single();
    let hi = Local
        .with_ymd_and_hms(day.year(), day.month(), day.day(), RAND_HOUR_HI, 0, 0)
        .single();
    let (lo_ms, hi_ms) = match (lo, hi) {
        (Some(l), Some(h)) => (l.timestamp_millis(), h.timestamp_millis()),
        _ => return latest_ms,
    };
    let hi_ms = hi_ms.min(latest_ms);
    let lo_ms = lo_ms.min(hi_ms);
    let span = (hi_ms - lo_ms).max(0) as f64;
    lo_ms + (rand::random::<f64>() * span) as i64
}

/// `--ago N` 补录：记录结束于 N 分钟前，因此起点再往前推一个用时。
///
/// 旧实现把 `--ago` 当成「起点在 N 分钟前」，用时一长 stopTime 就落到未来。
pub fn backfill_start_ms(now: i64, ago_min: i64, dur_s: i64) -> i64 {
    now - ago_min * 60_000 - dur_s * 1000
}

/// 用户填写的时刻对应的真实时间戳（**不**做未来钳制，便于调用方判断是否被钳制）。
pub fn specified_typed_ms(now: i64, days_ago: i64, hour: i64, minute: i64) -> i64 {
    use chrono::{Datelike, Duration, Local, TimeZone};
    let day = Local
        .timestamp_millis_opt(now)
        .single()
        .unwrap_or_else(Local::now)
        - Duration::days(days_ago.clamp(0, 3));
    let h = hour.clamp(0, 23) as u32;
    let m = minute.clamp(0, 59) as u32;
    Local
        .with_ymd_and_hms(day.year(), day.month(), day.day(), h, m, 0)
        .single()
        .map(|t| t.timestamp_millis())
        .unwrap_or(now)
}

/// 统一钳制：任何来源的开始时刻都必须满足「开始 + 用时 + 抖动 ≤ 当前时刻」。
///
/// 只钳开始时刻（`.min(now)`）是不够的：`start = now` 时整段记录会落到未来，
/// 服务端的时间窗（06:30-22:00）与「跑完即上传」的时序都会把它判成异常。
pub fn clamp_start_ms(now: i64, start_ms: i64, dur_s: i64) -> i64 {
    start_ms.min(latest_start_ms(now, dur_s))
}

/// 指定时刻（days_ago 天前 + 时/分），并保证整段记录不落在未来。
///
/// 用户填的过去时刻原样保留；只有「结束时刻越过当前时刻」时才前移到
/// `now - dur - 5s`（调用方负责在日志里说明被前移）。
pub fn specified_start_ms(now: i64, days_ago: i64, hour: i64, minute: i64, dur_s: i64) -> i64 {
    clamp_start_ms(now, specified_typed_ms(now, days_ago, hour, minute), dur_s)
}

pub(crate) fn parse_pace(v: &str) -> f32 {
    if let Some((m, s)) = v.split_once(':') {
        m.parse::<f32>().unwrap_or(0.0) * 60.0 + s.parse::<f32>().unwrap_or(0.0)
    } else {
        v.parse().unwrap_or(0.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `--time` 指向未来时，整段记录都不得落在未来（起点前移到 now-dur-5s）。
    #[test]
    fn specified_future_is_clamped_before_now() {
        let now = chrono::Local::now().timestamp_millis();
        let dur = 600;
        let start = specified_start_ms(now, 0, 23, 59, dur);
        assert_eq!(start, latest_start_ms(now, dur));
        assert!(start + dur * 1000 <= now, "整段记录仍落在未来");
    }

    /// 昨天填的指定时刻是过去时间，必须原样保留（离现在足够远，不触发钳制）。
    #[test]
    fn specified_past_is_kept() {
        let now = chrono::Local::now().timestamp_millis();
        let typed = specified_typed_ms(now, 2, 7, 30);
        assert!(typed < now);
        assert_eq!(specified_start_ms(now, 2, 7, 30, 600), typed);
    }

    /// 用户填的过去时刻若离现在太近（结束会越过 now），必须前移。
    #[test]
    fn specified_past_too_close_is_moved_earlier() {
        let now = 1_700_000_000_000i64;
        let typed = now - 60_000; // 1 分钟前
        let dur = 600; // 10 分钟
        let start = clamp_start_ms(now, typed, dur);
        assert_eq!(start, latest_start_ms(now, dur));
        assert!(start + dur * 1000 <= now);
    }

    /// 随机时刻落在 7:00-20:00，且不晚于上限。
    #[test]
    fn random_start_respects_window_and_latest() {
        let now = chrono::Local::now().timestamp_millis();
        let latest = latest_start_ms(now, 600);
        for _ in 0..50 {
            let start = random_start_ms(now, 1, latest);
            assert!(start <= latest, "抽样越过上限");
        }
    }

    /// 上限已经把用时与抖动余量扣掉。
    #[test]
    fn latest_start_reserves_duration_and_jitter() {
        let now = 10_000_000;
        assert_eq!(latest_start_ms(now, 600), now - 600_000 - JITTER_MARGIN_MS);
    }

    /// 补录模式：stopTime 永远落在过去。
    #[test]
    fn backfill_never_ends_in_the_future() {
        let now = 10_000_000;
        for ago in [1, 5, 60] {
            for dur in [0, 60, 1800] {
                let start = backfill_start_ms(now, ago, dur);
                assert!(start + dur * 1000 <= now, "ago={ago} dur={dur} 结束于未来");
            }
        }
    }
}
