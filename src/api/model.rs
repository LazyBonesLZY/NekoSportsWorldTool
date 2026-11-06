//! 数据模型与本地持久化。
//!
//! 持久化文件位于桌面 exe 目录或 Android 应用私有目录：identity.json（设备身份，device_id 固定复用
//! ——严禁每次随机，会触发 10121 风控）、session.json（登录态）、config.json
//! （账号/参数）、points_cache.json（点位缓存）。

pub use crate::crypto::header::HeaderIdentity;

pub const HOST: &str = "https://run.gxapp.iydsj.com";
/// 排行榜 / 违规名单域名（信封链与 RUN 相同）。
pub const DISCOVERY: &str = "https://discovery.gxapp.iydsj.com";

/// 登录态（session.json）。
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct Session {
    #[serde(default)]
    pub uid: i64,
    #[serde(default)]
    pub token: String,
    #[serde(default)]
    pub unid: String,
    #[serde(default)]
    pub name: String,
    #[serde(default = "default_weight")]
    pub weight: f64,
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub device_id: String,
    /// 登录响应完整业务数据（我的页展示）
    #[serde(default)]
    pub profile: serde_json::Value,
}

/// 未登录/服务端未下发体重时存 0，用于区分"真的没给"与"给了 0"。
/// 展示层请用 [`Session::display_weight`]。
fn default_weight() -> f64 {
    0.0
}

impl Session {
    /// 展示用体重：真机 `getPersonWeight` 缺失时界面按 68 显示，但**协议层不能**这么用
    /// （`userWeight` 为 0 时 calorie 就是 0，见 `track::calorie`）。
    pub fn display_weight(&self) -> f64 {
        if self.weight > 0.0 {
            self.weight
        } else {
            68.0
        }
    }

    /// `+[SWRunConfigDataManager userWeightForCalorie]`：`userWeight <= 0` 时按性别回退
    /// `getSex isEqualToString:@"1"` → 67，否则 54（见 `track::calorie`）。
    pub fn weight_for_power(&self) -> f64 {
        if self.weight > 0.0 {
            return self.weight;
        }
        let sex = self
            .profile
            .get("sex")
            .map(|v| match v {
                serde_json::Value::String(s) => s.clone(),
                serde_json::Value::Number(n) => n.to_string(),
                _ => String::new(),
            })
            .unwrap_or_default();
        if sex == "1" {
            67.0
        } else {
            54.0
        }
    }

    pub fn is_logged_in(&self) -> bool {
        self.uid >= 1 && !self.token.is_empty()
    }
}

/// UI 参数（config.json）。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Config {
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub password: String,
    #[serde(default)]
    pub remember: bool,
    #[serde(default = "default_dist_min")]
    pub dist_min: f32,
    #[serde(default = "default_dist_max")]
    pub dist_max: f32,
    #[serde(default = "default_pace_min")]
    pub pace_min: f32,
    #[serde(default = "default_pace_max")]
    pub pace_max: f32,
    #[serde(default = "default_true")]
    pub face_check: bool,
    /// 跑步时覆盖轨迹点 bdA；为空表示使用生成器的海拔曲线。
    #[serde(default)]
    pub manual_altitude: Option<f64>,
    /// 跑步时将生成器海拔曲线映射到此范围；为空表示不使用范围覆盖。
    #[serde(default)]
    pub manual_altitude_range: Option<crate::track::altitude::AltitudeRange>,
    #[serde(default = "default_ai_minutes")]
    pub ai_minutes: i64,
    #[serde(default = "default_ai_reps")]
    pub ai_reps: i64,
    /// 启动时检查更新：silent（静默，发现新版才弹窗）/ ask（每次询问）/ off
    #[serde(default = "default_update_check")]
    pub update_check: String,
    /// OSM 路网文件路径（真实道路路由用）。
    #[serde(default)]
    pub osm_path: String,
    /// 路线算法：legacy | road。
    #[serde(default = "default_route_mode")]
    pub route_mode: String,
    /// 开始时间模式：0=随机（当天窗口内抽样），1=指定时刻。
    #[serde(default)]
    pub start_mode: i64,
    /// 指定日期：0=今天，1..=3=几天前。
    #[serde(default)]
    pub days_ago: i64,
    /// 指定时刻（时/分），仅在 `start_mode == 1` 时生效。
    #[serde(default = "default_hour")]
    pub hour: i64,
    #[serde(default)]
    pub minute: i64,
}

fn default_hour() -> i64 {
    12
}

fn default_route_mode() -> String {
    "legacy".into()
}

/// `dist_min` 的缺省值（与 [`Config::default`] 的 1.0 一致）。
fn default_dist_min() -> f32 {
    1.0
}
/// `dist_max` 必须与 [`Config::default`] 一致（1.5），不能复用 `dist_min` 的 1.0，
/// 否则旧配置缺该字段时上下界相等、区间退化。
fn default_dist_max() -> f32 {
    1.5
}
/// `face_check` 在 [`Config::default`] 里是 true，serde 缺省也必须一致。
fn default_true() -> bool {
    true
}
fn default_pace_min() -> f32 {
    360.0
}
fn default_pace_max() -> f32 {
    480.0
}
fn default_ai_minutes() -> i64 {
    1
}
fn default_ai_reps() -> i64 {
    5
}
fn default_update_check() -> String {
    "silent".into()
}

impl Default for Config {
    fn default() -> Self {
        Self {
            username: String::new(),
            password: String::new(),
            remember: false,
            dist_min: 1.0,
            dist_max: 1.5,
            pace_min: default_pace_min(),
            pace_max: default_pace_max(),
            face_check: true,
            manual_altitude: None,
            manual_altitude_range: None,
            ai_minutes: default_ai_minutes(),
            ai_reps: default_ai_reps(),
            update_check: default_update_check(),
            osm_path: String::new(),
            route_mode: default_route_mode(),
            start_mode: 0,
            days_ago: 0,
            hour: default_hour(),
            minute: 0,
        }
    }
}

// ── 持久化（平台数据目录）──────────────────────────────────────

fn exe_dir() -> std::path::PathBuf {
    crate::platform::data_dir()
}

/// 严格读取：区分「文件不存在」（正常，返回 `Ok(None)`）与「读/解析失败」（返回 `Err`）。
fn read_json_checked<T: serde::de::DeserializeOwned>(name: &str) -> Result<Option<T>, String> {
    let path = exe_dir().join(name);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("读取 {} 失败: {e}", path.display())),
    };
    match serde_json::from_str(&text) {
        Ok(value) => Ok(Some(value)),
        Err(e) => Err(format!(
            "解析 {} 失败: {e}（文件已保留原样，请修复或删除后重试）",
            path.display()
        )),
    }
}

/// 宽松读取：缓存类文件（点位/围栏/AI 项目）损坏时降级为空缓存即可。
/// 设备身份不走这里，见 [`try_load_identity`]。
fn read_json<T: serde::de::DeserializeOwned>(name: &str) -> Option<T> {
    match read_json_checked(name) {
        Ok(value) => value,
        Err(e) => {
            eprintln!("[storage] {e}");
            None
        }
    }
}

/// 原子写入：先写同目录 `*.tmp` 再 rename 覆盖。
/// 直接 `fs::write` 在掉电/崩溃时会留下半截 JSON，下次启动会把「文件损坏」
/// 误判成「没有身份」，从而静默换掉 device_id（触发 10121 设备风险）。
fn write_json<T: serde::Serialize>(name: &str, value: &T) -> Result<(), String> {
    let dir = exe_dir();
    let path = dir.join(name);
    let json =
        serde_json::to_string_pretty(value).map_err(|e| format!("序列化 {name} 失败: {e}"))?;
    let tmp = dir.join(format!("{name}.tmp"));
    std::fs::write(&tmp, json).map_err(|e| format!("写入 {} 失败: {e}", tmp.display()))?;
    match std::fs::rename(&tmp, &path) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(format!("替换 {} 失败: {e}", path.display()))
        }
    }
}

/// 旧身份自愈：机型名不在真机池、系统版本不在该机型的系统池、厂商残留、
/// 分辨率与机型不符时逐项归一并返回 true（表示需要落盘）。
///
/// 旧版「Android 模拟」留下的 identity.json 会被上一轮迁移只翻掉 `platform`，
/// 于是文件里留下 `deviceName="22081212C"`（小米机型）+ `osVersion="14"`（Android 14）
/// 这种「iOS 客户端 + 安卓机型」组合，属最容易被交叉校验识别的指纹。
fn normalize_ios_fingerprint(id: &mut HeaderIdentity) -> bool {
    let mut dirty = false;
    let fallback = HeaderIdentity::default();

    if crate::crypto::header::pixels_for_model(id.device_name.trim()).is_none() {
        id.device_name = fallback.device_name.clone();
        dirty = true;
    }

    let os_pool = crate::crypto::header::os_pool_for_model(&id.device_name);
    if !os_pool.is_empty() && !os_pool.iter().any(|candidate| *candidate == id.os_version.trim()) {
        id.os_version = os_pool[0].to_string();
        dirty = true;
    }

    if !id.manufacturer.is_empty() {
        id.manufacturer.clear();
        dirty = true;
    }

    dirty |= id.normalize_pixels_to_model();
    dirty
}

/// 加载设备身份；device_id / app_install_time 缺失时生成一次并立即落盘，
/// 此后同一设备全生命周期复用（逐请求漂移会影响设备一致性）。
///
/// 出错时**不改写** identity.json：解析失败或写盘失败都保留原文件，让用户能修复，
/// 而不是静默换一个新设备身份。调用方应把错误展示出来。
pub fn try_load_identity() -> Result<HeaderIdentity, String> {
    match load_identity_inner()? {
        (id, None) => Ok(id),
        // 读到了身份但没写回去（磁盘只读/权限不足）：身份本身可用，但必须让用户知道
        // 「本次改动没落盘」，否则下次启动会退回旧值。
        (_, Some(save_error)) => Err(save_error),
    }
}

/// 兼容旧调用点的宽松封装：读/解析失败时打印并返回默认身份（**不写盘**）；
/// 写盘失败时仍然返回内存中已归一好的身份，只打印告警。
/// 新代码（UI/CLI）请改用 [`try_load_identity`] 并把错误展示给用户。
pub fn load_identity() -> HeaderIdentity {
    match load_identity_inner() {
        Ok((id, save_error)) => {
            if let Some(e) = save_error {
                eprintln!("[identity] {e}");
            }
            id
        }
        Err(e) => {
            eprintln!("[identity] {e}");
            HeaderIdentity::default()
        }
    }
}

/// 返回 `(身份, 落盘告警)`：外层 Err 表示连身份都读不出来，没有可用值。
fn load_identity_inner() -> Result<(HeaderIdentity, Option<String>), String> {
    let mut id: HeaderIdentity = read_json_checked("identity.json")?.unwrap_or_default();
    let mut dirty = false;
    if id.device_id.is_empty() {
        id.device_id = uuid::Uuid::new_v4().to_string().to_uppercase();
        dirty = true;
    }
    if id.app_install_time <= 0 {
        id.app_install_time = HeaderIdentity::fresh_install_time();
        dirty = true;
    }
    // iOS WHSimulateIDFA 语义：UUID 一经生成即终身复用（真机存钥匙串）。
    // 空值先按 device_id 确定性派生再落盘，保证请求头/登录体/业务请求看到同一个 IDFA。
    if id.idfa.is_empty() {
        id.idfa = id.effective_idfa();
        dirty = true;
    }
    // 只保留 iOS：旧配置若为 android 则就地迁移。
    if id.platform != "ios" {
        id.platform = "ios".into();
        dirty = true;
    }
    // 无论是否迁移都做一次指纹自愈，修复上一轮「只翻了 platform」的半迁移文件。
    dirty |= normalize_ios_fingerprint(&mut id);
    let save_error = if dirty {
        save_identity(&id).err()
    } else {
        None
    };
    Ok((id, save_error))
}

pub fn save_identity(id: &HeaderIdentity) -> Result<(), String> {
    write_json("identity.json", id)
}

/// AI 项目列表永久缓存：拉取成功即落盘，网络异常时兜底展示。
pub fn load_ai_sports() -> Option<Vec<crate::api::ai::AiSport>> {
    read_json("ai_sports.json")
}

pub fn save_ai_sports(list: &[crate::api::ai::AiSport]) -> Result<(), String> {
    write_json("ai_sports.json", &list.to_vec())
}

pub fn load_session() -> Session {
    read_json("session.json").unwrap_or_default()
}

pub fn save_session(s: &Session) -> Result<(), String> {
    write_json("session.json", s)
}

pub fn clear_session() {
    let _ = std::fs::remove_file(exe_dir().join("session.json"));
}

pub fn load_config() -> Config {
    read_json("config.json").unwrap_or_default()
}

pub fn save_config(c: &Config) -> Result<(), String> {
    write_json("config.json", c)
}

#[cfg(test)]
mod storage_tests {
    use super::*;

    #[test]
    fn identity_is_stable_and_manual_edits_survive_reload() {
        let directory =
            std::env::temp_dir().join(format!("neko-identity-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&directory).unwrap();
        crate::platform::TEST_DATA_DIR.with(|p| *p.borrow_mut() = Some(directory.clone()));
        struct Cleanup(std::path::PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                crate::platform::TEST_DATA_DIR.with(|p| *p.borrow_mut() = None);
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let _cleanup = Cleanup(directory);
        let first = load_identity();
        assert_eq!(first.platform, "ios", "身份固定为 iOS");
        assert!(uuid::Uuid::parse_str(&first.device_id).is_ok());
        let again = load_identity();
        assert_eq!(
            serde_json::to_value(&first).unwrap(),
            serde_json::to_value(&again).unwrap()
        );

        // 手动改动只要落在真机机型池内就必须原样保留（含手填 IDFA）。
        let mut existing = first.clone();
        existing.device_name = "iPhone 15 Pro Max".into();
        existing.idfa = "manual-value".into();
        // 换机型后像素必须同步，否则加载时会按机型自愈覆盖（见下面的像素测试）
        assert!(existing.normalize_pixels_to_model());
        let mut legacy = serde_json::to_value(&existing).unwrap();
        legacy.as_object_mut().unwrap().remove("manufacturer");
        write_json("identity.json", &legacy).unwrap();
        let loaded = load_identity();
        assert_eq!(
            serde_json::to_value(&loaded).unwrap(),
            serde_json::to_value(&existing).unwrap(),
            "opening an existing install must not overwrite a manual identity"
        );

        let _ = first;
    }

    /// 原子写：落盘后不应残留 `identity.json.tmp`。
    #[test]
    fn write_is_atomic_and_leaves_no_tmp_file() {
        let directory = std::env::temp_dir().join(format!("neko-atomic-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&directory).unwrap();
        crate::platform::TEST_DATA_DIR.with(|p| *p.borrow_mut() = Some(directory.clone()));
        struct Cleanup(std::path::PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                crate::platform::TEST_DATA_DIR.with(|p| *p.borrow_mut() = None);
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let _cleanup = Cleanup(directory.clone());
        save_identity(&HeaderIdentity::default()).unwrap();
        assert!(directory.join("identity.json").is_file());
        assert!(
            !directory.join("identity.json.tmp").exists(),
            "临时文件必须已被 rename 消费"
        );
    }

    /// identity.json 损坏时必须报错且**不覆盖**原文件（覆盖 = 静默换设备 → 10121）。
    #[test]
    fn corrupted_identity_is_reported_and_not_overwritten() {
        let directory = std::env::temp_dir().join(format!("neko-corrupt-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&directory).unwrap();
        crate::platform::TEST_DATA_DIR.with(|p| *p.borrow_mut() = Some(directory.clone()));
        struct Cleanup(std::path::PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                crate::platform::TEST_DATA_DIR.with(|p| *p.borrow_mut() = None);
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let _cleanup = Cleanup(directory.clone());
        let broken = "{ \"device_id\": \"A\", ";
        std::fs::write(directory.join("identity.json"), broken).unwrap();

        let err = try_load_identity().expect_err("损坏的身份文件必须报错");
        assert!(err.contains("解析"), "错误信息应指出解析失败: {err}");
        assert_eq!(
            std::fs::read_to_string(directory.join("identity.json")).unwrap(),
            broken,
            "解析失败时不得改写原文件"
        );
        // 兼容封装不得写盘
        let fallback = load_identity();
        assert!(fallback.device_id.is_empty());
        assert_eq!(
            std::fs::read_to_string(directory.join("identity.json")).unwrap(),
            broken,
            "宽松封装同样不得覆盖损坏文件"
        );
    }

    #[test]
    fn session_roundtrip_and_logout_use_private_data_directory() {
        let directory = std::env::temp_dir().join(format!("neko-storage-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&directory).unwrap();
        crate::platform::TEST_DATA_DIR.with(|p| *p.borrow_mut() = Some(directory.clone()));
        struct Cleanup(std::path::PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                crate::platform::TEST_DATA_DIR.with(|p| *p.borrow_mut() = None);
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let _cleanup = Cleanup(directory.clone());
        assert_eq!(
            exe_dir(),
            directory,
            "storage must use the injected app directory"
        );
        let session = Session {
            uid: 123,
            token: "test-only-token".into(),
            ..Default::default()
        };
        save_session(&session).unwrap();
        assert!(directory.join("session.json").is_file());
        assert_eq!(load_session().uid, 123);
        assert_eq!(load_session().token, session.token);
        clear_session();
        assert!(!directory.join("session.json").exists());
        assert_eq!(load_session().uid, 0);
    }
}

/// 点位缓存：{ts_ms, points}，TTL 300s（服务端限流 10603：5 分钟 3 次）。
pub const POINTS_TTL_MS: i64 = 300_000;

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct PointsCache {
    ts: i64,
    anchor: crate::location::Coordinate,
    points: Vec<serde_json::Value>,
}

/// 只返回与本次锚点相同的缓存。旧版未记录锚点的缓存会自然失效，避免串城市。
pub fn load_points_cache_for(
    anchor: crate::location::Coordinate,
) -> Option<(i64, Vec<serde_json::Value>)> {
    let v: serde_json::Value = read_json("points_cache.json")?;
    let cache: PointsCache = serde_json::from_value(v).ok()?;
    if !cache.anchor.is_near(anchor, 0.0001) {
        return None;
    }
    let ts = cache.ts;
    let pts = cache.points;
    Some((ts, pts))
}

pub fn save_points_cache(
    anchor: crate::location::Coordinate,
    points: &[serde_json::Value],
) -> Result<(), String> {
    let doc = PointsCache {
        ts: crate::crypto::envelope::now_ms(),
        anchor,
        points: points.to_vec(),
    };
    write_json("points_cache.json", &doc)
}

#[cfg(test)]
mod points_cache_tests {
    use super::*;
    #[test]
    fn cache_is_scoped_to_anchor() {
        let cache = PointsCache {
            ts: 1,
            anchor: crate::location::Coordinate::new(39.9, 116.4, 0.0).unwrap(),
            points: vec![],
        };
        let decoded: PointsCache =
            serde_json::from_value(serde_json::to_value(cache).unwrap()).unwrap();
        assert!(decoded.anchor.is_near(
            crate::location::Coordinate::new(39.9, 116.4, 0.0).unwrap(),
            0.0001
        ));
        assert!(!decoded.anchor.is_near(
            crate::location::Coordinate::new(38.9, 121.5, 0.0).unwrap(),
            0.0001
        ));
    }
}

/// 围栏缓存：拉取成功后落盘，UI 预览无网时兜底。
pub fn load_fence_cache() -> Option<Vec<Vec<(f64, f64)>>> {
    let v: serde_json::Value = read_json("fence_cache.json")?;
    serde_json::from_value(v.get("fences")?.clone()).ok()
}

pub fn save_fence_cache(fences: &[Vec<(f64, f64)>]) -> Result<(), String> {
    let doc = serde_json::json!({
        "ts": crate::crypto::envelope::now_ms(),
        "fences": fences,
    });
    write_json("fence_cache.json", &doc)
}

#[cfg(test)]
mod legacy_config_tests {
    use super::*;

    /// 旧版 config.json 含有已删除的 `submit_form` 字段时，必须能正常加载（忽略未知键）。
    #[test]
    fn legacy_config_with_removed_submit_form_still_loads() {
        let legacy = serde_json::json!({
            "username": "u", "password": "", "remember": false,
            "weight": 68.0, "submit_form": "android",
        });
        let cfg: Config = serde_json::from_value(legacy).expect("旧配置必须能反序列化");
        assert_eq!(cfg.username, "u");
    }

    /// 旧配置缺少字段时，serde 缺省值必须与 [`Config::default`] 逐字段一致，
    /// 否则会得到「上下界相等」或「人脸开关被关掉」这类静默错误配置。
    #[test]
    fn config_serde_defaults_match_struct_default() {
        let bare: Config = serde_json::from_str("{}").expect("空对象必须能反序列化");
        let default = Config::default();
        assert_eq!(bare.dist_min, default.dist_min);
        assert_eq!(bare.dist_max, default.dist_max, "dist_max 缺省必须是 1.5");
        assert_eq!(bare.dist_max, 1.5);
        assert_eq!(bare.dist_min, 1.0);
        assert_eq!(bare.face_check, default.face_check, "face_check 缺省必须是 true");
        assert!(bare.face_check);
        assert_eq!(bare.pace_min, default.pace_min);
        assert_eq!(bare.pace_max, default.pace_max);
        assert_eq!(bare.ai_minutes, default.ai_minutes);
        assert_eq!(bare.ai_reps, default.ai_reps);
        assert_eq!(bare.update_check, default.update_check);
        assert_eq!(bare.route_mode, default.route_mode);
        assert_eq!(bare.osm_path, default.osm_path);
        assert_eq!(bare.manual_altitude, default.manual_altitude);
        assert_eq!(bare.manual_altitude_range, default.manual_altitude_range);
        assert_eq!(bare.remember, default.remember);
        // 语义自检：上下界不得退化
        assert!(bare.dist_max > bare.dist_min, "缺省距离区间不得退化");
    }

    /// 旧版 identity.json 的 platform=android 必须在加载时迁移为 ios，
    /// 且保留 device_id / IDFA / 安装时间等稳定指纹。
    #[test]
    fn legacy_android_identity_migrates_to_ios() {
        let dir = std::env::temp_dir().join(format!("neko-mig-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        crate::platform::TEST_DATA_DIR.with(|p| *p.borrow_mut() = Some(dir.clone()));
        struct Cleanup(std::path::PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                crate::platform::TEST_DATA_DIR.with(|p| *p.borrow_mut() = None);
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let _c = Cleanup(dir);

        let legacy = serde_json::json!({
            "platform": "android",
            "device_id": "LEGACY-DEVICE-ID",
            "idfa": "LEGACY-IDFA",
            "os_version": "14",
            "device_name": "22081212C",
            "app_install_time": 1_700_000_000_000i64,
            "logic_pixel": "393x852",
            "physic_pixel": "1179x2556",
            "city": "江门市",
            "anchor_lat": 22.4169,
            "anchor_lon": 112.9472,
        });
        write_json("identity.json", &legacy).unwrap();

        let loaded = load_identity();
        assert_eq!(loaded.platform, "ios", "旧 android 身份必须迁移为 iOS");
        assert_eq!(loaded.device_id, "LEGACY-DEVICE-ID", "device_id 必须保持不变");
        assert_eq!(loaded.idfa, "LEGACY-IDFA", "IDFA 必须保持不变");
        assert_eq!(loaded.app_install_time, 1_700_000_000_000);

        // 迁移不能只翻 platform：安卓机型名/安卓系统版本/厂商必须一起归一，
        // 否则会留下「iOS 客户端 + 22081212C + iOS 14 + Xiaomi」这种一眼假的组合。
        assert!(
            crate::crypto::header::pixels_for_model(&loaded.device_name).is_some(),
            "迁移后机型必须在真机池内，实际 {}",
            loaded.device_name
        );
        let os_pool = crate::crypto::header::os_pool_for_model(&loaded.device_name);
        assert!(
            os_pool.contains(&loaded.os_version.as_str()),
            "迁移后系统版本必须在机型系统池内，实际 {}（池 {:?}）",
            loaded.os_version,
            os_pool
        );
        assert!(loaded.manufacturer.is_empty(), "Android 厂商必须清空");
        let (logic, physic) = crate::crypto::header::pixels_for_model(&loaded.device_name).unwrap();
        assert_eq!(loaded.logic_pixel, logic, "分辨率必须跟随机型");
        assert_eq!(loaded.physic_pixel, physic, "分辨率必须跟随机型");

        // 迁移结果必须已落盘（含归一后的机型/系统）
        let stored: serde_json::Value = read_json("identity.json").unwrap();
        assert_eq!(stored["platform"], "ios");
        assert_eq!(stored["device_name"], loaded.device_name);
        assert_eq!(stored["os_version"], loaded.os_version);
        assert_eq!(stored["manufacturer"], "");
    }

    /// 上一轮半迁移的文件（platform 已是 ios，但机型/系统还是安卓的）必须自愈。
    #[test]
    fn half_migrated_ios_identity_is_self_healed() {
        let dir = std::env::temp_dir().join(format!("neko-heal-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        crate::platform::TEST_DATA_DIR.with(|p| *p.borrow_mut() = Some(dir.clone()));
        struct Cleanup(std::path::PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                crate::platform::TEST_DATA_DIR.with(|p| *p.borrow_mut() = None);
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let _c = Cleanup(dir);

        let broken = serde_json::json!({
            "platform": "ios",
            "device_id": "HALF-MIGRATED",
            "idfa": "IDFA-X",
            "os_version": "14",
            "device_name": "22081212C",
            "manufacturer": "Xiaomi",
            "app_install_time": 1_700_000_000_000i64,
            "logic_pixel": "393x852",
            "physic_pixel": "1179x2556",
            "city": "江门市",
            "anchor_lat": 22.4169,
            "anchor_lon": 112.9472,
        });
        write_json("identity.json", &broken).unwrap();

        let healed = load_identity();
        assert_eq!(healed.device_id, "HALF-MIGRATED", "自愈不得换设备");
        assert!(crate::crypto::header::pixels_for_model(&healed.device_name).is_some());
        assert!(
            crate::crypto::header::os_pool_for_model(&healed.device_name)
                .contains(&healed.os_version.as_str())
        );
        assert!(healed.manufacturer.is_empty());
        let (logic, physic) = crate::crypto::header::pixels_for_model(&healed.device_name).unwrap();
        assert_eq!((healed.logic_pixel.as_str(), healed.physic_pixel.as_str()), (logic, physic));
    }

    /// 分辨率必须跟随机型：手填/旧版残留的像素一律按机型池权威值纠正，
    /// 避免「机型与像素对不上」的交叉校验特征。
    #[test]
    fn legacy_inconsistent_pixel_ratio_is_normalized() {
        let dir = std::env::temp_dir().join(format!("neko-px-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        crate::platform::TEST_DATA_DIR.with(|p| *p.borrow_mut() = Some(dir.clone()));
        struct Cleanup(std::path::PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                crate::platform::TEST_DATA_DIR.with(|p| *p.borrow_mut() = None);
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let _c = Cleanup(dir);

        // 情形 1：没有 device_name（取默认机型），像素与机型不符
        let legacy = serde_json::json!({
            "platform": "ios",
            "device_id": "DEV",
            "logic_pixel": "393x852",
            "physic_pixel": "1080x2400",  // 明显不是 2x/3x，也不是默认机型的像素
            "city": "江门市",
            "anchor_lat": 22.4169,
            "anchor_lon": 112.9472,
        });
        write_json("identity.json", &legacy).unwrap();
        let loaded = load_identity();
        let (logic, physic) = crate::crypto::header::pixels_for_model(&loaded.device_name).unwrap();
        assert_eq!(
            (loaded.logic_pixel.as_str(), loaded.physic_pixel.as_str()),
            (logic, physic),
            "必须按机型纠正分辨率，实际 {} / {}",
            loaded.logic_pixel,
            loaded.physic_pixel
        );

        // 情形 2：机型明确但像素是别档的（iPhone 15 Pro + iPhone 16 的像素）
        let pinned = serde_json::json!({
            "platform": "ios",
            "device_id": "DEV2",
            "device_name": "iPhone 15 Pro",
            "os_version": "18.6.2",
            "logic_pixel": "440x956",
            "physic_pixel": "1320x2868",
            "city": "江门市",
            "anchor_lat": 22.4169,
            "anchor_lon": 112.9472,
        });
        write_json("identity.json", &pinned).unwrap();
        let loaded = load_identity();
        assert_eq!(loaded.device_name, "iPhone 15 Pro", "池内机型名不得被改写");
        assert_eq!(
            (loaded.logic_pixel.as_str(), loaded.physic_pixel.as_str()),
            ("393x852", "1179x2556"),
            "必须改回 iPhone 15 Pro 的像素"
        );
    }
}
