//! 桌面端自重启：自更新替换完成后拉起新版本进程。

/// 启动新版本程序，旧进程随后退出。
///
/// 占位实现：用自身 exe 路径以子进程方式启动的代码暂缓启用（无参数、
/// 不经过 shell），当前更新替换完成后提示用户手动重启。启用时将本函数
/// 体替换为：定位 current_exe 后以子进程方式启动即可。
/// 当前实现是否支持自重启（占位实现返回 false，UI 据此改文案）。
#[cfg_attr(not(feature = "gui"), allow(dead_code))]
pub fn available() -> bool {
    false
}

#[cfg_attr(not(feature = "gui"), allow(dead_code))]
pub fn relaunch_self() -> Result<(), String> {
    Err("更新已完成，请关闭并重新打开程序以使用新版本".into())
}
