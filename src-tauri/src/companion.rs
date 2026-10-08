//! Agent Companion（会话悬浮窗）——**本 fork 不含此功能**。
//!
//! 上游把 Agent Companion 作为独立项目 vendored 进仓库、构建成 sidecar 二进制随包分发；
//! 本 fork 的交付形态是**便携单 EXE**，故不集成该 sidecar，仅保留同名命令的降级实现：
//! 前端调用不会失败（保持接口兼容），但开关恒为关闭、操作会返回可读提示。
//!
//! 若将来需要，可参考上游 `scripts/prepare-agent-companion.mjs` 与 `vendor/agent-companion`。

use serde_json::Value;

/// 悬浮窗功能是否可用（本 fork 恒为 false）。
#[allow(dead_code)]
pub fn is_available() -> bool {
    false
}

/// 读取悬浮窗开关状态（恒为 false）。
#[tauri::command]
pub fn get_companion_enabled(_app: tauri::AppHandle) -> bool {
    false
}

/// 设置悬浮窗开关（本 fork 不支持，返回空值表示无状态变更）。
#[tauri::command(rename_all = "camelCase")]
pub async fn set_companion_enabled(_app: tauri::AppHandle, _enabled: bool) -> Result<Value, String> {
    Err("本版本不含「会话悬浮窗」功能".to_string())
}

/// 打开悬浮窗设置（本 fork 不支持）。
#[tauri::command]
pub async fn open_companion_settings(_app: tauri::AppHandle) -> Result<(), String> {
    Err("本版本不含「会话悬浮窗」设置".to_string())
}

/// 托盘菜单：切换悬浮窗（本 fork 无操作）。
pub fn toggle_rail(_app: &tauri::AppHandle) {}

/// 托盘菜单：打开悬浮窗设置（本 fork 无操作）。
pub fn open_settings_from_tray(_app: &tauri::AppHandle) {}
