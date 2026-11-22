//! 会话身份的纯逻辑：标题选择、工作区归一、正文去标识、项目正文索引。
//!
//! 从 `session.rs` 拆出（4.2 拆分第 2 步，纯搬移、行为不变）。
//! 这些函数不碰数据库、不加锁，是「同一条会话在多个账号下是否等价」判定的基础件，
//! 单独成模块便于审阅与测试。

use std::path::{Path, PathBuf};

use super::store::nonempty_text;

/// WorkBuddy 侧栏展示名：优先 custom_title（用户改名 / 定时任务名），否则 title。
pub(crate) fn session_display_title(title: Option<String>, custom_title: Option<String>) -> String {
    nonempty_text(custom_title)
        .or_else(|| nonempty_text(title))
        .unwrap_or_else(|| "(无标题)".to_string())
}

/// Claw 是账号绑定的 IM 渠道工作区，复制会话行不够，目标账号也用不了。
pub(crate) fn is_claw_workspace(cwd: &str) -> bool {
    cwd.trim()
        .trim_end_matches(['/', '\\'])
        .rsplit(['/', '\\'])
        .next()
        .is_some_and(|name| name.eq_ignore_ascii_case("claw"))
}

/// 归一化工作目录：去掉首尾空白与尾部路径分隔符，用于跨副本的 cwd 比对。
pub(crate) fn normalize_cwd(cwd: &str) -> String {
    cwd.trim().trim_end_matches(['/', '\\']).to_string()
}

/// 会话正文去标识：把「本会话自身 id」替换为中性占位符，
/// 使复制产生的不同 id 副本在比对时视为同一份内容。
/// 复制时会把源 cid 替换为新 cid，故正文里出现的是会话自身 id。
const NEUTRAL_ID: &str = "\u{0}WB_SWITCH_NEUTRAL_ID\u{0}";

pub(crate) fn normalize_jsonl(text: &str, cid: &str) -> String {
    text.replace(cid, NEUTRAL_ID)
}

/// 一次性索引指定 `projects/` 下所有 `{cid}.jsonl`，返回 cid -> 路径。
///
/// 查找范围与 `find_project_jsonl` 一致（直接文件 + 各 workspace 子目录）。
/// 多档位/测试注入都必须走这里，避免固定读当前用户主目录而漏掉另一档位的数据。
pub(crate) fn index_project_jsonls_in(
    projects: &Path,
) -> std::collections::HashMap<String, PathBuf> {
    let mut map = std::collections::HashMap::new();
    if !projects.is_dir() {
        return map;
    }
    let mut visit = |p: PathBuf| {
        if let Some(stem) = p.file_stem().and_then(|s| s.to_str()) {
            map.entry(stem.to_string()).or_insert(p);
        }
    };
    if let Ok(entries) = std::fs::read_dir(projects) {
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_file() {
                visit(p);
            } else if p.is_dir() {
                if let Ok(sub) = std::fs::read_dir(&p) {
                    for f in sub.flatten() {
                        let fp = f.path();
                        if fp.is_file() {
                            visit(fp);
                        }
                    }
                }
            }
        }
    }
    map
}
