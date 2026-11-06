//! 请求头明文构造。
//! 固定键序紧凑 JSON；appInstallTime/appUpdateTime 从设备身份读取，
//! 同一 DeviceId 全生命周期稳定（逐请求漂移会影响设备一致性）。

use super::envelope::{md5_hex, now_ms};

/// iOS 系统版本档位（按机型代数给出**可能存在**的组合，避免"未发布机型 + 老系统"这类
/// 出厂不成立、可被机型/系统交叉校验识别的指纹）。
/// 键为 `IOS_DEVICE_POOL` 中的机型名。
fn os_pool_for_model_lower(name: &str) -> &'static [&'static str] {
    if name.starts_with("iphone 17") || name == "iphone air" {
        OS_POOL_26
    } else if name.starts_with("iphone 16") {
        OS_POOL_16
    } else if name.starts_with("iphone 15") {
        OS_POOL_15
    } else {
        OS_POOL_LEGACY
    }
}

/// iPhone 17 系列 / iPhone Air：出厂即 iOS 26。
pub const OS_POOL_26: &[&str] = &["26.5.2", "26.4.1", "26.3.0", "26.2.1", "26.1.0"];
/// iPhone 16 系列：出厂 iOS 18，可升到 26。
pub const OS_POOL_16: &[&str] = &["26.5.2", "26.4.1", "26.1.0", "18.6.2", "18.5.0", "18.3.1"];
/// iPhone 15 系列：出厂 iOS 17。
pub const OS_POOL_15: &[&str] = &["26.5.2", "26.1.0", "18.6.2", "18.3.1", "17.7.2"];
/// iPhone 14/13/12/11：出厂 iOS 16 及更早，可升到 26。
pub const OS_POOL_LEGACY: &[&str] = &["26.5.2", "18.6.2", "18.3.1", "17.7.2", "16.7.11"];

/// 机型 → 允许的系统版本池（大小写不敏感）。未命中的机型返回最保守的老机池。
pub fn os_pool_for_model(name: &str) -> &'static [&'static str] {
    os_pool_for_model_lower(&name.trim().to_lowercase())
}

/// 机型 → (logicPixel, physicPixel)。命中机型池时返回池内权威分辨率，
/// 用于在设备名被手改后自动纠正分辨率，避免"机型与像素对不上"的交叉校验特征。
pub fn pixels_for_model(name: &str) -> Option<(&'static str, &'static str)> {
    let key = name.trim();
    IOS_DEVICE_POOL
        .iter()
        .find(|(n, _, _)| n.eq_ignore_ascii_case(key))
        .map(|(_, logic, physic)| (*logic, *physic))
}

/// tokenSign 固定后缀（与提交签名盐同源）。
pub const TOKEN_SIGN_SUFFIX: &str = "2slhe02lsfiwowlcixisla_sls-_slaor";

pub const IOS_APP_VERSION: &str = "7.4.0";

/// 7.4.0 实机逆向：cpuModel = NXGetLocalArchInfo()->description，真机恒 "arm64"；
/// "x86_64" 是模拟器签名，真实设备永不出现。
pub const IOS_CPU_MODEL: &str = "arm64";

/// 真机 iPhone 身份档位（营销名 + UIScreen bounds / bounds×scale），全部为 3x 机型，
/// 与 UA 的 Scale/3.00 一致。随机身份时整套抽取并持久化，模拟真实设备多样性。
pub const IOS_DEVICE_POOL: [(&str, &str, &str); 21] = [
    // iPhone 17 系列（7.4.0 实机映射：iPhone18,1/18,2）
    ("iPhone 17 Pro", "402x874", "1206x2622"),
    ("iPhone 17 Pro Max", "440x956", "1320x2868"),
    ("iPhone 17", "402x874", "1206x2622"),
    ("iPhone Air", "420x912", "1260x2736"),
    // iPhone 16 系列
    ("iPhone 16", "393x852", "1179x2556"),
    ("iPhone 16 Plus", "428x926", "1284x2778"),
    ("iPhone 16 Pro", "402x874", "1206x2622"),
    ("iPhone 16 Pro Max", "440x956", "1320x2868"),
    // iPhone 15 系列
    ("iPhone 15", "393x852", "1179x2556"),
    ("iPhone 15 Pro", "393x852", "1179x2556"),
    ("iPhone 15 Pro Max", "430x932", "1290x2796"),
    // iPhone 14 系列
    ("iPhone 14", "390x844", "1170x2532"),
    ("iPhone 14 Pro", "393x852", "1179x2556"),
    ("iPhone 14 Pro Max", "430x932", "1290x2796"),
    ("iPhone 14 Plus", "428x926", "1284x2778"),
    // iPhone 13/12 系列
    ("iPhone 13", "390x844", "1170x2532"),
    ("iPhone 13 Pro", "390x844", "1170x2532"),
    ("iPhone 13 mini", "375x812", "1125x2436"),
    ("iPhone 12", "390x844", "1170x2532"),
    ("iPhone 12 Pro Max", "428x926", "1284x2778"),
    ("iPhone 11 Pro Max", "414x896", "1242x2688"),
];


/// 按设备身份动态生成 iOS UA（Alamofire "%@/%@ (%@; iOS %@; Scale/%0.2f)" 格式）。
/// UA 的 iOS 版本必须与 headerSign.osVersion 一致，否则是显式指纹矛盾。
pub fn ua_ios_for(os_version: &str) -> String {
    format!("SWCampus/{IOS_APP_VERSION} (iPhone; iOS {os_version}; Scale/3.00)")
}

/// 7.4.0 实机逆向：App 的 "IDFA" 并非系统广告标识符，而是 WHSimulateIDFA——
/// 首次启动生成一个随机 UUID 存钥匙串并终身复用。因此请求头 **永远** 携带该字段。
/// 设备身份。app_install_time <= 0 表示尚未生成。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct HeaderIdentity {
    #[serde(default = "default_platform")]
    pub platform: String, // 固定 "ios"（Android 模拟已移除）
    #[serde(default)]
    pub device_id: String,
    #[serde(default)]
    pub idfa: String,
    #[serde(default = "default_os_version")]
    pub os_version: String,
    #[serde(default = "default_device_name")]
    pub device_name: String,
    /// 本地展示的制造商品牌；不改变服务端 deviceName（机型）字段。
    #[serde(default)]
    pub manufacturer: String,
    #[serde(default = "default_anchor_lat")]
    pub anchor_lat: f64,
    #[serde(default = "default_anchor_lon")]
    pub anchor_lon: f64,
    #[serde(default)]
    pub app_install_time: i64,
    /// iOS 逻辑分辨率（真机 UIScreen bounds，随机身份时按档位抽取）
    #[serde(default = "default_logic_pixel")]
    pub logic_pixel: String,
    /// iOS 物理分辨率（bounds × scale）
    #[serde(default = "default_physic_pixel")]
    pub physic_pixel: String,
    /// 提交 body 的城市名
    #[serde(default = "default_city")]
    pub city: String,
    /// 本机跑步记录序号。
    ///
    /// 真机 22 键点位的 `flag` 是 `[SWRunGradeBuilder recordID]`——本机 Realm 记录的自增主键，
    /// **不是时间戳**。本工具没有 Realm，因此维护一个等价的设备级自增序号：
    /// 每次提交前 +1 并落盘，同一次记录的所有点共用同一个值。
    #[serde(default)]
    pub record_seq: i64,
}

fn default_platform() -> String { "ios".into() }
fn default_os_version() -> String { "26.5.2".into() }
fn default_device_name() -> String { "iPhone 17 Pro Max".into() }
fn default_logic_pixel() -> String { "440x956".into() }
fn default_physic_pixel() -> String { "1320x2868".into() }
/// 锚点默认值与城市默认值共同构成「尚未配置」哨兵：
/// [`HeaderIdentity::has_unconfigured_default_location`] 用它拦住未配置就跑的行为。
/// 这不是一个可用的真实位置，仅用于与真实配置区分。
fn default_anchor_lat() -> f64 { crate::location::DEFAULT_ANCHOR_LAT }
fn default_anchor_lon() -> f64 { crate::location::DEFAULT_ANCHOR_LON }
fn default_city() -> String { crate::location::default_city().to_string() }

impl HeaderIdentity {
    /// 返回设备页配置的锚点，并执行统一边界校验。
    pub fn anchor_coordinate(&self) -> Result<crate::location::Coordinate, String> {
        crate::location::Coordinate::new(self.anchor_lat, self.anchor_lon, 0.0)
    }

    /// 实际参与请求的 IDFA。
    ///
    /// 7.4.0 实机是 WHSimulateIDFA（钥匙串 UUID，生成一次终身复用），**永不缺失**。
    /// 因此空 `idfa` 不能被当成"没有该字段"，而必须由 `device_id` 确定性派生：
    /// 这样同一次会话里请求头、登录体、各业务请求拿到的是同一个值。
    pub fn effective_idfa(&self) -> String {
        if !self.idfa.is_empty() {
            return self.idfa.clone();
        }
        // 未持久化过：由 DeviceId 确定性派生。持久化层会在首次启动时补写，
        // 因此同一台设备跨会话、跨请求始终是同一个值。
        stable_idfa_for(&self.device_id)
    }

    /// 机型命中机型池时，按池内权威分辨率纠正逻辑/物理像素。
    /// 返回 true 表示发生了改写（需要落盘）。
    pub fn normalize_pixels_to_model(&mut self) -> bool {
        let Some((logic, physic)) = pixels_for_model(&self.device_name) else {
            return false;
        };
        if self.logic_pixel == logic && self.physic_pixel == physic {
            return false;
        }
        self.logic_pixel = logic.to_string();
        self.physic_pixel = physic.to_string();
        true
    }

    /// 旧版本首次启动会写入大连默认值。未明确配置前禁止将它作为异地跑步位置使用。
    pub fn has_unconfigured_default_location(&self) -> bool {
        self.city.trim().is_empty()
            || self.city.trim() == crate::location::default_city()
            || self.anchor_coordinate().map(|c| c.is_default_dalian()).unwrap_or(true)
    }

    /// 安装时间：持久值优先；缺失时按 iOS 惯例回退（3 天前）。
    pub fn install_time(&self, ts: i64) -> i64 {
        if self.app_install_time > 0 {
            self.app_install_time
        } else {
            ts - 3 * 86_400_000
        }
    }

    /// 生成一份新的安装时间（设备随机化时调用，iOS 惯例 3 天前）。
    pub fn fresh_install_time() -> i64 {
        now_ms() - 3 * 86_400_000
    }
}

impl Default for HeaderIdentity {
    fn default() -> Self {
        Self {
            platform: default_platform(),
            device_id: String::new(),
            idfa: String::new(),
            os_version: default_os_version(),
            device_name: default_device_name(),
            manufacturer: String::new(),
            anchor_lat: default_anchor_lat(),
            anchor_lon: default_anchor_lon(),
            app_install_time: 0,
            logic_pixel: default_logic_pixel(),
            physic_pixel: default_physic_pixel(),
            city: default_city(),
            record_seq: 0,
        }
    }
}

/// SWEncryption.signObject:suffix: 的 tokenSign。
/// `SWJSON sortedQueryString` 按 key 排序：`timeStamp=<ts>&token=<token>&uid=<uid>`。
/// **未登录时同样计算**（uid/token 取空串），这是 IDA 里 `managerHeadDict` 的固定行为。
pub fn native_token_sign(uid: &str, token: &str, timestamp_ms: &str) -> String {
    let query = format!("timeStamp={timestamp_ms}&token={token}&uid={uid}");
    md5_hex(format!("{}{}", query, TOKEN_SIGN_SUFFIX).as_bytes())
}
/// 由 device_id 确定性派生 IDFA（RFC-4122 v5 风格：MD5 前 16 字节）。
/// 同一 device_id 永远得到同一 IDFA，避免逐请求随机。
fn stable_idfa_for(device_id: &str) -> String {
    let digest = crate::crypto::envelope::md5_hex(
        format!("idfa:{device_id}:iOS_sportsWorld_campus").as_bytes(),
    );
    let h: Vec<char> = digest.chars().collect();
    let s: String = h.into_iter().collect();
    // 8-4-4-4-12
    format!(
        "{}-{}-{}-{}-{}",
        s[0..8].to_uppercase(),
        s[8..12].to_uppercase(),
        s[12..16].to_uppercase(),
        s[16..20].to_uppercase(),
        s[20..32].to_uppercase()
    )
}

/// iOS 请求头明文（SWRequestHeaderProvider._fillFullHeaders 字段序）。
/// 返回 (紧凑 JSON, HTTP 附加头字段: nonce/tokenSign/timeStamp)。
pub fn build_native_ios_header(
    identity: &HeaderIdentity,
    uid: i64,
    token: &str,
    timestamp_ms: Option<i64>,
) -> (String, Vec<(String, String)>) {
    build_native_ios_header_with_extra(identity, uid, token, timestamp_ms, &[])
}

/// 带附加字段的请求头。
///
/// 7.4.0 实证：`+[SWCHttpTool requestRecordHeaderDict:]` @0x10027fd6c 把
/// `runes`/`runef`/`runec` **并入头字典**，再整体 `dictionaryToJson` →
/// `SWEncryption encryptString:` → 只写 `headerSign` 一个 HTTP 头。
/// 也就是说这三项是 headerSign 密文里的字段（18 + 3 = 21 键），
/// **不是独立的 HTTP 请求头**。生产环境（`SWAPI environment != 2`）下
/// `-[SWRequestHeaderProvider _setHeaderWithDictionary:]` 那条逐字段发头的
/// 分支根本不会执行。
pub fn build_native_ios_header_with_extra(
    identity: &HeaderIdentity,
    uid: i64,
    token: &str,
    timestamp_ms: Option<i64>,
    extra_fields: &[(&str, &str)],
) -> (String, Vec<(String, String)>) {
    let mut m = serde_json::Map::new();
    let device_id = if identity.device_id.is_empty() {
        uuid::Uuid::new_v4().to_string().to_uppercase()
    } else {
        identity.device_id.clone()
    };
    let ts = timestamp_ms.unwrap_or_else(now_ms);
    let install = identity.install_time(ts);
    let nonce = uuid::Uuid::new_v4().to_string().to_uppercase();

    // ── 字段与顺序对齐 `+[SWCHttpTool managerHeadDict]` @0x10027efc4 ──
    // 这是 `setRequestSeriaHeader:` 真正加密进 headerSign 的字典
    // （`_fillFullHeaders` 的 18 键版本只被 H5 桥的 fullProvider 使用）。
    m.insert("osType".into(), serde_json::Value::String("1".into()));
    m.insert("DeviceId".into(), serde_json::Value::String(device_id.clone()));
    m.insert("appVersion".into(), serde_json::Value::String(IOS_APP_VERSION.into()));
    // CustomDeviceId 只在 NSUserDefaults 里有 APPDeviceID 时才写；这里恒有值。
    m.insert(
        "CustomDeviceId".into(),
        serde_json::Value::String(format!("{device_id}_iOS_sportsWorld_campus")),
    );
    // uid / token 恒存在，未登录时为空串（`isStrEmpty ? @"" : value`）。
    let uid_str = if uid >= 1 { uid.to_string() } else { String::new() };
    m.insert("uid".into(), serde_json::Value::String(uid_str.clone()));
    m.insert("token".into(), serde_json::Value::String(token.to_string()));
    // timeStamp 来自 `[CommonUtils dateTime]`，返回的是 **NSString**（毫秒）。
    let ts_str = ts.to_string();
    m.insert("timeStamp".into(), serde_json::Value::String(ts_str.clone()));
    // 未登录也照算 tokenSign（对内层 {uid,token,timeStamp} 三项签名）。
    m.insert(
        "tokenSign".into(),
        serde_json::Value::String(native_token_sign(&uid_str, token, &ts_str)),
    );
    m.insert("deviceName".into(), serde_json::Value::String(identity.device_name.clone()));
    m.insert("osVersion".into(), serde_json::Value::String(identity.os_version.clone()));
    // 7.4.0 实机：WHSimulateIDFA 钥匙串 UUID 终身复用，请求头永远携带，缺失即指纹异常。
    m.insert(
        "IDFA".into(),
        serde_json::Value::String(identity.effective_idfa()),
    );
    m.insert("logicPixel".into(), serde_json::Value::String(identity.logic_pixel.clone()));
    m.insert("physicPixel".into(), serde_json::Value::String(identity.physic_pixel.clone()));
    m.insert("cpuModel".into(), serde_json::Value::String(IOS_CPU_MODEL.into()));
    m.insert("isRoot".into(), serde_json::Value::Bool(false));
    m.insert("appInstallTime".into(), serde_json::Value::from(install));
    // nonce 是 UUIDString（大写带连字符）。
    m.insert("nonce".into(), serde_json::Value::String(nonce.clone()));
    // 附加字段在基础 18 键之后并入（与 App 的 mutableCopy + setObject: 一致）。
    for (k, v) in extra_fields {
        m.insert((*k).to_string(), serde_json::Value::String((*v).to_string()));
    }
    // 普通业务请求（`+[SWCHttpTool setRequestSeriaHeader:]`）只发 headerSign + appVersion；
    // `nonce`/`timeStamp`/`tokenSign` 只存在于 headerSign 的加密 JSON 里。
    // 注意这与记录提交不同：记录提交的请求构造块 `sub_1000483CC` 会再把
    // `makeHTTPRequestExtraHeader` 的 `runes`/`runef`/`runec` 逐个头设上去
    // （那三个由 `submit.rs` 负责，不在本函数返回值里）。返回值第二项恒为空。
    (serde_json::to_string(&m).unwrap_or_default(), Vec::new())
}

/// 请求头构造器（只保留 iOS 7.4.0 形态）。
pub fn build_header_for(
    identity: &HeaderIdentity,
    uid: i64,
    token: &str,
    timestamp_ms: Option<i64>,
) -> (String, Vec<(String, String)>) {
    build_native_ios_header(identity, uid, token, timestamp_ms)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// tokenSign 输入是**已经字符串化**的 uid/token/timeStamp（真机三者都是 NSString）。
    #[test]
    fn test_token_sign_vector() {
        assert_eq!(
            native_token_sign("13056447", "TOKENABC", "1788958186123"),
            "4a8d163186d91ac7b539a7bb07d457ba"
        );
        // 未登录（空 uid/token）也要能算，且结果非空
        let anon = native_token_sign("", "", "1788958186123");
        assert_eq!(anon.len(), 32);
        assert_ne!(anon, native_token_sign("13056447", "TOKENABC", "1788958186123"));
    }

    /// 键集与取值对齐 `+[SWCHttpTool managerHeadDict]` @0x10027efc4。
    /// 关键点：uid/token/tokenSign **恒存在**（未登录为空串），没有 studentId。
    #[test]
    fn test_ios_header_shape() {
        let identity = HeaderIdentity {
            device_id: "0FA3EA16-46F0-4E44-9A97-1B63F0D9B11A".into(),
            idfa: String::new(),
            ..Default::default()
        };
        let (json, extra) = build_native_ios_header(&identity, -1, "", Some(1788958186123));
        assert!(
            extra.is_empty(),
            "普通业务请求不发 nonce/timeStamp/tokenSign 独立 HTTP 头"
        );
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        let obj = v.as_object().unwrap();
        let mut keys: Vec<&str> = obj.keys().map(|s| s.as_str()).collect();
        keys.sort_unstable();
        let mut want = vec![
            "osType", "DeviceId", "appVersion", "CustomDeviceId", "uid", "token",
            "timeStamp", "tokenSign", "deviceName", "osVersion", "IDFA", "logicPixel",
            "physicPixel", "cpuModel", "isRoot", "appInstallTime", "nonce",
        ];
        want.sort_unstable();
        assert_eq!(keys, want, "未登录也必须是 17 键（uid/token/tokenSign 为空串而非缺失）");
        // 未登录：uid/token 是空串，tokenSign 仍然算出
        assert_eq!(v["uid"], "");
        assert_eq!(v["token"], "");
        assert_eq!(v["tokenSign"], native_token_sign("", "", "1788958186123"));
        // timeStamp 是字符串（[CommonUtils dateTime] 返回 NSString）
        assert_eq!(v["timeStamp"], "1788958186123");
        assert!(uuid::Uuid::parse_str(v["IDFA"].as_str().unwrap()).is_ok());
        assert_eq!(v["osType"], "1");
        assert_eq!(v["cpuModel"], "arm64");
        assert!(v.get("studentId").is_none(), "managerHeadDict 里没有 studentId");

        let (json2, _) = build_native_ios_header(&identity, 13056447, "TOKEN", Some(1788958186123));
        let v2: serde_json::Value = serde_json::from_str(&json2).unwrap();
        assert_eq!(v2["uid"], "13056447");
        assert_eq!(v2["token"], "TOKEN");
        assert_eq!(v2["tokenSign"], native_token_sign("13056447", "TOKEN", "1788958186123"));
    }

    /// 同一身份的 appInstallTime 跨请求零漂移（设备一致性防护）。
    #[test]
    fn test_install_time_stable_across_requests() {
        let identity = HeaderIdentity { app_install_time: 1_700_000_000_000_i64, ..Default::default() };
        let (j1, _) = build_native_ios_header(&identity, 1, "T", Some(1788958186123));
        let (j2, _) = build_native_ios_header(&identity, 1, "T", Some(1788958187000));
        let v1: serde_json::Value = serde_json::from_str(&j1).unwrap();
        let v2: serde_json::Value = serde_json::from_str(&j2).unwrap();
        assert_eq!(v1["appInstallTime"], v2["appInstallTime"]);
        assert_eq!(v1["appInstallTime"], 1_700_000_000_000_i64);
        // 身份未持久化安装时间时，回退值在同一请求内至少自洽
        let fallback = HeaderIdentity::default();
        assert_eq!(fallback.install_time(1000), 1000 - 3 * 86_400_000);
    }

    #[test]
    fn legacy_dalian_defaults_are_not_accepted_for_a_run() {
        assert!(HeaderIdentity::default().has_unconfigured_default_location());
        let configured = HeaderIdentity {
            city: "石家庄市".into(),
            anchor_lat: 38.0428,
            anchor_lon: 114.5149,
            ..Default::default()
        };
        assert!(!configured.has_unconfigured_default_location());
    }
}

/// 校验并修正 iOS 逻辑/物理分辨率：iPhone 的 Retina 倍率只能是 2x 或 3x，
/// 即 physicPixel 必须等于 k × logicPixel（k∈{2,3}）。返回 Some(修正值) 表示需要改写。
/// 说明：机型池内全部是 3x；这里同时允许 2x，是为了不把用户手填的
/// iPhone SE / 11 等 2x 机型误改成 3x。
pub fn normalize_pixel_ratio(logic: &str, physic: &str) -> Option<String> {
    let parse = |s: &str| -> Option<(i64, i64)> {
        let (a, b) = s.split_once('x')?;
        Some((a.trim().parse().ok()?, b.trim().parse().ok()?))
    };
    let (lw, lh) = parse(logic)?;
    match parse(physic) {
        Some((pw, ph)) if (pw == lw * 2 && ph == lh * 2) || (pw == lw * 3 && ph == lh * 3) => None,
        // 无法识别时按池内主流 3x 修正
        _ => Some(format!("{}x{}", lw * 3, lh * 3)),
    }
}

#[cfg(test)]
mod identity_consistency_tests {
    use super::*;

    /// 所有 iPhone 逻辑/物理分辨率必须是 3x。
    #[test]
    fn iphone_pool_is_all_3x() {
        for (name, logic, physic) in IOS_DEVICE_POOL {
            assert_eq!(
                normalize_pixel_ratio(logic, physic), None,
                "{name} 的 {logic} / {physic} 不是 3x，会被像素维度交叉校验识别"
            );
        }
    }

    #[test]
    fn mismatch_is_corrected_from_logic() {
        assert_eq!(
            normalize_pixel_ratio("430x932", "1179x2556"),
            Some("1290x2796".into())
        );
        assert_eq!(normalize_pixel_ratio("1290x2796", "3870x8388"), None);
        // 2x 机型（iPhone SE / 11）不得被改成 3x
        assert_eq!(normalize_pixel_ratio("375x667", "750x1334"), None);
        // 无法解析时也要给出 3x 结果（保留 logic）
        assert_eq!(normalize_pixel_ratio("393x852", "bad"), Some("1179x2556".into()));
    }

    /// IDFA 由 device_id 确定性派生：同 device_id 跨调用必须一致。
    #[test]
    fn idfa_derivation_is_stable() {
        let a = stable_idfa_for("66147D92-F160-4A5E-937B-B8B36FCEE21F");
        let b = stable_idfa_for("66147D92-F160-4A5E-937B-B8B36FCEE21F");
        let c = stable_idfa_for("OTHER-DEVICE");
        assert_eq!(a, b, "同一 device_id 必须得到同一 IDFA");
        assert_ne!(a, c);
        assert_eq!(a.len(), 36, "必须是 UUID 形态");
    }

    /// 空 IDFA 时，两个不同请求头里的 IDFA 必须一致（不能逐请求随机）。
    #[test]
    fn empty_idfa_yields_same_value_across_requests() {
        let identity = HeaderIdentity {
            device_id: "66147D92-F160-4A5E-937B-B8B36FCEE21F".into(),
            idfa: String::new(),
            ..Default::default()
        };
        let (j1, _) = build_native_ios_header(&identity, 1, "T", Some(1_700_000_000_000));
        let (j2, _) = build_native_ios_header(&identity, 1, "T", Some(1_700_000_009_000));
        let v1: serde_json::Value = serde_json::from_str(&j1).unwrap();
        let v2: serde_json::Value = serde_json::from_str(&j2).unwrap();
        assert_eq!(v1["IDFA"], v2["IDFA"], "空 IDFA 不得逐请求随机");
        assert_ne!(v1["IDFA"], "");
    }
}

/// `+[SWCHttpTool managerHeadDict]` 写入的 17 个 JSON 键。
pub const IOS_HEADER_KEYS: [&str; 17] = [
        "osType", "DeviceId", "appVersion", "CustomDeviceId", "uid", "token",
        "timeStamp", "tokenSign", "deviceName", "osVersion", "IDFA", "logicPixel",
        "physicPixel", "cpuModel", "isRoot", "appInstallTime", "nonce",
    ];

/// 请求头协议护栏：字段集合必须与 IPA `-[SWRequestHeaderProvider _fillFullHeaders]` 一致。
#[cfg(test)]
mod ios_header_guard_tests {
    use super::*;



    fn build() -> (serde_json::Value, Vec<(String, String)>) {
        let identity = HeaderIdentity {
            device_id: "66147D92-F160-4A5E-937B-B8B36FCEE21F".into(),
            idfa: "66147D92-F160-4A5E-937B-B8B36FCEE21F".into(),
            device_name: "iPhone 17 Pro Max".into(),
            os_version: "26.5.2".into(),
            logic_pixel: "440x956".into(),
            physic_pixel: "1320x2868".into(),
            app_install_time: 1_700_000_000_000,
            ..Default::default()
        };
        let (json, extra) = build_native_ios_header(&identity, 17837509, "TOKEN-X", Some(1_700_000_009_000));
        (serde_json::from_str(&json).unwrap(), extra)
    }

    #[test]
    fn header_key_set_matches_ipa() {
        let (v, _) = build();
        let mut got: Vec<String> = v.as_object().unwrap().keys().cloned().collect();
        got.sort_unstable();
        let mut want: Vec<String> = IOS_HEADER_KEYS.iter().map(|s| s.to_string()).collect();
        want.sort_unstable();
        assert_eq!(got, want, "iOS 请求头键集必须等于 managerHeadDict 的 17 键");
    }

    #[test]
    fn header_values_are_protocol_correct() {
        let (v, extra) = build();
        assert_eq!(v["osType"], "1", "iOS 的 osType 固定字符串 \"1\"");
        assert_eq!(v["appVersion"], IOS_APP_VERSION);
        assert_eq!(v["cpuModel"], IOS_CPU_MODEL);
        assert_eq!(v["isRoot"], false);
        assert_eq!(v["uid"], "17837509");
        assert_eq!(v["token"], "TOKEN-X");
        assert_eq!(v["timeStamp"], "1700000009000");
        // CustomDeviceId 必须是 "<DeviceId>_iOS_sportsWorld_campus"
        assert_eq!(
            v["CustomDeviceId"],
            "66147D92-F160-4A5E-937B-B8B36FCEE21F_iOS_sportsWorld_campus"
        );
        // tokenSign = MD5(timeStamp=..&token=..&uid=.. + 后缀)
        assert_eq!(
            v["tokenSign"],
            native_token_sign("17837509", "TOKEN-X", "1700000009000")
        );
        // 普通业务请求不发 nonce/timeStamp/tokenSign 独立头
        // （记录提交额外发的是 runes/runef/runec，在 submit.rs 里设）
        assert!(extra.is_empty(), "只应发 headerSign + appVersion 两个自定义头");
    }

    /// 跑步提交的 headerSign 必须比通用头多出 runes/runef/runec 三个字段
    /// （`+[SWCHttpTool requestRecordHeaderDict:]` 把三者并入头字典后再加密）。
    #[test]
    fn record_header_carries_runes_runef_runec_inside_header_sign() {
        let identity = HeaderIdentity {
            device_id: "66147D92-F160-4A5E-937B-B8B36FCEE21F".into(),
            ..Default::default()
        };
        // 真值形态：runes = uid；runef = uuid + 本机记录 id；runec = 定位信封
        let extra = crate::api::submit::run_header_extra(17837509, 1_700_000_123_456, "RUN-UUID", 42, "ENV-RUNEC");
        let refs: Vec<(&str, &str)> = extra.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        let (plain, _) = build_native_ios_header_with_extra(
            &identity,
            17837509,
            "TOKEN-X",
            Some(1_700_000_009_000),
            &refs,
        );
        let v: serde_json::Value = serde_json::from_str(&plain).unwrap();
        let obj = v.as_object().unwrap();
        assert_eq!(obj.len(), 20, "17 基础键 + 3 个记录附加键");
        assert_eq!(obj.get("runes").and_then(|x| x.as_str()), Some("178375091700000123456"));
        assert_eq!(obj.get("runef").and_then(|x| x.as_str()), Some("RUN-UUID42"));
        assert_eq!(obj.get("runec").and_then(|x| x.as_str()), Some("ENV-RUNEC"));
        // 通用请求头不得带这三项（否则与真机的头集合不同）
        let (plain2, _) =
            build_native_ios_header(&identity, 17837509, "TOKEN-X", Some(1_700_000_009_000));
        let v2: serde_json::Value = serde_json::from_str(&plain2).unwrap();
        for k in ["runes", "runef", "runec"] {
            assert!(v2.get(k).is_none(), "通用请求头不应含 {k}");
        }
    }

    /// UA 必须与 osVersion / appVersion 同源。
    #[test]
    fn ua_is_consistent_with_header() {
        let (v, _) = build();
        let ua = ua_ios_for(v["osVersion"].as_str().unwrap());
        assert!(ua.starts_with(&format!("SWCampus/{}", IOS_APP_VERSION)), "{ua}");
        assert!(ua.contains("iPhone"), "{ua}");
        assert!(ua.contains(v["osVersion"].as_str().unwrap()), "{ua}");
        assert!(ua.ends_with("Scale/3.00)"), "3x 机型必须标 Scale/3.00：{ua}");
    }
}
