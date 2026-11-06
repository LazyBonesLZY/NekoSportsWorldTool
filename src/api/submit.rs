//! 跑步提交：POST /api/v70260/runnings/save/record。
//!
//! 只实现 iOS 7.4.0 形态：38 键 body（权威来源 `-[SWRunGradeBuilder
//! buildGradeWithCompletion:]_block_invoke`）、iOS 签名语义、iOS 请求头，
//! 并随请求发送 `runes` / `runef` / `runec` 三元组。
//!
//! 关于 `runes` / `runef` / `runec`（IDA 7.4.0 实证）：
//! - `-[SWRunGradeBuilder makeHTTPRequestExtraHeader]` @0x100279f44 只是把
//!   `SWRunRecordEntity` 的同名属性原样取出，nil 时回落字面量 `"0"`；
//! - `+[SWRunRecordEntity defaultPropertyValues]` @0x10023e2e4 把这三个属性
//!   的 Realm 默认值**都设为 CFSTR("0")**；
//! - 全二进制中这三个 setter（`-[SWRunRecordEntity setRunes:/setRunef:/setRunec:]`）
//!   及其选器字符串**没有任何代码引用**，只有 ObjC 元数据。
//!
//! 结论：真机这三个请求头恒为 `"0"`。此前按 uid/时间戳拼接的做法是无依据的
//! 伪造值（本身就是一个可识别的异常特征），已按实证改为固定 `"0"`。

use super::client::{check_business, ureq_err, ApiClient};
use super::model::HOST;
use crate::crypto::decrypt::{decrypt_response, derive_paes_key};
use crate::crypto::envelope::{
    build_envelope, build_envelope_ts, rsa_public_key, OuterOrder,
};
use crate::crypto::header::{build_native_ios_header_with_extra, ua_ios_for};
use crate::crypto::sign::{original_sign_ios, signature_ios};
use crate::track::calorie::{avg_power, display_kcal, submitted_calorie};
use crate::track::geom::round_to;
use crate::track::model::{GenPoint, Track};
use crate::track::wire::validate_five_point_wrapper;
use serde_json::{Map, Value};

pub const RECORD_PATH: &str = "/api/v70260/runnings/save/record";

/// 跑步记录请求独有的 `runes` / `runef` / `runec` 三元组。
///
/// **这三个不是常量 "0"**。记录创建块 `sub_1000D2D14` 明确写入真值
/// （`SWRunRecordEntity defaultPropertyValues` 里的 `"0"` 只是 Realm 默认值，
/// 记录一旦创建就会被覆盖）：
///
/// ```objc
/// record.runec = [SWRunConfigDataManager runecWithLocation: <启动定位>]
/// record.runef = [SWRunConfigDataManager runefWithCer: cer]   // "{cer.UUID}{cer.recordID}"
/// record.runes = [SWRunConfigDataManager runes]               // "{uid}{NSUserDefaults 时间戳}"
/// ```
///
/// - `runes`：`+[SWRunConfigDataManager runes]` @0x1000a3954 = `uid` 拼接
///   `NSUserDefaults` 键 `com.iydsj.swcampus.runes.timestamp`。
///   该键在**策略响应处理** `sub_1000A4B7C` 里被写入：
///   `[defaults setObject:[[data objectForKey:@"timestamp"] stringValue] forKey:@"…runes.timestamp"]`。
///   本工具提交前一定会拉一次策略，因此 `runes = "{uid}{policy.timestamp}"`
///   （不是裸 uid —— 早前"该键只读不写"的判断是错的）。
/// - `runef`：`+[SWRunConfigDataManager runefWithCer:]` @0x1000a3a28 =
///   `cer.UUID` 拼接 `cer.recordID`（本机记录 id）。
/// - `runec`：与点位请求同款 `runecWithLocation:`，明文是
///   `"{uid}{经度:.6}{纬度:.6}{毫秒时间戳}"`，取记录创建那一刻的定位。
pub fn run_header_extra(
    uid: i64,
    policy_timestamp: i64,
    run_uuid: &str,
    record_id: i64,
    runec: &str,
) -> Vec<(String, String)> {
    vec![
        ("runes".to_string(), format!("{uid}{policy_timestamp}")),
        ("runef".to_string(), format!("{run_uuid}{record_id}")),
        ("runec".to_string(), runec.to_string()),
    ]
}

/// iOS 7.4.0 权威 body 键集。
///
/// 来源是**两段**代码，不是一段：
/// - `sub_100272F1C`（`buildGradeWithCompletion:` 传给
///   `+[GeeGuard submitReceiptWithSignData:completion:]` 的完成块）先往 body 字典里
///   写入 `geeToken` 与 `errorCode`；
/// - `sub_1002730B4`（同一个 block_invoke）再写其余 36 键，**并在写完之后**才对
///   这个字典算签名。
///
/// 所以真机 iOS 提交体是 **38 键**，且 `geeToken`/`errorCode` **参与签名**
/// （签名输入 = 38 − 2 排除 − 5 后插 = 31 键）。此前只发 36 键，服务端重算签名的
/// 输入集就会与我们不一致。
///
/// `roomId` 只在 sportType==3（室内跑）时出现；本工具固定 sportType=1，不含该键。
/// `sessionId` 只出现在未被调用的同步版 `buildGrade`。
/// 注意 `address` 是**条件键**：`sub_1002730B4` 里
/// `if ([record.city length] != 0) setObject:city forKey:@"address"` ——
/// 城市为空时**整个键不出现**（此时 body 为 37 键），而不是发空串。
pub const IOS_BODY_KEYS: [&str; 38] = [
    "fivePointJson", "allLocJson", "sportType", "policy", "totalTime", "startTime",
    "stopTime", "getPrize", "status", "uuid", "uid", "unauthorized", "themeId",
    "selectedUnid", "selRunTime", "selDistance", "totalDis", "speed", "validDis",
    "validTime", "complete", "unCompleteReason", "calorie", "address", "goalId",
    "totalAscent", "avgPower", "totalSteps", "avgStepFreq", "useMobilityTools",
    "faceCheck", "geeToken", "errorCode",
    "segmentJson", "speedPerTenSec", "stepsPerTenSec", "signature", "originalSign",
];

/// GeeGuard 防作弊回执的取值。
///
/// 真机 `+[GeeGuard submitReceiptWithSignData:completion:]` 成功后写入
/// `respondedGeeToken` + `"200"`；失败分支写入 `""` + `"-200"`/`"-999"`。
/// 本工具无法生成极验回执，只能如实复现 App 自己的失败态：空 token + `"-999"`。
/// 这两个值参与签名，必须与 body 里实际发送的一致。
/// GeeGuard 回执三元组的选择。
///
/// `sub_100272F1C`（GeeGuard 完成块）的分支完整形状：
///
/// ```text
/// error == nil                        → geeToken = [result respondedGeeToken]; errorCode = "200"
/// error.code ∈ {-300, -500, -501}     → geeToken = [result geeToken];          errorCode = "200"
/// 其它（失败）                         → geeToken = "";  errorCode = ("-200" | "-999")
/// ```
///
/// 也就是说 **`geeToken` 为空 ⇔ `errorCode` 绝不是 `"200"`**（只可能是 `-200` 或 `-999`）。
/// 本工具无法产出 GeeGuard 回执，因此走失败分支：空 token + `-999`（未知错误）。
pub const GEE_TOKEN: &str = "";
pub const GEE_ERROR_CODE: &str = "-999";

/// 总爬升。
///
/// 真机 `-[SWRunGradeBuilder _totalAscentFromLocations]` @0x100279c38 是
/// **`max(lAltitude) − min(lAltitude)` 再与 0 取大**，不是"正增量累加"。
/// 本工具用同样口径对 `bdA`（我们唯一上传的高度字段）求极差，
/// 这样服务端按收到的 `bdA` 重算也能得到同一个值。
pub fn total_ascent(locs: &[GenPoint]) -> f64 {
    let mut it = locs.iter().map(|p| p.bdA).filter(|v| v.is_finite());
    let Some(first) = it.next() else { return 0.0 };
    let (mut lo, mut hi) = (first, first);
    for v in it {
        lo = lo.min(v);
        hi = hi.max(v);
    }
    (hi - lo).max(0.0)
}

pub struct SubmitParams {
    pub track: Track,
    pub uid: i64,
    pub selected_unid: i64,
    pub policy: i64,
    /// 提交体 `sportType`（= runMode 映射值，见 `IosBodyInput::sport_type`）。
    pub sport_type: i64,
    pub min_distance: i64,
    /// 原始 `userWeight`：只用于 calorie，缺失时必须是 0。
    pub weight: f64,
    /// `userWeightForCalorie`：只用于 avgPower，缺失时按性别回退 67/54。
    pub power_weight: f64,
    pub face_check: i64,
    pub five_point_json: String,
    pub address: String,
    /// runRuleModel.minRunTime：iOS selRunTime 用它（非 totalTime）。
    pub min_run_time: i64,
    /// 本地达标判定结果（complete / unCompleteReason）。
    pub complete: bool,
    pub uncomplete_reason: i64,
    /// 本机记录 id（真机 `cer.recordID`），用于 `runef` 与点位的 `flag`。
    pub record_id: i64,
    /// 策略响应的 `timestamp`：`runes` 的后半段就是它（见 `run_header_extra`）。
    pub policy_timestamp: i64,
}

/// 提交 body 的 `speed`（`-[SWRunGradeBuilder buildGradeWithCompletion:]`）。
///
/// 真机算法：`totalTime <= 0` 或 `totalDis < 1e-6` → `0`；
/// 否则 `sub_1000FEB7C(totalDis, totalTime)` = `totalTime/60 / (ceil(totalDis*100)/100/1000)`
/// （分/公里配速），再 `round(x, 2)`，最后 `(long)(x * 1000)`。
pub fn body_speed_ios(total_dis: f64, total_time: i64) -> i64 {
    if total_time <= 0 || total_dis <= 1e-6 || !total_dis.is_finite() {
        return 0;
    }
    let km = (total_dis * 100.0).ceil() / 100.0 / 1000.0;
    (round_to(total_time as f64 / 60.0 / km, 2) * 1000.0) as i64
}

/// iOS body 构造入参（与 -[SWRunGradeBuilder buildGradeWithCompletion:] 的取值一一对应）。
pub struct IosBodyInput {
    pub five_point_json: String,
    pub run_uuid: String,
    pub uid: i64,
    pub unid: i64,
    /// 记录体 `policy`：由 runMode 经 `indoor::policy_for` 映射（runMode 1 → 1，6 → 0）。
    pub policy: i64,
    /// 记录体 `sportType`：由 runMode 经 `indoor::sport_type_for` 映射
    /// （`record.sportType = [runCtl runType]`，同一张索引表）。
    pub sport_type: i64,
    pub total_time: i64,
    pub start_ms: i64,
    pub stop_ms: i64,
    pub min_run_time: i64,
    pub min_distance: i64,
    pub total_dis: f64,
    pub speed: i64,
    pub kcal: i64,
    pub address: String,
    pub ascent: i64,
    pub power: i64,
    pub total_steps: i64,
    pub avg_step_freq: i64,
    pub face_check: i64,
    /// 本地达标判定结果（实机由 SWRunGradeReviewImpl 计算后填入）。
    pub complete: bool,
    pub uncomplete_reason: i64,
}

/// 构造 iOS 7.4.0 提交 body。
///
/// 权威来源：-[SWRunGradeBuilder buildGradeWithCompletion:]_block_invoke
/// （App 实际路径 SWRunGradeManager.uploadGrade → buildGradeWithCompletion:）。
/// 同步版 -[SWRunGradeBuilder buildGrade] 是未被调用的旧路径，**含 sessionId、缺
/// unauthorized/themeId/address/goalId/totalAscent/avgPower**，不可据此实现。
///
/// 38 键；签名集 = 全键 − {allLocJson, fivePointJson}；
/// 签名后再插入 {segmentJson, speedPerTenSec, stepsPerTenSec, signature, originalSign}。
/// 值语义：getPrize/complete/unauthorized 为 NSString，计数类为 NSNumber。
pub fn build_ios_body(i: &IosBodyInput) -> Map<String, Value> {
    let mut body = Map::new();
    body.insert("fivePointJson".into(), Value::String(i.five_point_json.clone()));
    body.insert("allLocJson".into(), Value::String(String::new()));
    body.insert("sportType".into(), Value::from(i.sport_type));
    body.insert("policy".into(), Value::from(i.policy));
    body.insert("totalTime".into(), Value::from(i.total_time));
    body.insert("startTime".into(), Value::from(i.start_ms));
    body.insert("stopTime".into(), Value::from(i.stop_ms));
    body.insert("getPrize".into(), Value::String("false".into()));
    body.insert("status".into(), Value::from(0));
    body.insert("uuid".into(), Value::String(i.run_uuid.clone()));
    // `-[SWRunRecordEntity uid]` 的返回类型是 **NSString ***（Realm 属性），
    // 所以 body 里的 uid 是字符串，不是数字。`description` 文本相同，不影响签名，
    // 但服务端按模型解析时类型必须对得上。
    body.insert("uid".into(), Value::String(i.uid.to_string()));
    body.insert("unauthorized".into(), Value::String("0".into()));
    body.insert("themeId".into(), Value::from(0));
    // 同上：`-[SWRunRecordEntity unid]` 也是 NSString *（nil 时回退 getChosenCampusId）。
    body.insert("selectedUnid".into(), Value::String(i.unid.to_string()));
    // selRunTime = runRuleModel.minRunTime（不是 totalTime）
    body.insert("selRunTime".into(), Value::from(i.min_run_time));
    body.insert("selDistance".into(), Value::from(i.min_distance));
    // totalDis/validDis = (long)record.totalDis —— 截断
    body.insert("totalDis".into(), Value::from(i.total_dis as i64));
    body.insert("speed".into(), Value::from(i.speed));
    body.insert("validDis".into(), Value::from(i.total_dis as i64));
    body.insert("validTime".into(), Value::from(i.total_time));
    body.insert(
        "complete".into(),
        Value::String(if i.complete { "true" } else { "false" }.into()),
    );
    body.insert("unCompleteReason".into(), Value::from(i.uncomplete_reason));
    body.insert("calorie".into(), Value::from(i.kcal));
    // 条件键：`if ([record.city length] != 0) setObject:city forKey:@"address"` ——
    // 城市为空时整个键不出现（body 变 37 键），不是发空串。
    let city = i.address.trim();
    if !city.is_empty() {
        body.insert("address".into(), Value::String(city.to_string()));
    }
    body.insert("goalId".into(), Value::from(0));
    body.insert("totalAscent".into(), Value::from(i.ascent));
    body.insert("avgPower".into(), Value::from(i.power));
    body.insert("totalSteps".into(), Value::from(i.total_steps));
    body.insert("avgStepFreq".into(), Value::from(i.avg_step_freq));
    body.insert("useMobilityTools".into(), Value::from(0));
    body.insert("faceCheck".into(), Value::from(i.face_check));
    // GeeGuard 回执字段：真机在 body 组装前就已写入，因此**参与签名**。
    body.insert("geeToken".into(), Value::String(GEE_TOKEN.into()));
    body.insert("errorCode".into(), Value::String(GEE_ERROR_CODE.into()));
    // signature = MD5(sortedQueryStringWithoutEncoding + 盐)；originalSign = 同一明文
    let signed = Value::Object(body.clone());
    let sig = signature_ios(&signed);
    let orig = original_sign_ios(&signed);
    body.insert("segmentJson".into(), Value::String(String::new()));
    body.insert("speedPerTenSec".into(), Value::Array(Vec::new()));
    body.insert("stepsPerTenSec".into(), Value::Array(Vec::new()));
    body.insert("signature".into(), Value::String(sig));
    body.insert("originalSign".into(), Value::String(orig));
    assert_body_key_set(&body);
    body
}

/// 运行期键集护栏：提交前就把「键集偏离 IPA」变成硬失败。
///
/// 之前出过按错误方法实现签名与 body、却在测试里自己证明自己的事故；
/// 只在测试里断言键数不足以防住，因此这里在**每次**构造 body 时校验键名集合。
/// body 与 track 的交叉一致性：提交前硬校验，不成立就直接失败。
///
/// 这些量服务端都能用 `allLocJson` / 十秒窗独立重算一遍，body 里写错一个就会出现
/// 「同一份数据两套口径」。放在发送前，而不是只放在测试里。
fn assert_body_track_consistency(body: &Map<String, Value>, track: &crate::track::model::Track) {
    let num = |k: &str| body.get(k).and_then(|v| v.as_i64());
    let total_dis = num("totalDis").unwrap_or(-1);
    let expect_dis = track.totalDistance as i64;
    assert_eq!(total_dis, expect_dis, "body.totalDis 与轨迹不一致");
    assert_eq!(
        num("validDis"),
        Some(expect_dis),
        "body.validDis 必须等于 totalDis"
    );
    assert_eq!(
        num("totalTime"),
        Some(track.totalTime),
        "body.totalTime 与轨迹不一致"
    );
    assert_eq!(
        num("validTime"),
        Some(track.totalTime),
        "body.validTime 必须等于 totalTime"
    );
    assert_eq!(
        num("totalSteps"),
        Some(track.totalSteps),
        "body.totalSteps 与轨迹不一致"
    );
    // stopTime 必须是 startTime + totalTime*1000
    let start = num("startTime").unwrap_or(0);
    let stop = num("stopTime").unwrap_or(0);
    assert_eq!(
        stop,
        start + track.totalTime * 1000,
        "body.stopTime 必须等于 startTime + totalTime*1000"
    );
    // 末点累计量必须与顶层声明一致（validate_consistency 已保证，这里再兜一层）
    if let Some(last) = track.locations.last() {
        assert_eq!(
            last.totalDis as i64, expect_dis,
            "末点累计距离与 body.totalDis 不一致"
        );
        assert_eq!(last.steps, track.totalSteps, "末点步数与 body.totalSteps 不一致");
    }
}

fn assert_body_key_set(body: &Map<String, Value>) {
    // `address` 是条件键：城市非空时 38 键，为空时 37 键（IPA 里整键不出现）。
    let has_address = body.contains_key("address");
    let expect = if has_address {
        IOS_BODY_KEYS.len()
    } else {
        IOS_BODY_KEYS.len() - 1
    };
    debug_assert_eq!(
        body.len(),
        expect,
        "提交 body 键数 {} != IPA 的 {}（address 存在={has_address}）",
        body.len(),
        expect
    );
    for k in IOS_BODY_KEYS {
        if k == "address" && !has_address {
            continue;
        }
        assert!(body.contains_key(k), "提交 body 缺少 IPA 键 {k}");
    }
    for k in body.keys() {
        assert!(
            IOS_BODY_KEYS.contains(&k.as_str()),
            "提交 body 出现 IPA 之外的键 {k}"
        );
    }
}

pub struct SubmitResult {
    pub rrid: i64,
    pub uuid: String,
    pub start_ms: i64,
    pub total_dis: f64,
    pub total_time: i64,
    pub total_steps: i64,
    pub avg_step_freq: i64,
    pub calorie: i64,
    pub avg_power: i64,
    pub sel_distance: i64,
}

/// 提交跑步记录（sportType=1 自由跑 + 实时五点）。
pub fn submit_record(client: &mut ApiClient, p: &SubmitParams, log: &mut dyn FnMut(&str)) -> Result<SubmitResult, String> {
    let track = &p.track;
    track.validate_consistency()?;
    if p.five_point_json.is_empty() { return Err("五点轨迹不能为空".into()); }
    validate_five_point_wrapper(&p.five_point_json)?;
    let total_time = track.totalTime;
    let total_dis = track.totalDistance;
    let total_steps = track.totalSteps;
    let start_ms = track.startTime;
    let stop_ms = start_ms + total_time * 1000;
    let ascent = total_ascent(&track.locations);
    let power = avg_power(p.power_weight, total_dis, total_time);
    // 提交体 calorie 用真机公式 (long)(km × 体重 × 1.036)；MET 公式只用于界面展示。
    let kcal = submitted_calorie(p.weight, total_dis);
    // 界面估算：体重缺失（0）时用 `userWeightForCalorie` 的回退值，避免显示 0 kcal。
    let ui_kcal = display_kcal(
        if p.weight > 0.0 { p.weight } else { p.power_weight },
        total_time,
        total_dis,
    );

    let run_uuid = uuid::Uuid::new_v4().to_string().to_uppercase();
    let unid = p.selected_unid;

    // 真机 `_buildStepAndSpeedInfo`：totalSteps == 0 → avgStepFreq = 0；
    // 否则 `(unsigned long long)(totalSteps / (totalTime / 60.0))` —— FCVTZU，**截断**。
    let avg_step_freq = if total_steps == 0 || total_time <= 0 {
        0
    } else {
        (total_steps as f64 / (total_time as f64 / 60.0)) as i64
    };

    // iOS：sub_1000FEB7C → round2(分/公里) * 1000，再截断（IPA buildGradeWithCompletion）
    let speed_ios = body_speed_ios(total_dis, total_time);
    // 只用 iOS 形态。
    let body = build_ios_body(&IosBodyInput {
        five_point_json: p.five_point_json.clone(),
        run_uuid: run_uuid.clone(),
        uid: p.uid,
        unid,
        policy: p.policy,
        sport_type: p.sport_type,
        total_time,
        start_ms,
        stop_ms,
        min_run_time: p.min_run_time,
        min_distance: p.min_distance,
        total_dis,
        speed: speed_ios,
        kcal,
        address: p.address.clone(),
        ascent: ascent as i64,
        power,
        total_steps,
        avg_step_freq,
        face_check: p.face_check,
        complete: p.complete,
        uncomplete_reason: p.uncomplete_reason,
    });
    // 发送前最后一道：body 与 track 必须互相说得通（服务端会用坐标独立重算）。
    assert_body_track_consistency(&body, track);
    let body_plain = Value::Object(body).to_string();

    // 请求头一律用 iOS（设备身份全链与 policy/points/OBS 一致）。
    // 头字段集对齐 IPA -[SWRequestHeaderProvider _fillFullHeaders]（18 键）。
    // runes / runef / runec 三个真值（见 `run_header_extra` 的取证）。
    let runec_record = {
        let start = &track.locations[0];
        let plain = super::points::runec_plaintext(p.uid, start.gLng, start.gLat, start_ms);
        build_envelope(&mut client.session, &plain, OuterOrder::Observed).json
    };
    let run_extra = run_header_extra(p.uid, p.policy_timestamp, &run_uuid, p.record_id, &runec_record);
    let run_extra_ref: Vec<(&str, &str)> = run_extra
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    let (hp, hp_extra, ua): (String, Vec<(String, String)>, String) = {
        // runes/runef/runec 并入 headerSign 的加密 JSON（17 + 3 = 20 键），
        // **并且**作为独立 HTTP 头发送（见下面的取证注释）。
        let (hp, extra) = build_native_ios_header_with_extra(
            &client.identity,
            p.uid,
            &client_token(client),
            None,
            &run_extra_ref,
        );
        (hp, extra, ua_ios_for(&client.identity.os_version))
    };
    let header_env = build_envelope(&mut client.session, &hp, OuterOrder::Observed);
    // body 时间戳 = header ts + 0..=2ms（与 client.rs 一致：原生两次独立取时的自然抖动）
    let delta = (rand::Rng::gen::<u32>(&mut rand::thread_rng()) % 3) as i64;
    let body_env =
        build_envelope_ts(&mut client.session, &body_plain, OuterOrder::Insert, header_env.ts_ms + delta);

    // 与普通业务请求共用同一套通用头，避免两条链的头集合不一致。
    let mut req = super::client::apply_ios_headers(
        client.agent.post(&format!("{HOST}{RECORD_PATH}")),
        &ua,
        &header_env.json,
        true,
    );

    // `-[SWRunGradeManager uploadRunScore:]` 的请求构造块 `sub_1000483CC`：
    //   setURL(saveSportRecord) → setHTTPMethod:1 → setParameters(body)
    //   → 把 `makeHTTPRequestExtraHeader` 的**每个键** setValue:forHTTPHeaderField:
    // 也就是说 `runes`/`runef`/`runec` 既并入 headerSign（`requestRecordHeaderDict:`），
    // 又作为独立 HTTP 头发出。第一轮误以为只进 headerSign 而删掉了这三个头，现已恢复。
    for (k, v) in &run_extra {
        req = req.set(k, v);
    }
    for (k, v) in &hp_extra {
        req = req.set(k, v);
    }
    let resp = req.send_string(&body_env.json).map_err(ureq_err)?;
    let status = resp.status();
    let raw = resp.into_string().unwrap_or_default();
    log(&format!(
        "[record] sportType={} HTTP {status} len={}",
        p.sport_type,
        raw.len()
    ));

    let key = derive_paes_key(
        &body_env.key_data[0], &body_env.key_data[1], &body_env.key_data[2], &body_env.key_data[3],
    );
    let dec = decrypt_response(raw.as_bytes(), &key, &rsa_public_key())
        .map_err(|e| format!("提交响应解密失败: {e}"))?;
    let biz = check_business(&dec.business)?;
    log(&format!(
        "[record] 服务端响应 {}",
        super::client::safe_truncate(
            &crate::textlog::redact_json(&biz).to_string(),
            600
        )
    ));
    let rrid = super::client::get_field(&biz, "rrid").and_then(|v| v.as_i64()).unwrap_or(0);
    if rrid <= 0 {
        return Err(format!("提交未返回 rrid: {}", truncate_json(&biz)));
    }
    log(&format!("√ 提交成功 rrid={rrid} uuid={run_uuid}"));
    // 本地判定 vs 服务端判定：这是「是否全绿」最直接的信号。
    // 两者不一致时服务端会把记录记为异常，必须让用户当场看到。
    let server_complete = super::client::get_field(&biz, "complete").and_then(|v| v.as_bool());
    if let Some(server) = server_complete {
        if server != p.complete {
            log(&format!(
                "⚠ 判定不一致：本地 complete={}（reason={}），服务端 complete={}。\
                 该记录可能被计为异常，请核对轨迹与打卡点。",
                p.complete, p.uncomplete_reason, server
            ));
        } else if !server {
            log("⚠ 服务端判定为不达标（complete=false），本次记录不会全绿");
        }
    }
    for key in ["isCheat", "cheatType", "abnormal", "score"] {
        if let Some(v) = super::client::get_field(&biz, key) {
            log(&format!("[record] {key}={v}"));
        }
    }
    Ok(SubmitResult {
        rrid,
        uuid: run_uuid,
        start_ms,
        total_dis,
        total_time,
        total_steps,
        avg_step_freq,
        calorie: ui_kcal,
        avg_power: power,
        sel_distance: p.min_distance,
    })
}

fn client_token(client: &ApiClient) -> String {
    client.login.as_ref().map(|s| s.token.clone()).unwrap_or_default()
}

fn truncate_json(v: &Value) -> String {
    let s = v.to_string();
    s.chars().take(240).collect()
}

#[cfg(test)]
mod ios_body_shape_tests {
    use super::*;

    fn sample_input() -> IosBodyInput {
        IosBodyInput {
            five_point_json: "{\"useZip\":false,\"fivePointJson\":\"[]\"}".into(),
            run_uuid: "TEST-UUID".into(),
            uid: 17837509,
            unid: 47203,
            policy: 1,
            sport_type: 1,
            total_time: 540,
            start_ms: 1_793_851_519_462,
            stop_ms: 1_793_852_059_462,
            min_run_time: 300,
            min_distance: 1000,
            total_dis: 1200.0,
            speed: 7500,
            kcal: 87,
            address: "江门市".into(),
            ascent: 5,
            power: 166,
            total_steps: 1200,
            avg_step_freq: 133,
            face_check: 1,
            complete: true,
            uncomplete_reason: 0,
        }
    }

    /// 用真实构造函数产出 body，键集必须等于 IPA 常量集。
    #[test]
    fn real_builder_produces_exact_ipa_key_set() {
        let body = build_ios_body(&sample_input());
        let mut got: Vec<&str> = body.keys().map(|s| s.as_str()).collect();
        got.sort_unstable();
        let mut want: Vec<&str> = IOS_BODY_KEYS.to_vec();
        want.sort_unstable();
        assert_eq!(got, want, "构造出的 body 键集必须等于 IPA 键集");
        assert_eq!(got.len(), 38);
    }

    /// IPA 关键语义断言（防止再次误改）。
    #[test]
    fn ipa_semantics_pinned() {
        let body = build_ios_body(&sample_input());
        // 1) 必须发 originalSign（与 signature 同源明文 query）
        assert!(body.contains_key("originalSign"), "iOS 必须发 originalSign");
        // 2) iOS 不发 sessionId（那是未调用的同步版 buildGrade 才有）
        assert!(!body.contains_key("sessionId"), "iOS 不发 sessionId");
        // 3) 字符串型字段
        assert_eq!(body["getPrize"], Value::String("false".into()));
        assert_eq!(body["complete"], Value::String("true".into()));
        assert_eq!(body["unauthorized"], Value::String("0".into()));
        // 4) selRunTime = minRunTime，不是 totalTime
        assert_eq!(body["selRunTime"], Value::from(300));
        assert_eq!(body["selDistance"], Value::from(1000));
        // 5) totalDis 截断
        assert_eq!(body["totalDis"], Value::from(1200));
        // 6) uid / selectedUnid 是字符串（`-[SWRunRecordEntity uid]` / `[unid]` 返回 NSString *）
        assert!(body["uid"].is_string(), "uid 必须是字符串");
        assert!(body["selectedUnid"].is_string(), "selectedUnid 必须是字符串");
        assert!(body["sportType"].is_number(), "sportType 是 NSNumber");
        assert!(body["totalDis"].is_number(), "totalDis 是 NSNumber");
        assert!(body["uuid"].is_string() && body["address"].is_string());

        // 7) 签名后插入的键
        assert_eq!(body["segmentJson"], Value::String(String::new()));
        assert_eq!(body["speedPerTenSec"].as_array().unwrap().len(), 0);

        // speed = (long)(round(分/公里, 2) * 1000)：1200m/540s → 9/1.2 = 7.5 min/km → 7500
        assert_eq!(super::body_speed_ios(1200.0, 540), 7500);
        // 3000m/1200s → 20/3 = 6.6667 → round 6.67 → 6670
        assert_eq!(super::body_speed_ios(3000.0, 1200), 6670);
        // 边界：时长/距离非法一律 0
        assert_eq!(super::body_speed_ios(1200.0, 0), 0);
        assert_eq!(super::body_speed_ios(0.0, 540), 0);
    }

    /// 服务端验签重算：去掉后插 5 键与排除 2 键 → 重算 MD5 必须等于 signature；
    /// 且 originalSign 必须等于同一明文串。
    #[test]
    fn server_recomputation_matches_signature_and_original_sign() {
        let body = build_ios_body(&sample_input());
        let signed = body["signature"].as_str().unwrap().to_string();
        let orig = body["originalSign"].as_str().unwrap().to_string();

        let mut received = body.clone();
        for k in ["segmentJson", "speedPerTenSec", "stepsPerTenSec", "signature", "originalSign"] {
            received.remove(k);
        }
        let recomputed = signature_ios(&Value::Object(received.clone()));
        assert_eq!(recomputed, signed, "服务端重算必须等于 signature");

        // originalSign 是同一 query 明文
        let expected_orig = original_sign_ios(&Value::Object(received));
        assert_eq!(orig, expected_orig, "originalSign 必须等于签名输入明文");
        assert_eq!(
            signed,
            crate::crypto::envelope::md5_hex(format!("{orig}{}", crate::crypto::sign::SALT).as_bytes()),
            "signature 必须是 MD5(originalSign + 盐)"
        );
    }

    /// 签名输入键数 = 38 − 2(排除) − 5(后插) = 31，且不含已废弃的旧平台专有键。
    #[test]
    fn signed_key_count_and_no_android_only_keys() {
        let body = build_ios_body(&sample_input());
        let signed: Vec<&String> = body.keys()
            .filter(|k| !crate::crypto::sign::IOS_SIGN_EXCLUDED.contains(&k.as_str()))
            .filter(|k| !["segmentJson","speedPerTenSec","stepsPerTenSec","signature","originalSign"].contains(&k.as_str()))
            .collect();
        assert_eq!(signed.len(), 31, "签名输入应为 31 键");
        // geeToken/errorCode 是 iOS 真机也发的（GeeGuard 回执），必须参与签名
        for k in ["geeToken", "errorCode"] {
            assert!(body.contains_key(k), "iOS body 必须含 GeeGuard 回执字段 {k}");
        }
        for k in ["isUpload", "more", "latitude", "longitude",
                  "maxRunTime", "minSteps", "sessionId", "roomId"] {
            assert!(!body.contains_key(k), "iOS body 不应含旧平台专有键 {k}");
        }
    }

    /// `address` 是条件键：IPA 里 `if ([record.city length] != 0)` 才写入，
    /// 城市为空时整个键不出现（body 变 37 键），不是发空串。
    #[test]
    fn address_is_a_conditional_key() {
        let mk = |city: &str| {
            let mut i = sample_input();
            i.address = city.to_string();
            super::build_ios_body(&i)
        };
        let with = mk("江门市");
        assert_eq!(with.len(), super::IOS_BODY_KEYS.len());
        assert_eq!(with["address"], "江门市");

        for empty in ["", "   "] {
            let without = mk(empty);
            assert_eq!(
                without.len(),
                super::IOS_BODY_KEYS.len() - 1,
                "城市为空时应少一个 address 键"
            );
            assert!(!without.contains_key("address"), "城市为空时不得写 address");
        }
        // 尾随空白应被裁掉而不是原样发送
        assert_eq!(mk(" 江门市 ")["address"], "江门市");
    }

    /// `sub_100272F1C` 的不变量：`geeToken` 为空时 `errorCode` 不得是 `"200"`。
    /// 只有拿到真实回执（error 为空或 code ∈ {-300,-500,-501}）才会同时给出 token 与 200。
    #[test]
    fn gee_token_and_error_code_are_consistent() {
        let tok = super::GEE_TOKEN;
        let code = super::GEE_ERROR_CODE;
        if tok.is_empty() {
            assert!(
                code == "-200" || code == "-999",
                "空 geeToken 时 errorCode 不得为 {code}"
            );
        } else {
            assert_eq!(code, "200", "有 geeToken 时 errorCode 必须是 200");
        }
    }

    fn sample_track() -> crate::track::model::Track {
        let pts = [(22.411290, 112.940320), (22.411343, 112.941001),
                   (22.410866, 112.941100), (22.410582, 112.941216),
                   (22.410823, 112.940376)];
        crate::track::generator::build(1200.0, 540, 2026, (22.4109, 112.9407), 1_793_840_000_000, &pts)
    }

    /// 正例：生成器产出的 track 与按它构造的 body 必须通过交叉校验。
    #[test]
    fn body_track_guard_passes_on_generated_track() {
        let track = sample_track();
        let mut i = sample_input();
        i.total_time = track.totalTime;
        i.total_dis = track.totalDistance;
        i.total_steps = track.totalSteps;
        i.start_ms = track.startTime;
        i.stop_ms = track.startTime + track.totalTime * 1000;
        let body = super::build_ios_body(&i);
        super::assert_body_track_consistency(&body, &track);
    }

    /// 反例：总距离被改掉后必须立刻失败（服务端会用坐标重算）。
    #[test]
    #[should_panic(expected = "body.totalDis")]
    fn body_track_guard_rejects_wrong_total_dis() {
        let track = sample_track();
        let mut i = sample_input();
        i.total_time = track.totalTime;
        i.total_dis = track.totalDistance + 50.0; // 故意写错
        i.total_steps = track.totalSteps;
        i.start_ms = track.startTime;
        i.stop_ms = track.startTime + track.totalTime * 1000;
        let body = super::build_ios_body(&i);
        super::assert_body_track_consistency(&body, &track);
    }

    /// 反例：stopTime 与 startTime+totalTime*1000 不一致必须失败。
    #[test]
    #[should_panic(expected = "stopTime")]
    fn body_track_guard_rejects_wrong_stop_time() {
        let track = sample_track();
        let mut i = sample_input();
        i.total_time = track.totalTime;
        i.total_dis = track.totalDistance;
        i.total_steps = track.totalSteps;
        i.start_ms = track.startTime;
        i.stop_ms = track.startTime + track.totalTime * 1000 + 5_000;
        let body = super::build_ios_body(&i);
        super::assert_body_track_consistency(&body, &track);
    }
}
