//! 跑步提交：POST /api/v70260/runnings/save/record。
//! Android 身份 + runes/runef 头；body 31+ 字段 + signature/originalSign。

use super::client::{check_business, ureq_err, ApiClient};
use super::model::HOST;
use crate::crypto::decrypt::{decrypt_response, derive_paes_key};
use crate::crypto::envelope::{
    build_envelope, build_envelope_ts, rsa_public_key, OuterOrder,
};
use crate::crypto::header::{
    build_android_header, build_native_ios_header, ua_ios_for, HeaderIdentity, UA_ANDROID,
};
use crate::crypto::sign::{original_sign, original_sign_ios, signature, signature_ios};
use crate::track::calorie::{avg_power, official_kcal};
use crate::track::geom::round_to;
use crate::track::model::{GenPoint, Track};
use crate::track::wire::validate_five_point_wrapper;
use serde_json::{json, Map, Value};

pub const RECORD_PATH: &str = "/api/v70260/runnings/save/record";

/// Android 10s 窗（id 种子 60000）。
fn android_tensec(track: &Track, start_ms: i64, kind: &str) -> Vec<Value> {
    let locs = &track.locations;
    let total_time = track.totalTime;
    let mut out = Vec::new();
    let rid_seed = 60000i64;
    let mut w = 10i64;
    while w <= total_time {
        let lo = w - 10;
        let hi = w.min(total_time);
        let mut d_lo = 0.0f64;
        let mut s_lo = 0i64;
        let mut d_hi = 0.0f64;
        let mut s_hi = 0i64;
        for p in locs {
            let tt = p.totalTime;
            if tt <= lo {
                d_lo = p.totalDis;
                s_lo = p.steps;
            }
            if tt <= hi {
                d_hi = p.totalDis;
                s_hi = p.steps;
            }
        }
        let dist = round_to((d_hi - d_lo).max(0.0), 4);
        let steps_n = (s_hi - s_lo).max(0);
        let begin = start_ms + lo * 1000;
        let end = start_ms + hi * 1000;
        let qn = w / 10 - 1;
        if kind == "speed" {
            out.push(json!({
                "beginTime": begin, "distance": dist, "endTime": end,
                "flag": start_ms, "id": rid_seed + qn, "queueNum": qn, "state": 0,
            }));
        } else {
            out.push(json!({
                "avgDiff": 0.0, "beginTime": begin, "endTime": end,
                "flag": start_ms, "id": rid_seed + qn, "maxDiff": 0.0,
                "minDiff": 1000.0, "queueNum": qn, "state": 0, "stepsNum": steps_n,
            }));
        }
        w += 10;
    }
    out
}

/// bdA 正差分累计。
pub fn total_ascent(locs: &[GenPoint]) -> f64 {
    let mut ascent = 0.0;
    for i in 1..locs.len() {
        let d = locs[i].bdA - locs[i - 1].bdA;
        if d > 0.0 {
            ascent += d;
        }
    }
    ascent
}

pub struct SubmitParams {
    pub track: Track,
    pub uid: i64,
    pub selected_unid: i64,
    pub policy: i64,
    pub policy_ts: i64,
    pub min_distance: i64,
    pub weight: f64,
    pub face_check: i64,
    pub five_point_json: String,
    pub address: String,
}

#[allow(dead_code)]
pub struct SubmitResult {
    pub rrid: i64,
    pub uuid: String,
    pub start_ms: i64,
    pub complete: Option<bool>,
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
    let start_coordinate = track.validate_consistency()?;
    if p.five_point_json.is_empty() { return Err("五点轨迹不能为空".into()); }
    validate_five_point_wrapper(&p.five_point_json)?;
    let total_time = track.totalTime;
    let total_dis = track.totalDistance;
    let total_steps = track.totalSteps;
    let start_ms = track.startTime;
    let stop_ms = start_ms + total_time * 1000;
    let ascent = total_ascent(&track.locations);
    let power = avg_power(p.weight, total_dis, total_time);
    let kcal = official_kcal(p.weight, total_time, total_dis);

    let run_uuid = uuid::Uuid::new_v4().to_string().to_uppercase();
    let unid = p.selected_unid;

    let dis_ceil = (total_dis * 100.0).ceil() / 100.0;
    let speed = (round_to(total_time as f64 / dis_ceil * 50.0 / 3.0, 2) * 1024.0) as i64;
    let avg_step_freq = 1i64.max(round_to(total_steps as f64 / total_time as f64 * 60.0, 0) as i64);

    let mut body = Map::new();
    // 提交身份形态：ios（默认，7.4.0 实机 29 键 body + iOS 签名）| android（历史实测兼容形态）
    let ios_form = crate::api::model::load_config().submit_form.eq_ignore_ascii_case("ios");
    if ios_form {
        // iOS 7.4.0 buildGrade 键序（保持插入序对齐实机）。
        // 值类型：getPrize/unauthorized/complete 为字符串；goalId 为数字；address 为城市或空串。
        body.insert("fivePointJson".into(), Value::String(p.five_point_json.clone()));
        body.insert("allLocJson".into(), Value::String(String::new()));
        body.insert("sportType".into(), Value::from(1));
        body.insert("policy".into(), Value::from(p.policy));
        body.insert("totalTime".into(), Value::from(total_time));
        body.insert("startTime".into(), Value::from(start_ms));
        body.insert("stopTime".into(), Value::from(stop_ms));
        body.insert("getPrize".into(), Value::String("false".into()));
        body.insert("status".into(), Value::from(0));
        body.insert("uuid".into(), Value::String(run_uuid.clone()));
        body.insert("uid".into(), Value::from(p.uid));
        body.insert("unauthorized".into(), Value::String("0".into()));
        body.insert("themeId".into(), Value::from(0));
        body.insert("selectedUnid".into(), Value::from(unid));
        body.insert("selRunTime".into(), Value::from(total_time));
        body.insert("selDistance".into(), Value::from(p.min_distance));
        body.insert("totalDis".into(), Value::from(round_to(total_dis, 0) as i64));
        body.insert("speed".into(), Value::from(speed));
        body.insert("validDis".into(), Value::from(round_to(total_dis, 0) as i64));
        body.insert("validTime".into(), Value::from(total_time));
        body.insert("complete".into(), Value::String("true".into()));
        body.insert("unCompleteReason".into(), Value::from(0));
        body.insert("calorie".into(), Value::from(kcal));
        body.insert("address".into(), Value::String(p.address.trim().to_string()));
        body.insert("goalId".into(), Value::from(0));
        body.insert("totalAscent".into(), Value::from(round_to(ascent, 0) as i64));
        body.insert("avgPower".into(), Value::from(power));
        body.insert("totalSteps".into(), Value::from(total_steps));
        body.insert("avgStepFreq".into(), Value::from(avg_step_freq));
        body.insert("useMobilityTools".into(), Value::from(0));
        body.insert("faceCheck".into(), Value::from(p.face_check));
        // iOS 签名：全部键 − 排除集（allLocJson/fivePointJson 等 7 键），大小写敏感排序
        let body_val = Value::Object(body.clone());
        let sig = signature_ios(&body_val);
        let orig = original_sign_ios(&body_val);
        body.insert("signature".into(), Value::String(sig));
        body.insert("originalSign".into(), Value::String(orig));
        // 签名后插入：10s 窗数组恒空（真实数据走 OBS speed_json/step_freq_json）
        body.insert("segmentJson".into(), Value::String(String::new()));
        body.insert("speedPerTenSec".into(), Value::Array(Vec::new()));
        body.insert("stepsPerTenSec".into(), Value::Array(Vec::new()));
    } else {
        // Android 兼容形态：31+ 字段超集 + UploadSignEntity 白名单签名（历史实测通过）
        body.insert("allLocJson".into(), Value::String(String::new()));
        body.insert("sportType".into(), Value::from(1));
        body.insert("policy".into(), Value::from(p.policy));
        body.insert("totalTime".into(), Value::from(total_time));
        body.insert("startTime".into(), Value::from(start_ms));
        body.insert("stopTime".into(), Value::from(stop_ms));
        body.insert("getPrize".into(), Value::Bool(false));
        body.insert("status".into(), Value::from(0));
        body.insert("uuid".into(), Value::String(run_uuid.clone()));
        body.insert("uid".into(), Value::from(p.uid));
        body.insert("selectedUnid".into(), Value::from(unid));
        body.insert("selRunTime".into(), Value::from(total_time));
        body.insert("selDistance".into(), Value::from(p.min_distance));
        body.insert("totalDis".into(), Value::from(round_to(total_dis, 0) as i64));
        body.insert("speed".into(), Value::from(speed));
        body.insert("validDis".into(), Value::from(round_to(total_dis, 0) as i64));
        body.insert("validTime".into(), Value::from(total_time));
        body.insert("complete".into(), Value::Bool(true));
        body.insert("unCompleteReason".into(), Value::from(0));
        body.insert("calorie".into(), Value::from(kcal));
        body.insert("totalSteps".into(), Value::from(total_steps));
        body.insert("avgStepFreq".into(), Value::from(avg_step_freq));
        body.insert("useMobilityTools".into(), Value::from(0));
        body.insert("faceCheck".into(), Value::from(p.face_check));
        body.insert("totalAscent".into(), Value::from(round_to(ascent, 0) as i64));
        body.insert("avgPower".into(), Value::from(power));
        body.insert("speedPerTenSec".into(), Value::Array(android_tensec(track, start_ms, "speed")));
        body.insert("stepsPerTenSec".into(), Value::Array(android_tensec(track, start_ms, "steps")));
        body.insert("isUpload".into(), Value::Bool(false));
        body.insert("more".into(), Value::Bool(false));
        body.insert("latitude".into(), Value::from(start_coordinate.latitude));
        body.insert("longitude".into(), Value::from(start_coordinate.longitude));
        body.insert("maxRunTime".into(), Value::from(0));
        body.insert("minSteps".into(), Value::from(0));
        if !p.five_point_json.is_empty() {
            body.insert("fivePointJson".into(), Value::String(p.five_point_json.clone()));
        }
        // Android 必现扩展字段
        body.insert("errorCode".into(), Value::from(0));
        body.insert("geeToken".into(), Value::String(String::new()));
        body.insert("unauthorized".into(), Value::from(0));
        body.insert("themeId".into(), Value::from(0));
        body.insert("goalId".into(), Value::Null);
        body.insert("address".into(), Value::String(p.address.trim().to_string()));
        let body_val = Value::Object(body.clone());
        let sig = signature(&body_val, false);
        let orig = original_sign(&body_val, false);
        body.insert("signature".into(), Value::String(sig));
        body.insert("originalSign".into(), Value::String(orig));
    }
    let body_plain = Value::Object(body).to_string();

    let device_id = if client.identity.device_id.is_empty() {
        uuid::Uuid::new_v4().to_string().to_uppercase()
    } else {
        client.identity.device_id.clone()
    };
    let (hp, hp_extra, ua, app_ver): (String, Vec<(String, String)>, String, &'static str) = if ios_form {
        // iOS 形态：直接复用全链设备身份（平台/机型/系统版本与 policy/points/OBS 完全一致）
        let (hp, extra) = build_native_ios_header(&client.identity, p.uid, &client_token(client), None);
        (hp, extra, ua_ios_for(&client.identity.os_version), crate::crypto::header::IOS_APP_VERSION)
    } else {
        let android_identity = HeaderIdentity {
            platform: "android".into(),
            device_id: device_id.clone(),
            os_version: "14".into(),
            device_name: "22081212C".into(),
            ..client.identity.clone()
        };
        let (hp, extra) = build_android_header(&android_identity, p.uid, &client_token(client), None);
        (hp, extra, UA_ANDROID.to_string(), crate::crypto::header::ANDROID_APP_VERSION)
    };
    let header_env = build_envelope(&mut client.session, &hp, OuterOrder::Observed);
    // body 时间戳 = header ts + 0..=2ms（与 client.rs 一致：原生两次独立取时的自然抖动）
    let delta = (rand::Rng::gen::<u32>(&mut rand::thread_rng()) % 3) as i64;
    let body_env =
        build_envelope_ts(&mut client.session, &body_plain, OuterOrder::Insert, header_env.ts_ms + delta);

    // runes = uid + policy 时间戳；runef = 记录 UUID + startTime（recordID）
    let runes = format!("{}{}", p.uid, p.policy_ts);
    let runef = format!("{}{}", run_uuid, start_ms);
    let mut req = client.agent.post(&format!("{HOST}{RECORD_PATH}"));
    req = req
        .set("Content-Type", "application/json; charset=utf-8")
        .set("Accept", "*/*")
        .set("Accept-Language", "zh-Hans-CN;q=1.0")
        .set("Accept-Encoding", "gzip;q=1.0, compress;q=0.5")
        .set("User-Agent", &ua)
        .set("appVersion", app_ver)
        .set("headerSign", &header_env.json)
        .set("runes", &runes)
        .set("runef", &runef);
    for (k, v) in &hp_extra {
        req = req.set(k, v);
    }
    let resp = req.send_string(&body_env.json).map_err(ureq_err)?;
    let status = resp.status();
    let raw = resp.into_string().unwrap_or_default();
    log(&format!("[record] sportType=1 HTTP {status} len={}", raw.len()));

    let key = derive_paes_key(
        &body_env.key_data[0], &body_env.key_data[1], &body_env.key_data[2], &body_env.key_data[3],
    );
    let dec = decrypt_response(raw.as_bytes(), &key, &rsa_public_key())
        .map_err(|e| format!("提交响应解密失败: {e}"))?;
    let biz = check_business(&dec.business)?;
    let rrid = super::client::get_field(&biz, "rrid").and_then(|v| v.as_i64()).unwrap_or(0);
    if rrid <= 0 {
        return Err(format!("提交未返回 rrid: {}", truncate_json(&biz)));
    }
    log(&format!("√ 提交成功 rrid={rrid} uuid={run_uuid}"));
    Ok(SubmitResult {
        rrid,
        uuid: run_uuid,
        start_ms,
        complete: super::client::get_field(&biz, "complete").and_then(|v| v.as_bool()),
        total_dis,
        total_time,
        total_steps,
        avg_step_freq,
        calorie: kcal,
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
