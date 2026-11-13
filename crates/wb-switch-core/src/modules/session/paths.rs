use std::path::PathBuf;

use crate::modules::config::store_dir;
use crate::modules::variant::WbVariant;

/// 关联存储的命名空间：决定关联表 / 基线 / 预览凭据 / 存储锁的名字。
///
/// 三个宿主（WorkBuddy 桌面版、VS Code CodeBuddy 插件、CodeBuddy IDE）共用同一份内核
/// （[`crate::modules::session_link`]），但各自的关联关系互不可见：
/// 同一工具存储根下按命名空间取不同的文件名与目录名，避免互相污染。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum LinkNamespace {
    /// WorkBuddy 桌面版（默认，路径与改造前逐字相同）。
    #[default]
    WorkBuddy,
    /// VS Code CodeBuddy 插件的会话（独立文件名与目录）。
    VscodeExt,
    /// CodeBuddy IDE（国内版桌面客户端）的会话（独立文件名与目录）。
    CodeBuddyIde,
}

/// 会话操作涉及的路径集合：工具存储根（`~/.wb-switch`）与档位数据根。
///
/// 生产入口用 [`SessionPaths::for_variant`]（WorkBuddy）与 [`SessionPaths::for_vscode_ext`]
/// （VS Code 插件）；单测注入临时目录，绝不触碰真实 `~/.wb-switch` 或客户端数据目录。
#[derive(Clone, Debug)]
pub struct SessionPaths {
    /// 工具存储根：关联表、基线、操作日志、锁与备份都在这里。
    pub store_root: PathBuf,
    /// 档位数据根：`projects/`、`workbuddy.db`、`edge-sync-mapping-*.db`。
    /// VS Code 命名空间下不使用该字段（恒为空路径）。
    pub data_root: PathBuf,
    /// 该档位的官方登录态文件（来源账号 uid 的判据）。
    /// VS Code 命名空间下不使用该字段（恒为空路径）。
    pub auth_file: PathBuf,
    /// 关联存储命名空间；决定下面几个 `*_links*` 路径的名字。
    pub link_namespace: LinkNamespace,
}

impl SessionPaths {
    pub fn for_variant(variant: WbVariant) -> Self {
        Self {
            store_root: store_dir(),
            data_root: variant.data_root(),
            auth_file: variant.auth_file_path(),
            link_namespace: LinkNamespace::WorkBuddy,
        }
    }

    /// VS Code CodeBuddy 插件的关联存储路径。
    ///
    /// 只用到 `store_root`：关联表 / 基线 / 预览凭据 / 存储锁都落在 `~/.wb-switch` 下
    /// VS Code 专属的名字里（design §2）；扩展的会话文件由调用方按数据根另行解析，
    /// 不走 `data_root` / `auth_file`（因此两者留空，避免误用）。
    pub fn for_vscode_ext() -> Self {
        Self::for_vscode_ext_at(store_dir())
    }

    /// [`Self::for_vscode_ext`] 的可测实现：显式传入工具存储根。
    pub fn for_vscode_ext_at(store_root: PathBuf) -> Self {
        Self {
            store_root,
            data_root: PathBuf::new(),
            auth_file: PathBuf::new(),
            link_namespace: LinkNamespace::VscodeExt,
        }
    }

    /// CodeBuddy IDE（国内版桌面客户端）的关联存储路径。
    ///
    /// 与 [`Self::for_vscode_ext`] 同构：只用到 `store_root`，会话文件由调用方按数据根另行解析。
    pub fn for_codebuddy_ide() -> Self {
        Self::for_codebuddy_ide_at(store_dir())
    }

    /// [`Self::for_codebuddy_ide`] 的可测实现：显式传入工具存储根。
    pub fn for_codebuddy_ide_at(store_root: PathBuf) -> Self {
        Self {
            store_root,
            data_root: PathBuf::new(),
            auth_file: PathBuf::new(),
            link_namespace: LinkNamespace::CodeBuddyIde,
        }
    }

    pub fn workbuddy_db(&self) -> PathBuf {
        self.data_root.join("workbuddy.db")
    }

    pub fn projects_dir(&self) -> PathBuf {
        self.data_root.join("projects")
    }

    pub fn edge_sync_db(&self, variant: WbVariant) -> PathBuf {
        super::edge_sync_db_path(&self.data_root, variant)
    }

    pub fn backup_root(&self) -> PathBuf {
        self.store_root.join("backups")
    }

    /// 关联组主表：三个目标各一份，互不可见（design §2）。
    pub fn session_links_file(&self) -> PathBuf {
        match self.link_namespace {
            LinkNamespace::WorkBuddy => self.store_root.join("session_links.json"),
            LinkNamespace::VscodeExt => self.store_root.join("vscode_session_links.json"),
            LinkNamespace::CodeBuddyIde => self.store_root.join("codebuddy_ide_session_links.json"),
        }
    }

    /// 关联存储目录（基线 / 凭据 / 操作日志的父目录）。
    pub fn session_links_dir(&self) -> PathBuf {
        match self.link_namespace {
            LinkNamespace::WorkBuddy => self.store_root.join("session-links"),
            LinkNamespace::VscodeExt => self.store_root.join("vscode-session-links"),
            LinkNamespace::CodeBuddyIde => self.store_root.join("codebuddy-ide-session-links"),
        }
    }

    pub fn baselines_dir(&self) -> PathBuf {
        self.session_links_dir().join("baselines")
    }

    pub fn operations_dir(&self) -> PathBuf {
        self.session_links_dir().join("operations")
    }

    /// 预览凭据目录（design §6：绑定保存在服务端，前端只拿 id）。
    /// 不属于未完成痕迹——凭据是一次性的，主文件缺失时不得据此拒绝初始化。
    pub fn preview_tokens_dir(&self) -> PathBuf {
        self.session_links_dir().join("previews")
    }

    pub fn locks_dir(&self) -> PathBuf {
        self.store_root.join("locks")
    }

    pub fn variant_ops_lock_file(&self, variant: WbVariant) -> PathBuf {
        self.locks_dir()
            .join(format!("session-ops-{}.lock", variant.as_str()))
    }

    /// Long-running client-scoped session operation lock. IDE and VS Code stores are shared
    /// across WorkBuddy variants, so their mutations must serialize across both variants.
    pub fn client_ops_lock_file(&self) -> PathBuf {
        let name = match self.link_namespace {
            LinkNamespace::WorkBuddy => "workbuddy",
            LinkNamespace::VscodeExt => "vscode-ext",
            LinkNamespace::CodeBuddyIde => "codebuddy-ide",
        };
        self.locks_dir().join(format!("session-client-{name}.lock"))
    }

    /// 关联存储的短时全局锁文件：与主表同生命周期，按命名空间分开。
    pub fn link_store_lock_file(&self) -> PathBuf {
        match self.link_namespace {
            LinkNamespace::WorkBuddy => self.locks_dir().join("session-links.lock"),
            LinkNamespace::VscodeExt => self.locks_dir().join("vscode-session-links.lock"),
            LinkNamespace::CodeBuddyIde => {
                self.locks_dir().join("codebuddy-ide-session-links.lock")
            }
        }
    }
}
