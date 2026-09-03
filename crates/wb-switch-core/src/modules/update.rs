//! 自动更新：检查公开 GitHub Releases 版本 + 更新源配置。
//!
//! 对照 server.py `load_github_config` / `save_github_config` /
//! `compare_versions` / `update_check`。下载安装走 tauri-plugin-updater（整包更新）。
//!
//! 版本检查不走 GitHub API（避免 60 次/小时/IP 限流），按优先级逐源降级：
//! 1. 主端点：release 资产的 updater manifest（GitHub 直连 + 国内镜像加速前缀，
//!    资产下载不计 API 配额）；
//! 2. 兜底端点：`github.com/{owner}/{repo}/releases/latest` 的 302 `Location`
//!    头解析 tag（仅直连，镜像服务不代理 releases 页面）；
//! 3. 终极兜底：jsDelivr CDN 读取仓库 `package.json` 的 version（省略版本号时
//!    返回最新 Release 标签对应的文件内容，国内网络一般可达）；
//! 4. 全部更新源失败时返回明确的降级提示，不影响软件其他功能；
//! 5. 成功结果进程级缓存 6 小时，缓存命中不发网络请求。

use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

use crate::modules::config::{
    atomic_write, http_request_raw_timeout, http_request_with_proxy_timeout, now_secs, store_dir,
};

/// 应用当前版本（来自 Cargo.toml package.version）。
pub const APP_VERSION: &str = env!("CARGO_PKG_VERSION");
pub const GITHUB_OWNER: &str = "heylumen";
pub const GITHUB_REPO: &str = "workbuddy-switch-tidy";

/// 默认国内镜像加速前缀：以「前缀 + 完整 GitHub URL」形式代理 release 资产下载。
/// 可在 github_config.json 的 `mirrors` 字段（字符串数组）中自定义覆盖；
/// 缺省或为空时使用该默认值。
pub const DEFAULT_MIRRORS: &[&str] = &["https://gh-proxy.com/", "https://ghproxy.net/"];

/// jsDelivr CDN 主机（多域名互为备用，国内网络一般可达）。
const JSDELIVR_HOSTS: &[&str] = &[
    "https://cdn.jsdelivr.net",
    "https://fastly.jsdelivr.net",
    "https://gcore.jsdelivr.net",
];

/// 检查更新单次请求超时（秒）。逐源降级策略下，单源超时不应拖累整体流程。
const UPDATE_TIMEOUT_SECS: u64 = 6;
/// 传输级失败（连不上/超时）时每个候选 URL 的最大尝试次数（含首次）。
const TRANSPORT_ATTEMPTS: usize = 2;

/// 成功结果缓存有效期（6 小时）。自动轮询（30 分钟）命中缓存，不发网络请求；
/// 设置页手动检查传 force=true 绕过缓存强制刷新。
const CACHE_TTL_SECS: i64 = 6 * 60 * 60;

/// 进程级内存缓存，只缓存 ok=true 的结果；失败不写缓存。
struct CachedCheck {
    checked_at: i64,
    value: Value,
}

static CACHE: Mutex<Option<CachedCheck>> = Mutex::new(None);

pub fn github_config_file() -> PathBuf {
    store_dir().join("github_config.json")
}

/// 规范镜像前缀：仅接受 `https://` 开头的地址（更新检查不向非加密源发请求），
/// 去空白并确保以 `/` 结尾；不合法返回空串（调用方丢弃该条目；
/// 若配置列表全部非法则回退内置默认镜像）。
fn normalize_mirror(m: &str) -> String {
    let t = m.trim();
    if !t.starts_with("https://") {
        return String::new();
    }
    if t.ends_with('/') {
        t.to_string()
    } else {
        format!("{t}/")
    }
}

fn default_mirrors() -> Vec<String> {
    DEFAULT_MIRRORS.iter().map(|s| s.to_string()).collect()
}

/// 从配置 JSON 的 `mirrors` 字段解析镜像前缀列表（空/非法时回退默认值）。
fn mirrors_from_config(cfg: &Value) -> Vec<String> {
    let parsed: Vec<String> = cfg
        .get("mirrors")
        .and_then(|v| v.as_array())
        .map(|list| {
            list.iter()
                .filter_map(|m| m.as_str())
                .map(normalize_mirror)
                .filter(|m| !m.is_empty())
                .collect()
        })
        .unwrap_or_default();
    if parsed.is_empty() {
        default_mirrors()
    } else {
        parsed
    }
}

/// 读取更新源配置（兼容旧配置文件，但永不返回 token）。
pub fn load_github_config() -> Value {
    let mut owner = GITHUB_OWNER.to_string();
    let mut repo = GITHUB_REPO.to_string();
    let mut proxy = String::new();
    let mut mirrors = default_mirrors();
    let f = github_config_file();
    let mut should_normalize = false;
    if f.exists() {
        if let Ok(text) = std::fs::read_to_string(&f) {
            if let Ok(v) = serde_json::from_str::<Value>(&text) {
                if let Some(value) = v.get("owner").and_then(|v| v.as_str()) {
                    if !value.trim().is_empty() {
                        owner = value.to_string();
                    }
                }
                if let Some(value) = v.get("repo").and_then(|v| v.as_str()) {
                    if !value.trim().is_empty() {
                        repo = value.to_string();
                    }
                }
                if let Some(value) = v.get("proxy").and_then(|v| v.as_str()) {
                    proxy = value.trim().to_string();
                }
                if v.get("mirrors").is_some() {
                    mirrors = mirrors_from_config(&v);
                }
                should_normalize = v.get("token").is_some();
            }
        }
    }
    // 旧版本截图/配置曾使用 changexbc/wb-switch；迁移到实际公开仓库。
    if owner == "changexbc" && repo == "wb-switch" {
        repo = GITHUB_REPO.to_string();
        should_normalize = true;
    }
    let normalized = json!({"owner": owner, "repo": repo, "proxy": proxy, "mirrors": mirrors});
    if should_normalize {
        let _ = atomic_write(
            &f,
            &serde_json::to_string_pretty(&normalized).unwrap_or_default(),
        );
    }
    normalized
}

/// 保存更新源配置；公开仓库不需要也不保存 GitHub token。
pub fn save_github_config(cfg: &Value) -> std::io::Result<()> {
    let owner = cfg
        .get("owner")
        .and_then(|v| v.as_str())
        .filter(|v| !v.trim().is_empty())
        .unwrap_or(GITHUB_OWNER);
    let repo = cfg
        .get("repo")
        .and_then(|v| v.as_str())
        .filter(|v| !v.trim().is_empty())
        .unwrap_or(GITHUB_REPO);
    let proxy = cfg
        .get("proxy")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .unwrap_or("");
    let mirrors = mirrors_from_config(cfg);
    let clean = json!({"owner": owner, "repo": repo, "proxy": proxy, "mirrors": mirrors});
    std::fs::create_dir_all(store_dir())?;
    atomic_write(
        &github_config_file(),
        &serde_json::to_string_pretty(&clean).unwrap_or_default(),
    )
}

fn version_tuple(v: &str) -> Vec<i64> {
    v.trim_start_matches('v')
        .split('.')
        .filter_map(|x| x.parse::<i64>().ok())
        .collect()
}

/// 版本比较：a > b 返回 1，a < b 返回 -1，相等返回 0。
pub fn compare_versions(a: &str, b: &str) -> i64 {
    let ta = version_tuple(a);
    let tb = version_tuple(b);
    for i in 0..ta.len().max(tb.len()) {
        let x = ta.get(i).copied().unwrap_or(0);
        let y = tb.get(i).copied().unwrap_or(0);
        if x != y {
            return if x > y { 1 } else { -1 };
        }
    }
    0
}

/// updater manifest 候选 URL（按优先级）。
///
/// 1. 合并后的 `latest.json`（含各平台）；
/// 2. 当前系统的 `latest-<os>-<arch>.json`；
/// 3. 兼容旧 Windows 安装包：它们仍请求 `latest-macos-<arch>.json`。
pub fn updater_manifest_urls(owner: &str, repo: &str, os: &str, arch: &str) -> Vec<String> {
    let os_slug = match os {
        "macos" | "darwin" => "macos",
        other => other,
    };
    let mut urls = vec![format!(
        "https://github.com/{owner}/{repo}/releases/latest/download/latest.json"
    )];
    urls.push(format!(
        "https://github.com/{owner}/{repo}/releases/latest/download/latest-{os_slug}-{arch}.json"
    ));
    if os_slug != "macos" {
        urls.push(format!(
            "https://github.com/{owner}/{repo}/releases/latest/download/latest-macos-{arch}.json"
        ));
    }
    urls.dedup();
    urls
}

/// 生成一个 GitHub 直连 URL 的候选请求地址：直连优先，其后依次拼接各镜像前缀。
fn candidate_urls(github_url: &str, mirrors: &[String]) -> Vec<String> {
    let mut urls = vec![github_url.to_string()];
    urls.extend(mirrors.iter().map(|m| format!("{m}{github_url}")));
    urls
}

/// 主端点：逐候选拉取 updater manifest（直连 + 镜像前缀）。
/// 成功返回（解析后的 JSON, 实际命中的 URL），失败返回最后一个候选的可读错误。
async fn fetch_manifest_version(
    owner: &str,
    repo: &str,
    proxy: Option<&str>,
    mirrors: &[String],
) -> Result<(Value, String), String> {
    let mut headers = HashMap::new();
    headers.insert("Accept".to_string(), "application/json".to_string());
    headers.insert("User-Agent".to_string(), "wb-switch".to_string());
    let mut last_err = "更新清单不可用".to_string();
    for github_url in
        updater_manifest_urls(owner, repo, std::env::consts::OS, std::env::consts::ARCH)
    {
        for url in candidate_urls(&github_url, mirrors) {
            for attempt in 0..TRANSPORT_ATTEMPTS {
                let resp = http_request_with_proxy_timeout(
                    &url,
                    "GET",
                    None,
                    Some(&headers),
                    proxy,
                    UPDATE_TIMEOUT_SECS,
                )
                .await;
                let version = resp
                    .get("version")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .trim()
                    .to_string();
                if !version.is_empty() {
                    return Ok((resp, url));
                }
                let code = resp.get("code").and_then(|v| v.as_i64()).unwrap_or(0);
                let message = resp
                    .get("message")
                    .and_then(|v| v.as_str())
                    .unwrap_or("更新清单解析失败")
                    .to_string();
                if code >= 0 {
                    // 端点可达但无该资产（如 404）：换下一个候选，不做传输重试。
                    last_err = format!("HTTP {code}: {message}");
                    break;
                }
                // 传输级失败（code=-1）：间隔 200ms 后重试，仍失败则换下一个候选。
                last_err = message;
                if attempt + 1 < TRANSPORT_ATTEMPTS {
                    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                }
            }
        }
    }
    Err(last_err)
}

/// 兜底端点：请求 `/releases/latest`，读 302 `Location` 头（形如
/// `.../releases/tag/v0.1.13`）解析 tag。不跟随重定向，避免拉到 HTML 页面。
/// 仅直连尝试（镜像服务不代理 releases 页面）。成功返回 tag，失败返回可读错误 + code。
async fn fetch_latest_tag(
    owner: &str,
    repo: &str,
    proxy: Option<&str>,
) -> Result<String, (String, i64)> {
    let url = format!("https://github.com/{owner}/{repo}/releases/latest");
    let mut headers = HashMap::new();
    headers.insert("Accept".to_string(), "text/html".to_string());
    headers.insert("User-Agent".to_string(), "wb-switch".to_string());
    let (status, resp_headers, body) = http_request_raw_timeout(
        &url,
        "GET",
        None,
        Some(&headers),
        proxy,
        false,
        UPDATE_TIMEOUT_SECS,
    )
    .await;

    if status == 0 {
        let msg = if body.trim().is_empty() {
            "网络请求失败".to_string()
        } else {
            body
        };
        return Err((msg, -1));
    }
    if status == 404 {
        // 无正式 release 或仓库不存在。
        return Err(("未找到可用的发布版本".to_string(), 404));
    }
    let location = resp_headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("location"))
        .map(|(_, v)| v.clone());
    let location = match location {
        Some(location) => location,
        None => {
            return Err((
                format!("无法获取发布页跳转地址（HTTP {status}）"),
                status as i64,
            ))
        }
    };
    let tag = location.rsplit('/').next().unwrap_or("").trim().to_string();
    if tag.is_empty() || !location.contains("/releases/tag/") {
        return Err(("无法解析发布版本标签".to_string(), -1));
    }
    Ok(tag)
}

/// 终极兜底：jsDelivr CDN 读取仓库 `package.json` 的 version（省略版本号时
/// 返回最新 Release 标签对应的文件内容，国内网络一般可达）。
async fn fetch_jsdelivr_version(
    owner: &str,
    repo: &str,
    proxy: Option<&str>,
) -> Result<String, String> {
    let mut headers = HashMap::new();
    headers.insert("Accept".to_string(), "application/json".to_string());
    headers.insert("User-Agent".to_string(), "wb-switch".to_string());
    let mut last_err = "jsDelivr CDN 不可达".to_string();
    for host in JSDELIVR_HOSTS {
        let url = format!("{host}/gh/{owner}/{repo}/package.json");
        let resp = http_request_with_proxy_timeout(
            &url,
            "GET",
            None,
            Some(&headers),
            proxy,
            UPDATE_TIMEOUT_SECS,
        )
        .await;
        if let Some(version) = resp.get("version").and_then(|v| v.as_str()) {
            let version = version.trim().trim_start_matches('v').to_string();
            if !version.is_empty() {
                return Ok(version);
            }
        }
        if let Some(message) = resp.get("message").and_then(|v| v.as_str()) {
            last_err = message.to_string();
        }
    }
    Err(last_err)
}

/// 查询最新 Release，与本地版本对比。对照 server.py `update_check`。
///
/// `force=true` 绕过缓存强制刷新（设置页手动检查）；否则 6 小时内成功结果直接返回，
/// 不发网络请求。三个更新源按优先级逐级降级；全部失败时返回明确的降级提示。
pub async fn update_check(proxy: Option<&str>, force: bool) -> Value {
    if !force {
        if let Some(cached) = CACHE.lock().unwrap().as_ref() {
            if now_secs() - cached.checked_at < CACHE_TTL_SECS {
                return cached.value.clone();
            }
        }
    }

    let cfg = load_github_config();
    let owner = cfg
        .get("owner")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let repo = cfg
        .get("repo")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let configured_proxy = cfg
        .get("proxy")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|v| !v.is_empty());
    let proxy = proxy.or(configured_proxy);
    let mirrors = mirrors_from_config(&cfg);
    let release_url = format!("https://github.com/{owner}/{repo}/releases/latest");
    let current = APP_VERSION.to_string();
    let mut failures: Vec<String> = Vec::new();

    // 层 1：updater manifest（GitHub 直连 + 国内镜像加速，资产下载不计 API 配额）。
    match fetch_manifest_version(&owner, &repo, proxy, &mirrors).await {
        Ok((manifest, via)) => {
            let version = manifest
                .get("version")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim();
            let latest = version.strip_prefix('v').unwrap_or(version).to_string();
            let tag = format!("v{latest}");
            let release_name = manifest
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or(&tag)
                .to_string();
            let published_at = manifest
                .get("pub_date")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            let value = json!({
                "ok": true,
                "current": current,
                "latest": latest,
                "latestTag": tag,
                "hasUpdate": compare_versions(&latest, &current) > 0,
                "releaseName": release_name,
                "releaseUrl": release_url,
                "publishedAt": published_at,
                "source": via,
                "checkedAt": now_secs(),
            });
            *CACHE.lock().unwrap() = Some(CachedCheck {
                checked_at: now_secs(),
                value: value.clone(),
            });
            return value;
        }
        Err(err) => failures.push(format!("更新清单（直连+镜像）: {err}")),
    }

    // 层 2：`/releases/latest` 的 302 Location 头解析 tag（仅 GitHub 直连，VPN 场景）。
    match fetch_latest_tag(&owner, &repo, proxy).await {
        Ok(tag) => {
            let latest = tag.strip_prefix('v').unwrap_or(&tag).to_string();
            let value = json!({
                "ok": true,
                "current": current,
                "latest": latest,
                "latestTag": tag,
                "hasUpdate": compare_versions(&latest, &current) > 0,
                "releaseName": tag.clone(),
                "releaseUrl": release_url,
                "source": "release-redirect",
                "checkedAt": now_secs(),
            });
            *CACHE.lock().unwrap() = Some(CachedCheck {
                checked_at: now_secs(),
                value: value.clone(),
            });
            value
        }
        Err((msg, code)) => {
            failures.push(format!("release 页跳转: {msg}（code={code}）"));
            // 继续走层 3。
            check_jsdelivr_fallback(&owner, &repo, proxy, &current, &release_url, &mut failures)
                .await
        }
    }
}

/// 层 3：jsDelivr CDN 兜底；全部失败时返回降级提示。
async fn check_jsdelivr_fallback(
    owner: &str,
    repo: &str,
    proxy: Option<&str>,
    current: &str,
    release_url: &str,
    failures: &mut Vec<String>,
) -> Value {
    match fetch_jsdelivr_version(owner, repo, proxy).await {
        Ok(latest) => {
            let tag = format!("v{latest}");
            let value = json!({
                "ok": true,
                "current": current,
                "latest": latest,
                "latestTag": tag,
                "hasUpdate": compare_versions(&latest, current) > 0,
                "releaseName": tag.clone(),
                "releaseUrl": release_url,
                "source": "jsdelivr-cdn",
                "checkedAt": now_secs(),
            });
            *CACHE.lock().unwrap() = Some(CachedCheck {
                checked_at: now_secs(),
                value: value.clone(),
            });
            value
        }
        Err(err) => {
            failures.push(format!("jsDelivr CDN: {err}"));
            let detail = failures.join("；");
            let message = format!(
                "检查更新失败：GitHub 直连、国内镜像加速与 jsDelivr CDN 均无法访问。\
                 请检查网络连接后重试；该失败不影响软件其他功能。详情：{detail}"
            );
            json!({
                "ok": false,
                "error": "所有更新源均不可达",
                "message": message,
                "releaseUrl": release_url,
                "checkedAt": now_secs(),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn updater_manifest_urls_macos_skips_duplicate_fallback() {
        let urls = updater_manifest_urls("changexbc", "workbuddy-switch", "macos", "aarch64");
        assert_eq!(
            urls,
            vec![
                "https://github.com/changexbc/workbuddy-switch/releases/latest/download/latest.json",
                "https://github.com/changexbc/workbuddy-switch/releases/latest/download/latest-macos-aarch64.json",
            ]
        );
    }

    #[test]
    fn updater_manifest_urls_windows_keeps_macos_compat() {
        let urls = updater_manifest_urls("changexbc", "workbuddy-switch", "windows", "x86_64");
        assert_eq!(
            urls,
            vec![
                "https://github.com/changexbc/workbuddy-switch/releases/latest/download/latest.json",
                "https://github.com/changexbc/workbuddy-switch/releases/latest/download/latest-windows-x86_64.json",
                "https://github.com/changexbc/workbuddy-switch/releases/latest/download/latest-macos-x86_64.json",
            ]
        );
    }

    #[test]
    fn compare_versions_orders_semver() {
        assert_eq!(compare_versions("0.1.18", "0.1.18"), 0);
        assert_eq!(compare_versions("0.1.19", "0.1.18"), 1);
        assert_eq!(compare_versions("v0.1.17", "0.1.18"), -1);
    }

    #[test]
    fn candidate_urls_put_direct_before_mirrors() {
        let mirrors = vec!["https://gh-proxy.com/".to_string()];
        let urls = candidate_urls(
            "https://github.com/a/b/releases/latest/download/latest.json",
            &mirrors,
        );
        assert_eq!(
            urls,
            vec![
                "https://github.com/a/b/releases/latest/download/latest.json",
                "https://gh-proxy.com/https://github.com/a/b/releases/latest/download/latest.json",
            ]
        );
    }

    #[test]
    fn normalize_mirror_appends_trailing_slash() {
        assert_eq!(
            normalize_mirror(" https://ghproxy.net "),
            "https://ghproxy.net/"
        );
        assert_eq!(normalize_mirror("https://x.com/"), "https://x.com/");
        assert_eq!(normalize_mirror("   "), "");
    }

    #[test]
    fn normalize_mirror_rejects_non_https_prefixes() {
        // 仅接受 https://：明文 http、缺 scheme、垃圾输入一律丢弃（返回空串）。
        assert_eq!(normalize_mirror("http://insecure.example/"), "");
        assert_eq!(normalize_mirror("ghproxy.net/"), "");
        assert_eq!(normalize_mirror("ftp://x/"), "");
    }

    #[test]
    fn mirrors_from_config_falls_back_to_defaults() {
        assert_eq!(
            mirrors_from_config(&json!({})),
            default_mirrors(),
            "缺省 mirrors 时应回退内置默认镜像"
        );
        assert_eq!(
            mirrors_from_config(&json!({"mirrors": []})),
            default_mirrors(),
            "空 mirrors 数组也应回退内置默认镜像"
        );
        assert_eq!(
            mirrors_from_config(&json!({"mirrors": ["https://my-mirror.example"]})),
            vec!["https://my-mirror.example/".to_string()],
            "自定义镜像应去空白并补全尾部斜杠"
        );
    }
}
