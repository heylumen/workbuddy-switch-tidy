//! 会话复制/关联/恢复的端到端单测。
//!
//! 所有用例都在临时目录里构造数据根与存储根，绝不读写真实的 `~/.wb-switch`
//! 或 WorkBuddy 数据目录。

use super::*;
use crate::modules::session_link::{LinkStore, MemberState, Operation, StoreState};
use serde_json::json;

struct Env {
    root: PathBuf,
    paths: SessionPaths,
}

impl Env {
    fn new(name: &str) -> Self {
        let root = std::env::temp_dir().join(format!(
            "wb_switch_copy_test_{}_{name}",
            uuid::Uuid::new_v4().simple()
        ));
        let paths = SessionPaths {
            store_root: root.join("store"),
            data_root: root.join("data"),
            auth_file: root.join("auth.info"),
            link_namespace: LinkNamespace::WorkBuddy,
        };
        std::fs::create_dir_all(paths.projects_dir().join("ws-a")).unwrap();
        Env { root, paths }
    }

    fn paths(&self) -> SessionPaths {
        self.paths.clone()
    }

    fn set_login(&self, uid: &str) {
        std::fs::write(
            &self.paths.auth_file,
            json!({"account": {"uid": uid}}).to_string(),
        )
        .unwrap();
    }

    fn create_db(&self) {
        let conn = Connection::open(self.paths.workbuddy_db()).unwrap();
        conn.execute_batch(
            "CREATE TABLE sessions (
                    id TEXT PRIMARY KEY,
                    user_id TEXT NOT NULL,
                    title TEXT,
                    custom_title TEXT,
                    cwd TEXT,
                    created_at INTEGER,
                    updated_at INTEGER,
                    deleted_at INTEGER,
                    is_playground INTEGER
                );",
        )
        .unwrap();
    }

    fn create_edge_db(&self, variant: WbVariant) {
        let conn = Connection::open(self.paths.edge_sync_db(variant)).unwrap();
        conn.execute_batch(
            "CREATE TABLE edge_sync_mapping (
                    session_id TEXT,
                    conversation_id TEXT,
                    msg_channel TEXT,
                    created_at INTEGER
                );",
        )
        .unwrap();
    }

    /// 带 `sessions.status` 列的表：真实库有该列，默认建表没有。
    ///
    /// 默认值与真实库一致（`Pending`，不在终态白名单内），避免「缺省即可归档」。
    fn create_db_with_status(&self) {
        self.create_db_with_status_column("status TEXT NOT NULL DEFAULT 'Pending'");
    }

    /// `status` 可空的历史形态：验证「NULL 不授予归档权限」时必须能真的写出 NULL。
    fn create_db_with_nullable_status(&self) {
        self.create_db_with_status_column("status TEXT");
    }

    fn create_db_with_status_column(&self, status_column: &str) {
        let conn = Connection::open(self.paths.workbuddy_db()).unwrap();
        conn.execute_batch(&format!(
            "CREATE TABLE sessions (
                    id TEXT PRIMARY KEY,
                    user_id TEXT NOT NULL,
                    title TEXT,
                    custom_title TEXT,
                    {status_column},
                    cwd TEXT,
                    created_at INTEGER,
                    updated_at INTEGER,
                    deleted_at INTEGER,
                    is_playground INTEGER
                );"
        ))
        .unwrap();
    }

    fn add_session(&self, id: &str, uid: &str, title: &str) {
        let conn = Connection::open(self.paths.workbuddy_db()).unwrap();
        conn.execute(
                "INSERT INTO sessions (id, user_id, title, custom_title, cwd, created_at, updated_at, deleted_at, is_playground)
                 VALUES (?1, ?2, ?3, NULL, '/ws/a', 1000, 2000, NULL, 0)",
                rusqlite::params![id, uid, title],
            )
            .unwrap();
    }

    fn add_body(&self, cid: &str, text: &str) -> PathBuf {
        let path = self
            .paths
            .projects_dir()
            .join("ws-a")
            .join(format!("{cid}.jsonl"));
        std::fs::write(&path, text).unwrap();
        path
    }

    fn body_path(&self, cid: &str) -> PathBuf {
        self.paths
            .projects_dir()
            .join("ws-a")
            .join(format!("{cid}.jsonl"))
    }

    fn delete_row(&self, id: &str) {
        let conn = Connection::open(self.paths.workbuddy_db()).unwrap();
        conn.execute("DELETE FROM sessions WHERE id = ?1", [id])
            .unwrap();
    }

    /// 设置会话状态（模拟用户归档/取消归档、App 写入活跃态）。
    fn set_status(&self, id: &str, status: &str) {
        let conn = Connection::open(self.paths.workbuddy_db()).unwrap();
        conn.execute(
            "UPDATE sessions SET status = ?1 WHERE id = ?2",
            rusqlite::params![status, id],
        )
        .unwrap();
    }

    /// 把状态置为 NULL（旧库/NULL 语义），验证白名单不授予 NULL 归档权限。
    fn set_status_null(&self, id: &str) {
        let conn = Connection::open(self.paths.workbuddy_db()).unwrap();
        conn.execute("UPDATE sessions SET status = NULL WHERE id = ?1", [id])
            .unwrap();
    }

    /// 设置更新时间戳（构造「本次时间戳已落库」的中断现场）。
    fn set_updated_at(&self, id: &str, updated_at: i64) {
        let conn = Connection::open(self.paths.workbuddy_db()).unwrap();
        conn.execute(
            "UPDATE sessions SET updated_at = ?1 WHERE id = ?2",
            rusqlite::params![updated_at, id],
        )
        .unwrap();
    }

    /// 设置历史起点时间戳（`None` 造 NULL：旧库没记下创建时间的形态）。
    fn set_created_at(&self, id: &str, created_at: Option<i64>) {
        let conn = Connection::open(self.paths.workbuddy_db()).unwrap();
        conn.execute(
            "UPDATE sessions SET created_at = ?1 WHERE id = ?2",
            rusqlite::params![created_at, id],
        )
        .unwrap();
    }

    fn created_at_of(&self, id: &str) -> Option<i64> {
        let conn = Connection::open(self.paths.workbuddy_db()).unwrap();
        conn.query_row(
            "SELECT created_at FROM sessions WHERE id = ?1",
            [id],
            |row| row.get::<_, Option<i64>>(0),
        )
        .ok()
        .flatten()
    }

    fn updated_at_of(&self, id: &str) -> Option<i64> {
        let conn = Connection::open(self.paths.workbuddy_db()).unwrap();
        conn.query_row(
            "SELECT updated_at FROM sessions WHERE id = ?1",
            [id],
            |row| row.get::<_, Option<i64>>(0),
        )
        .ok()
        .flatten()
    }

    fn status_of(&self, id: &str) -> Option<String> {
        let conn = Connection::open(self.paths.workbuddy_db()).unwrap();
        conn.query_row("SELECT status FROM sessions WHERE id = ?1", [id], |row| {
            row.get::<_, Option<String>>(0)
        })
        .ok()
        .flatten()
    }

    fn target(&self, uid: &str) -> Value {
        json!({"id": format!("acc-{uid}"), "uid": uid, "variant": "cn"})
    }

    fn store(&self) -> LinkStore {
        match session_link::load_store(&self.paths) {
            StoreState::Ready(store) => store,
            other => panic!("同步记录应为 Ready，实际 {other:?}"),
        }
    }

    fn body_files(&self) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(self.paths.projects_dir().join("ws-a"))
            .unwrap()
            .flatten()
            .filter_map(|entry| {
                let name = entry.file_name().to_string_lossy().to_string();
                name.ends_with(".jsonl").then_some(name)
            })
            .collect();
        names.sort();
        names
    }

    /// 基线目录里的 `*.json` 数量（判断恢复是否新增基线）。
    fn baseline_files(&self) -> usize {
        std::fs::read_dir(self.paths.baselines_dir())
            .map(|entries| {
                entries
                    .flatten()
                    .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "json"))
                    .count()
            })
            .unwrap_or(0)
    }

    /// 云端映射表行数（判断恢复是否重复登记）。
    fn mapping_rows(&self) -> usize {
        let conn = Connection::open(self.paths.edge_sync_db(WbVariant::Cn)).unwrap();
        conn.query_row("SELECT COUNT(*) FROM edge_sync_mapping", [], |row| {
            row.get::<_, i64>(0)
        })
        .unwrap() as usize
    }

    fn rows_for(&self, uid: &str) -> Vec<String> {
        let conn = Connection::open(self.paths.workbuddy_db()).unwrap();
        let mut stmt = conn
            .prepare("SELECT id FROM sessions WHERE user_id = ?1 AND deleted_at IS NULL")
            .unwrap();
        let mut rows: Vec<String> = stmt
            .query_map([uid], |row| row.get::<_, String>(0))
            .unwrap()
            .flatten()
            .collect();
        rows.sort();
        rows
    }

    fn first_copy_id(&self, report: &Value) -> String {
        report["copied"][0]["newId"].as_str().unwrap().to_string()
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn body_text(cid: &str) -> String {
    body_text_with(cid, "你好")
}

/// 带独特正文的会话：仅内容去重（复制/折叠）相关用例需要用它区分互不相同的会话，
/// 避免所有夹具都因「除自身 id 外逐字相同」被正确判成同一份内容。
fn body_text_with(cid: &str, marker: &str) -> String {
    format!(
        "{}\n{}\n",
        json!({"type": "user", "sessionId": cid, "text": marker}),
        json!({"type": "assistant", "sessionId": cid, "text": format!("{marker}-reply")})
    )
}

/// 一个可用的国内版环境：源账号 uid-a 有一个带正文的会话 sess-1。
fn ready_env(name: &str) -> Env {
    let env = Env::new(name);
    env.create_db();
    env.create_edge_db(WbVariant::Cn);
    env.set_login("uid-a");
    env.add_session("sess-1", "uid-a", "标题一");
    env.add_body("sess-1", &body_text("sess-1"));
    env
}

fn copy(env: &Env, target_uid: &str, ids: &[&str]) -> Value {
    let ids: Vec<String> = ids.iter().map(|id| id.to_string()).collect();
    copy_sessions_for_switch_at(
        &env.paths(),
        WbVariant::Cn,
        &env.target(target_uid),
        &ids,
        |_| false,
    )
    .unwrap()
}

// ---------------------------------------------------------------------------
// 基础路径与能力探测
// ---------------------------------------------------------------------------

/// A1 防回归：WorkBuddy 命名空间下三条关联路径必须与改造前**逐字相同**
/// （存量关联表 / 基线 / 锁都在这些名字上，改名即等于数据丢失）。
#[test]
fn workbuddy_link_paths_are_byte_identical_to_before() {
    let paths = SessionPaths::for_variant(WbVariant::Cn);
    assert!(paths.session_links_file().ends_with("session_links.json"));
    assert!(paths.session_links_dir().ends_with("session-links"));
    assert!(paths.baselines_dir().ends_with("session-links/baselines"));
    assert!(paths
        .preview_tokens_dir()
        .ends_with("session-links/previews"));
    assert!(paths.operations_dir().ends_with("session-links/operations"));
    assert!(paths
        .link_store_lock_file()
        .ends_with("locks/session-links.lock"));
    // 相对工具存储根逐段比对：写死分隔符会在 Windows 上反转断言方向。
    let root = std::env::temp_dir().join("wb-switch-store");
    let at_root = SessionPaths {
        store_root: root.clone(),
        data_root: PathBuf::new(),
        auth_file: PathBuf::new(),
        link_namespace: LinkNamespace::WorkBuddy,
    };
    assert_eq!(
        at_root.session_links_file(),
        root.join("session_links.json")
    );
    assert_eq!(at_root.session_links_dir(), root.join("session-links"));
    assert_eq!(
        at_root.baselines_dir(),
        root.join("session-links").join("baselines")
    );
    assert_eq!(
        at_root.link_store_lock_file(),
        root.join("locks").join("session-links.lock")
    );
}

/// 命名空间隔离：VS Code 侧的关联表 / 目录 / 锁与 WorkBuddy 名字不同，
/// 且两者的存储根相同（同一 `~/.wb-switch` 下并存而不互相污染）。
#[test]
fn vscode_link_paths_are_isolated_from_workbuddy() {
    let root = std::env::temp_dir().join("wb-switch-store");
    let workbuddy = SessionPaths {
        store_root: root.clone(),
        data_root: PathBuf::new(),
        auth_file: PathBuf::new(),
        link_namespace: LinkNamespace::WorkBuddy,
    };
    let vscode = SessionPaths::for_vscode_ext_at(root.clone());
    assert_eq!(vscode.store_root, workbuddy.store_root);
    assert_eq!(
        vscode.session_links_file(),
        root.join("vscode_session_links.json")
    );
    assert_eq!(
        vscode.session_links_dir(),
        root.join("vscode-session-links")
    );
    assert_eq!(
        vscode.baselines_dir(),
        root.join("vscode-session-links").join("baselines")
    );
    assert_eq!(
        vscode.link_store_lock_file(),
        root.join("locks").join("vscode-session-links.lock")
    );
    assert_eq!(
        vscode.preview_tokens_dir(),
        root.join("vscode-session-links").join("previews")
    );
    assert_eq!(
        vscode.operations_dir(),
        root.join("vscode-session-links").join("operations")
    );
    assert_ne!(vscode.session_links_file(), workbuddy.session_links_file());
    assert_ne!(vscode.session_links_dir(), workbuddy.session_links_dir());
    assert_ne!(vscode.baselines_dir(), workbuddy.baselines_dir());
    assert_ne!(vscode.preview_tokens_dir(), workbuddy.preview_tokens_dir());
    assert_ne!(vscode.operations_dir(), workbuddy.operations_dir());
    assert_ne!(
        vscode.link_store_lock_file(),
        workbuddy.link_store_lock_file()
    );
    // 默认命名空间是 WorkBuddy：`SessionPaths::for_variant` 之外的历史构造点
    // 不会因为新增字段而漂移到 VS Code 名字上。
    assert_eq!(LinkNamespace::default(), LinkNamespace::WorkBuddy);
}

/// 命名空间隔离：CodeBuddy IDE 侧的关联表 / 目录 / 锁与 WorkBuddy、VS Code 都不同，
/// 三个目标在同一 `~/.wb-switch` 下并存而不互相污染。
#[test]
fn codebuddy_ide_link_paths_are_isolated_from_other_namespaces() {
    let root = std::env::temp_dir().join("wb-switch-store");
    let workbuddy = SessionPaths {
        store_root: root.clone(),
        data_root: PathBuf::new(),
        auth_file: PathBuf::new(),
        link_namespace: LinkNamespace::WorkBuddy,
    };
    let vscode = SessionPaths::for_vscode_ext_at(root.clone());
    let ide = SessionPaths::for_codebuddy_ide_at(root.clone());
    assert_eq!(ide.store_root, workbuddy.store_root);
    assert_eq!(
        ide.session_links_file(),
        root.join("codebuddy_ide_session_links.json")
    );
    assert_eq!(
        ide.session_links_dir(),
        root.join("codebuddy-ide-session-links")
    );
    assert_eq!(
        ide.baselines_dir(),
        root.join("codebuddy-ide-session-links").join("baselines")
    );
    assert_eq!(
        ide.link_store_lock_file(),
        root.join("locks").join("codebuddy-ide-session-links.lock")
    );
    assert_eq!(
        ide.preview_tokens_dir(),
        root.join("codebuddy-ide-session-links").join("previews")
    );
    assert_eq!(
        ide.operations_dir(),
        root.join("codebuddy-ide-session-links").join("operations")
    );
    for other in [&workbuddy, &vscode] {
        assert_ne!(ide.session_links_file(), other.session_links_file());
        assert_ne!(ide.session_links_dir(), other.session_links_dir());
        assert_ne!(ide.baselines_dir(), other.baselines_dir());
        assert_ne!(ide.preview_tokens_dir(), other.preview_tokens_dir());
        assert_ne!(ide.operations_dir(), other.operations_dir());
        assert_ne!(ide.link_store_lock_file(), other.link_store_lock_file());
    }
}

#[test]
fn db_paths_follow_variant_data_root() {
    let cn = SessionPaths::for_variant(WbVariant::Cn);
    // Path::ends_with 按路径分量比较，Windows 上 `\` 与 `/` 等价；
    // 不要用 to_string_lossy().ends_with()——那会把分隔符写进断言。
    assert!(cn.workbuddy_db().ends_with(".workbuddy/workbuddy.db"));
    // 映射库文件名交给解析器：真实数据根可能是任意版本（WorkBuddy 5.6 已迁移到
    // v4 且 v2/v3 残留并存），这里只断言落在国内版数据根下的 edge-sync-mapping-*.db，
    // 具体发现规则由 edge_sync_db_picks_largest_discovered_version 用临时目录覆盖。
    let cn_edge = cn.edge_sync_db(WbVariant::Cn);
    assert_eq!(cn_edge.parent(), Some(cn.data_root.as_path()));
    assert!(cn_edge.to_string_lossy().contains("edge-sync-mapping-"));

    let ai = SessionPaths::for_variant(WbVariant::Ai);
    assert_eq!(ai.workbuddy_db().parent(), Some(ai.data_root.as_path()));
    assert_ne!(cn.workbuddy_db(), ai.workbuddy_db());
    // 国际版数据根与国内版不同构，默认文件名为 v4，且同样走动态发现。
    assert!(ai
        .edge_sync_db(WbVariant::Ai)
        .to_string_lossy()
        .ends_with("edge-sync-mapping-v4.db"));
    assert_ne!(
        cn.edge_sync_db(WbVariant::Cn),
        ai.edge_sync_db(WbVariant::Ai)
    );
    // 锁与关联存储都挂在工具存储根下，顺序固定为「档位锁 → 存储锁」。
    assert!(cn
        .variant_ops_lock_file(WbVariant::Cn)
        .ends_with("locks/session-ops-cn.lock"));
    assert_ne!(
        cn.variant_ops_lock_file(WbVariant::Cn),
        cn.variant_ops_lock_file(WbVariant::Ai)
    );
    assert!(cn
        .link_store_lock_file()
        .ends_with("locks/session-links.lock"));
}

/// 在临时数据根里放好给定文件，返回国内版（或指定档位）解析出的映射库文件名。
fn edge_sync_pick(variant: WbVariant, files: &[&str]) -> String {
    let root = temp_root("edge_sync_pick");
    std::fs::create_dir_all(&root).unwrap();
    for name in files {
        std::fs::write(root.join(name), b"stub").unwrap();
    }
    let paths = SessionPaths {
        data_root: root.clone(),
        ..SessionPaths::for_variant(variant)
    };
    let picked = paths
        .edge_sync_db(variant)
        .file_name()
        .unwrap()
        .to_string_lossy()
        .to_string();
    std::fs::remove_dir_all(&root).unwrap();
    picked
}

#[test]
fn edge_sync_db_picks_largest_discovered_version() {
    // 回归：WorkBuddy 客户端自行演进映射库文件名（本机实测 v2 迁移残留、v3、v4
    // 并存，v3 从未出现在本工具代码里）。路径必须动态发现最大版本号，写死任何
    // 名字都会再次失效——写死 v2 会把登记写进迁移残留库，云端归属随之丢失。
    // 六种组合：空 / 仅无后缀 / 仅 v2 / v2+v4 / v2+v3+v4 / 仅 v4。
    let cn = WbVariant::Cn;
    assert_eq!(edge_sync_pick(cn, &[]), "edge-sync-mapping-v2.db");
    assert_eq!(
        edge_sync_pick(cn, &["edge-sync-mapping.db"]),
        "edge-sync-mapping.db"
    );
    assert_eq!(
        edge_sync_pick(cn, &["edge-sync-mapping-v0.db"]),
        "edge-sync-mapping-v0.db"
    );
    assert_eq!(
        edge_sync_pick(cn, &["edge-sync-mapping-v2.db"]),
        "edge-sync-mapping-v2.db"
    );
    assert_eq!(
        edge_sync_pick(cn, &["edge-sync-mapping-v2.db", "edge-sync-mapping-v4.db"]),
        "edge-sync-mapping-v4.db"
    );
    assert_eq!(
        edge_sync_pick(
            cn,
            &[
                "edge-sync-mapping-v2.db",
                "edge-sync-mapping-v3.db",
                "edge-sync-mapping-v4.db",
            ]
        ),
        "edge-sync-mapping-v4.db"
    );
    assert_eq!(
        edge_sync_pick(cn, &["edge-sync-mapping-v4.db"]),
        "edge-sync-mapping-v4.db"
    );

    // 伴生文件（-shm / -wal）不是候选；更高版本出现时自动适配。
    assert_eq!(
        edge_sync_pick(
            cn,
            &[
                "edge-sync-mapping-v4.db",
                "edge-sync-mapping-v4.db-shm",
                "edge-sync-mapping-v4.db-wal",
            ]
        ),
        "edge-sync-mapping-v4.db"
    );
    assert_eq!(
        edge_sync_pick(cn, &["edge-sync-mapping-v4.db", "edge-sync-mapping-v5.db"]),
        "edge-sync-mapping-v5.db"
    );

    // 版本号只接受十进制数字：前导零仍按数值比较；非数字、溢出、大小写差异和
    // 额外前后缀都静默忽略，避免把相似但非 WorkBuddy 文件误当成映射库。
    assert_eq!(
        edge_sync_pick(cn, &["edge-sync-mapping-v01.db"]),
        "edge-sync-mapping-v01.db"
    );
    assert_eq!(
        edge_sync_pick(cn, &["edge-sync-mapping-v01.db", "edge-sync-mapping-v1.db"]),
        "edge-sync-mapping-v1.db"
    );
    assert_eq!(
        edge_sync_pick(cn, &["edge-sync-mapping-v+9.db"]),
        "edge-sync-mapping-v2.db"
    );
    assert_eq!(
        edge_sync_pick(cn, &["edge-sync-mapping-v18446744073709551616.db"]),
        "edge-sync-mapping-v2.db"
    );
    assert_eq!(
        edge_sync_pick(
            cn,
            &[
                "edge-sync-mapping-vx.db",
                "Edge-sync-mapping-v9.db",
                "prefix-edge-sync-mapping-v9.db",
                "edge-sync-mapping-v9.db.bak",
                "edge-sync-mapping-v9-extra.db",
            ]
        ),
        "edge-sync-mapping-v2.db"
    );

    // 国际版走同一套发现逻辑（不再写死 v4）。
    assert_eq!(
        edge_sync_pick(WbVariant::Ai, &[]),
        "edge-sync-mapping-v4.db"
    );
    assert_eq!(
        edge_sync_pick(
            WbVariant::Ai,
            &["edge-sync-mapping-v4.db", "edge-sync-mapping-v6.db"]
        ),
        "edge-sync-mapping-v6.db"
    );

    // 目录不存在：安全回落到默认文件名，不 panic（调用方据此报「云端映射库不存在」）。
    let missing = temp_root("edge_sync_missing");
    assert_eq!(
        edge_sync_db_path(&missing, cn),
        missing.join("edge-sync-mapping-v2.db")
    );
    assert_eq!(
        edge_sync_db_path(&missing, WbVariant::Ai),
        missing.join("edge-sync-mapping-v4.db")
    );
}

fn temp_root(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "wb_switch_session_root_{}_{name}",
        uuid::Uuid::new_v4().simple()
    ))
}

fn create_sessions_db(path: &Path) {
    let conn = Connection::open(path).unwrap();
    conn.execute_batch("CREATE TABLE sessions (id TEXT PRIMARY KEY, user_id TEXT, title TEXT);")
        .unwrap();
}

/// 能力探测：`projects/` 目录 + `workbuddy.db` 的 `sessions` 表同时存在才可用。
#[test]
fn session_copy_capability_probe() {
    let bare = temp_root("bare");
    std::fs::create_dir_all(&bare).unwrap();
    assert!(!session_copy_supported_at(&bare));

    let only_projects = temp_root("only-projects");
    std::fs::create_dir_all(only_projects.join("projects")).unwrap();
    assert!(!session_copy_supported_at(&only_projects));

    let empty_db = temp_root("empty-db");
    std::fs::create_dir_all(&empty_db).unwrap();
    let conn = Connection::open(empty_db.join("workbuddy.db")).unwrap();
    conn.execute_batch("CREATE TABLE other (x INTEGER);")
        .unwrap();
    drop(conn);
    assert!(!session_copy_supported_at(&empty_db));

    let db_only = temp_root("db-only");
    std::fs::create_dir_all(&db_only).unwrap();
    create_sessions_db(&db_only.join("workbuddy.db"));
    assert!(!session_copy_supported_at(&db_only));

    let ready = temp_root("ready");
    std::fs::create_dir_all(ready.join("projects")).unwrap();
    create_sessions_db(&ready.join("workbuddy.db"));
    assert!(session_copy_supported_at(&ready));

    for dir in [bare, only_projects, empty_db, db_only, ready] {
        let _ = std::fs::remove_dir_all(dir);
    }
}

/// 能力不满足时返回明确错误，且不写任何文件。
#[test]
fn copy_sessions_for_switch_rejects_unsupported_root() {
    let env = Env::new("unsupported");
    let bare = Env {
        root: env.root.clone(),
        paths: SessionPaths {
            store_root: env.root.join("bare-store"),
            data_root: env.root.join("bare-data"),
            auth_file: env.root.join("auth.info"),
            link_namespace: LinkNamespace::WorkBuddy,
        },
    };
    std::fs::create_dir_all(bare.paths.data_root.clone()).unwrap();
    let err = copy_sessions_for_switch_at(
        &bare.paths(),
        WbVariant::Ai,
        &json!({"id": "ai-1", "uid": "u-ai", "variant": "ai"}),
        &["cid-1".to_string()],
        |_| false,
    )
    .expect_err("不支持的档位必须返回错误");
    assert!(err.contains(SESSION_COPY_UNSUPPORTED), "{err}");
    assert_eq!(std::fs::read_dir(&bare.paths.data_root).unwrap().count(), 0);
}

/// 能力探测只对国际版生效：国内版在探测不通过的数据根上仍走改造前的路径。
#[test]
fn session_copy_probe_only_gates_ai() {
    let env = Env::new("cn-no-probe");
    let bare = SessionPaths {
        store_root: env.root.join("bare-store"),
        data_root: env.root.join("bare-data"),
        auth_file: env.root.join("auth.info"),
        link_namespace: LinkNamespace::WorkBuddy,
    };
    std::fs::create_dir_all(&bare.data_root).unwrap();

    let cn_err = copy_sessions_for_switch_at(
        &bare,
        WbVariant::Cn,
        &json!({"id": "cn-1", "variant": "cn", "uid": "   "}),
        &["cid-1".to_string()],
        |_| false,
    )
    .expect_err("缺 uid 仍必须返回错误");
    assert_eq!(cn_err, "目标账号缺少 uid，无法复制会话");
    assert!(
        !cn_err.contains(SESSION_COPY_UNSUPPORTED),
        "国内版不得被能力探测拦截: {cn_err}"
    );

    let ai_err = copy_sessions_for_switch_at(
        &bare,
        WbVariant::Ai,
        &json!({"id": "ai-1", "uid": "u-ai", "variant": "ai"}),
        &["cid-1".to_string()],
        |_| false,
    )
    .expect_err("国际版不满足能力探测必须返回错误");
    assert!(ai_err.contains(SESSION_COPY_UNSUPPORTED), "{ai_err}");
}

/// 能力可用时继续走 uid 校验（证明探测不会误短路）。
#[test]
fn copy_sessions_for_switch_requires_target_uid() {
    let env = ready_env("requires-uid");
    let err = copy_sessions_for_switch_at(
        &env.paths(),
        WbVariant::Cn,
        &json!({"id": "a-1", "variant": "cn", "uid": "   "}),
        &["sess-1".to_string()],
        |_| false,
    )
    .expect_err("缺 uid 必须返回错误");
    assert_eq!(err, "目标账号缺少 uid，无法复制会话");
}

/// 未登录 / 目标即当前账号：拒绝且不写任何东西。
#[test]
fn copy_rejects_missing_login_and_same_account() {
    let env = ready_env("login-checks");
    std::fs::remove_file(&env.paths.auth_file).unwrap();
    let err = copy_sessions_for_switch_at(
        &env.paths(),
        WbVariant::Cn,
        &env.target("uid-b"),
        &["sess-1".to_string()],
        |_| false,
    )
    .expect_err("缺登录态必须报错");
    assert!(err.contains("未读取到本机登录态"), "{err}");

    env.set_login("uid-b");
    let err = copy_sessions_for_switch_at(
        &env.paths(),
        WbVariant::Cn,
        &env.target("uid-b"),
        &["sess-1".to_string()],
        |_| false,
    )
    .expect_err("同一账号必须报错");
    assert!(err.contains("当前账号与目标账号相同"), "{err}");
    assert_eq!(env.rows_for("uid-b").len(), 0);
}

// ---------------------------------------------------------------------------
// 幂等复制与关联组（R1）
// ---------------------------------------------------------------------------

#[test]
fn copy_writes_body_row_and_link_group() {
    let env = ready_env("happy");
    let report = copy(&env, "uid-b", &["sess-1"]);

    assert_eq!(report["sourceUid"], "uid-a");
    assert_eq!(report["targetUid"], "uid-b");
    assert_eq!(report["copied"].as_array().unwrap().len(), 1);
    assert_eq!(report["alreadyLinked"].as_array().unwrap().len(), 0);
    assert!(report.get("errors").is_none());
    assert!(report.get("needsRecovery").is_none());

    let new_id = env.first_copy_id(&report);
    assert_ne!(new_id, "sess-1");
    assert_eq!(report["copied"][0]["id"], "sess-1");

    // 正文：新 id 文件存在、旧 id 引用已替换，源文件不动。
    let copied_body = std::fs::read_to_string(env.body_path(&new_id)).unwrap();
    assert!(copied_body.contains(&new_id));
    assert!(!copied_body.contains("sess-1"));
    assert_eq!(
        std::fs::read_to_string(env.body_path("sess-1")).unwrap(),
        body_text("sess-1")
    );

    // 数据库行归属目标账号。
    assert_eq!(env.rows_for("uid-b"), vec![new_id.clone()]);
    assert_eq!(env.rows_for("uid-a"), vec!["sess-1".to_string()]);

    // 不预写云端映射：登记交接给客户端（edge-sync 扩展迁移时自行写入）。
    assert_eq!(env.mapping_rows(), 0, "复制不得预写映射行");

    // 关联组：同一逻辑会话、两个账号各一个 active 成员、一对基线。
    let store = env.store();
    assert!(
        store.revision >= 1,
        "首次落地空存储 + 关联提交都会推进 revision"
    );
    assert_eq!(store.groups.len(), 1);
    let group = &store.groups[0];
    assert_eq!(group.variant, WbVariant::Cn);
    assert_eq!(group.members.len(), 2);
    assert!(group
        .members
        .iter()
        .all(|member| member.state == MemberState::Active));
    assert_eq!(group.pair_bases.len(), 1);
    assert!(env
        .paths
        .baselines_dir()
        .join(format!("{}.json", group.pair_bases[0].baseline_ref))
        .exists());
    // 来源账号的成员带上了 accountId（账号库缺失时为 None，不影响身份判定）。
    assert!(group
        .members
        .iter()
        .any(|member| member.uid == "uid-a" && member.session_id == "sess-1"));

    // 成功清理：临时备份已回收，报告不展示可还原路径，也没有维护记录残留。
    assert!(report["copied"][0]["backup"].is_null(), "{report}");
    assert_eq!(report["copied"][0]["cleanupState"], "cleaned", "{report}");
    assert_eq!(report["temporaryFiles"], json!([]), "{report}");
    let operation_id = session_link::scan_operations(&env.paths)
        .operations
        .first()
        .expect("操作日志必须保留")
        .operation_id
        .clone();
    assert!(
        !session_backup::transaction_dir(&env.paths, WbVariant::Cn, &operation_id)
            .unwrap()
            .exists(),
        "成功路径必须回收本次操作专属目录"
    );
    assert!(session_backup::scan_lifecycle(&env.paths)
        .records
        .is_empty());
}

/// 复制保留源 `created_at`（会话历史起点不伪造），`updated_at` 仍写当前时间。
///
/// `updated_at` 驱动客户端「最近更新」排序：副本沿用源的旧值会直接沉底，而副本
/// 此刻确实刚被创建，写当前时间才符合事实。
#[test]
fn copy_preserves_source_created_at_and_refreshes_updated_at() {
    let env = ready_env("copy-created-at");
    // 源行由 add_session 写成 created_at=1000 / updated_at=2000。
    let report = copy(&env, "uid-b", &["sess-1"]);
    let new_id = env.first_copy_id(&report);

    assert_eq!(env.created_at_of("sess-1"), Some(1000), "源行 created_at");
    assert_eq!(
        env.created_at_of(&new_id),
        Some(1000),
        "副本必须沿用源的 created_at"
    );
    let copied_updated_at = env.updated_at_of(&new_id).expect("副本必须有 updated_at");
    assert!(
        copied_updated_at > 2000,
        "副本 updated_at 必须是本次时间而不是源的旧值：{copied_updated_at}"
    );
    assert_eq!(
        env.updated_at_of("sess-1"),
        Some(2000),
        "源行时间戳不得被复制改动"
    );
}

/// 源行 `created_at` 为 NULL（旧库没记）时回退当前时间：NULL 下传会破坏客户端排序。
#[test]
fn copy_falls_back_to_now_when_source_created_at_is_null() {
    let env = ready_env("copy-created-at-null");
    env.add_session("sess-2", "uid-a", "标题二");
    env.add_body("sess-2", &body_text("sess-2"));
    env.set_created_at("sess-2", None);
    assert_eq!(
        env.created_at_of("sess-2"),
        None,
        "源行 created_at 必须是 NULL"
    );

    let before = now_ms();
    let report = copy(&env, "uid-b", &["sess-2"]);
    let new_id = env.first_copy_id(&report);
    let after = now_ms();

    let copied_created_at = env
        .created_at_of(&new_id)
        .expect("源值为 NULL 时必须回退为具体时间，不得下传 NULL");
    assert!(
        (before..=after).contains(&copied_created_at),
        "回退值必须是本次时间：{copied_created_at} 不在 [{before}, {after}]"
    );
}

/// 批量复制逐项清理：进入下一项之前，上一成功项的临时目录已经消失（不累积备份）。
#[test]
fn batch_copy_cleans_each_backup_before_next_item() {
    let env = ready_env("batch-cleanup");
    env.add_session("sess-2", "uid-a", "标题二");
    env.add_body("sess-2", &body_text_with("sess-2", "第二个会话独有内容"));

    let report = copy(&env, "uid-b", &["sess-1", "sess-2"]);
    assert_eq!(report["copied"].as_array().unwrap().len(), 2, "{report}");
    for item in report["copied"].as_array().unwrap() {
        assert!(item["backup"].is_null(), "{report}");
        assert_eq!(item["cleanupState"], "cleaned", "{report}");
    }
    // 两个成功项都不留目录与维护记录：连续操作不累积成功备份。
    let transactions = env
        .paths
        .backup_root()
        .join(session_backup::TRANSACTIONS_DIR_NAME)
        .join(WbVariant::Cn.as_str());
    let leftovers = std::fs::read_dir(&transactions)
        .map(|entries| entries.flatten().count())
        .unwrap_or(0);
    assert_eq!(leftovers, 0, "成功批次不得累积操作专属目录");
    assert!(session_backup::scan_lifecycle(&env.paths)
        .records
        .is_empty());
    assert_eq!(
        env.body_files().len(),
        4,
        "两条来源内容与两个复制后的内容都在"
    );
}

/// 归属不可验证（临时目录路径被替换为符号链接）：业务成功保持不变、材料保留并上报，
/// 解除异常后由维护入口补清理。
#[cfg(unix)]
#[test]
fn cleanup_protects_business_success_when_transaction_path_is_symlinked() {
    let env = ready_env("cleanup-symlink");
    let transactions = env
        .paths
        .backup_root()
        .join(session_backup::TRANSACTIONS_DIR_NAME);
    std::fs::create_dir_all(&transactions).unwrap();
    let target = env.root.join("redirected-transactions");
    std::fs::create_dir_all(&target).unwrap();
    // 档位目录被替换为符号链接：删除前校验必须拒绝，且不得跟随链接删除。
    std::os::unix::fs::symlink(&target, transactions.join(WbVariant::Cn.as_str())).unwrap();

    let report = copy(&env, "uid-b", &["sess-1"]);
    assert_eq!(report["copied"].as_array().unwrap().len(), 1, "{report}");
    let new_id = env.first_copy_id(&report);
    // 业务成功不变：正文与数据库行都在，报告仍报成功，只是临时文件待处理。
    assert!(env.body_path(&new_id).exists());
    assert_eq!(env.rows_for("uid-b"), vec![new_id.clone()]);
    assert_eq!(report["copied"][0]["cleanupState"], "pending", "{report}");
    assert!(
        report["copied"][0]["backup"].as_str().is_some(),
        "待清理必须保留位置：{report}"
    );
    assert!(
        report["copied"][0]["cleanupError"]
            .as_str()
            .unwrap()
            .contains("符号链接"),
        "{report}"
    );
    let temporary_files = report["temporaryFiles"].as_array().expect("temporaryFiles");
    assert!(
        !temporary_files.is_empty(),
        "本轮清理受阻必须出现在报告级 temporaryFiles：{report}"
    );
    assert!(
        temporary_files.iter().any(|item| item["reason"]
            .as_str()
            .is_some_and(|reason| reason.contains("符号链接"))),
        "{report}"
    );
    assert_eq!(session_backup::scan_lifecycle(&env.paths).records.len(), 1);

    // 解除异常后：维护入口补清理，业务结果不变；链接目标不被当作本操作材料删除。
    std::fs::remove_file(transactions.join(WbVariant::Cn.as_str())).unwrap();
    let recovery = recover_pending_session_operations_at(&env.paths, WbVariant::Cn);
    assert!(recovery.is_clean(), "{:?}", recovery.needs_recovery);
    assert!(session_backup::scan_lifecycle(&env.paths)
        .records
        .is_empty());
    assert!(env.body_path(&new_id).exists());
    assert_eq!(
        session_link::scan_operations(&env.paths).operations[0]
            .cleanup_state
            .as_deref(),
        Some(session_backup::CLEANUP_STATE_CLEANED)
    );
}

#[test]
fn copy_retry_reuses_member_without_second_copy() {
    let env = ready_env("retry");
    let first = copy(&env, "uid-b", &["sess-1"]);
    let new_id = env.first_copy_id(&first);
    let revision_before_retry = env.store().revision;

    let second = copy(&env, "uid-b", &["sess-1"]);
    assert_eq!(second["copied"].as_array().unwrap().len(), 0);
    assert_eq!(second["alreadyLinked"].as_array().unwrap().len(), 1);
    assert_eq!(second["alreadyLinked"][0]["sessionId"], new_id);
    assert!(second.get("errors").is_none());

    assert_eq!(env.body_files().len(), 2, "重试不得产生第二个副本");
    assert_eq!(env.rows_for("uid-b").len(), 1);
    assert_eq!(
        env.store().revision,
        revision_before_retry,
        "幂等复用不写同步记录"
    );
    assert_eq!(env.store().groups[0].members.len(), 2);
}

#[test]
fn copy_back_reuses_original_session() {
    let env = ready_env("back");
    let first = copy(&env, "uid-b", &["sess-1"]);
    let new_id = env.first_copy_id(&first);

    // 目标账号成为当前账号，把副本复制回原账号：必须复用原件。
    env.set_login("uid-b");
    let back = copy(&env, "uid-a", &[&new_id]);
    assert_eq!(back["copied"].as_array().unwrap().len(), 0);
    assert_eq!(back["alreadyLinked"].as_array().unwrap().len(), 1);
    assert_eq!(back["alreadyLinked"][0]["sessionId"], "sess-1");

    assert_eq!(env.body_files().len(), 2, "B→A 不得新建副本");
    assert_eq!(env.rows_for("uid-a"), vec!["sess-1".to_string()]);
    assert_eq!(env.store().groups[0].members.len(), 2);
}

#[test]
fn chain_a_to_b_then_a_to_c_then_b_to_c_reuses_existing_copy() {
    let env = ready_env("chain");
    let to_b = copy(&env, "uid-b", &["sess-1"]);
    let b_id = env.first_copy_id(&to_b);

    // 同一来源再复制给 C：同组内新增一个成员，不是新组。
    let to_c = copy(&env, "uid-c", &["sess-1"]);
    let c_id = env.first_copy_id(&to_c);
    assert_eq!(env.store().groups.len(), 1);
    assert_eq!(env.store().groups[0].members.len(), 3);

    // B→C：组内已有 C 的有效副本，复用而不重复复制。
    env.set_login("uid-b");
    let b_to_c = copy(&env, "uid-c", &[&b_id]);
    assert_eq!(b_to_c["copied"].as_array().unwrap().len(), 0);
    assert_eq!(b_to_c["alreadyLinked"][0]["sessionId"], c_id);
    assert_eq!(env.body_files().len(), 3);

    // 配对基线按成员对保存：A/B、A/C 各有基线；C 加入时按 A/B 基线继承出 B/C。
    let group = &env.store().groups[0];
    assert_eq!(group.pair_bases.len(), 3);
    let member_id = |uid: &str| {
        group
            .members
            .iter()
            .find(|member| member.uid == uid)
            .unwrap()
            .member_id
            .clone()
    };
    let (a, b, c) = (member_id("uid-a"), member_id("uid-b"), member_id("uid-c"));
    let pair = |left: &str, right: &str| {
        session_link::find_pair_base(group, left, right)
            .unwrap_or_else(|| panic!("缺少成员对基线 {left}/{right}"))
            .baseline_ref
            .clone()
    };
    assert_eq!(pair(&a, &b), pair(&b, &c), "B/C 继承自 A/B 的共同基线");
    assert_ne!(pair(&a, &c), pair(&a, &b), "A/C 是各自新建的基线");
}

#[test]
fn rename_does_not_break_link() {
    let env = ready_env("rename");
    let first = copy(&env, "uid-b", &["sess-1"]);
    let new_id = env.first_copy_id(&first);

    // 用户在目标账号改名：关联不依赖标题。
    let conn = Connection::open(env.paths.workbuddy_db()).unwrap();
    conn.execute(
        "UPDATE sessions SET title = '改过的标题', custom_title = '自定义名' WHERE id = ?1",
        [&new_id],
    )
    .unwrap();
    drop(conn);

    let again = copy(&env, "uid-b", &["sess-1"]);
    assert_eq!(again["alreadyLinked"][0]["sessionId"], new_id);
    assert_eq!(env.body_files().len(), 2);
    assert_eq!(env.store().groups[0].members.len(), 2);
}

#[test]
fn same_title_independent_sessions_stay_separate() {
    let env = ready_env("same-title");
    env.add_session("sess-2", "uid-a", "标题一");
    env.add_body("sess-2", &body_text_with("sess-2", "同标题但正文不同"));

    let report = copy(&env, "uid-b", &["sess-1", "sess-2"]);
    assert_eq!(report["copied"].as_array().unwrap().len(), 2);
    let ids: Vec<String> = report["copied"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["newId"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(ids.len(), 2);
    assert_ne!(ids[0], ids[1]);

    // 同标题不建立关联：第二个会话各成一个组，且各自独立幂等。
    assert_eq!(env.store().groups.len(), 2);
    assert_eq!(env.rows_for("uid-b").len(), 2);
    assert_eq!(env.body_files().len(), 4);

    let again = copy(&env, "uid-b", &["sess-1", "sess-2"]);
    assert_eq!(again["alreadyLinked"].as_array().unwrap().len(), 2);
    assert_eq!(env.body_files().len(), 4);
}

#[test]
fn invalid_target_member_is_superseded_and_rebuilt_without_resurrection() {
    let env = ready_env("rebuild");
    let first = copy(&env, "uid-b", &["sess-1"]);
    let old_id = env.first_copy_id(&first);

    // 目标副本的正文丢失 → 旧成员失效，重建新成员。
    std::fs::remove_file(env.body_path(&old_id)).unwrap();
    let rebuilt = copy(&env, "uid-b", &["sess-1"]);
    assert_eq!(rebuilt["copied"].as_array().unwrap().len(), 1);
    let new_id = env.first_copy_id(&rebuilt);
    assert_ne!(new_id, old_id);
    assert_eq!(
        env.body_files().len(),
        2,
        "旧复制后的内容已删，只剩源与新建副本"
    );

    let group = &env.store().groups[0];
    assert_eq!(group.members.len(), 3);
    let actives: Vec<&str> = group
        .members
        .iter()
        .filter(|member| member.uid == "uid-b" && member.state == MemberState::Active)
        .map(|member| member.session_id.as_str())
        .collect();
    assert_eq!(actives, vec![new_id.as_str()], "每账号唯一 active");
    assert_eq!(
        group
            .members
            .iter()
            .find(|member| member.session_id == old_id)
            .unwrap()
            .state,
        MemberState::Superseded
    );

    // 旧正文恢复后不得自动争夺有效位置：仍是重建后的成员有效。
    env.add_body(&old_id, &body_text(&old_id));
    let after_restore = copy(&env, "uid-b", &["sess-1"]);
    assert_eq!(after_restore["alreadyLinked"][0]["sessionId"], new_id);
    assert_eq!(env.store().groups[0].members.len(), 3, "不新增成员");
}

#[test]
fn identity_uses_uid_and_never_rebinds_by_account_id() {
    let env = ready_env("identity");

    // 手工写入一个组：目标成员 uid=uid-b，accountId 是旧账号 id（重新导入前的 id）。
    let paths = env.paths();
    session_link::with_link_store_write(&paths, |store| {
        store.groups.push(LinkGroup {
            id: "g-1".to_string(),
            variant: WbVariant::Cn,
            created_at: 1,
            members: vec![
                LinkMember {
                    member_id: "m-a".to_string(),
                    account_id: Some("acc-uid-a".to_string()),
                    uid: "uid-a".to_string(),
                    session_id: "sess-1".to_string(),
                    variant: None,
                    state: MemberState::Active,
                    linked_at: 1,
                    last_synced_at: None,
                },
                LinkMember {
                    member_id: "m-b".to_string(),
                    account_id: Some("old-account-id".to_string()),
                    uid: "uid-b".to_string(),
                    session_id: "sess-b".to_string(),
                    variant: None,
                    state: MemberState::Active,
                    linked_at: 1,
                    last_synced_at: None,
                },
            ],
            pair_bases: Vec::new(),
        });
        Ok(())
    })
    .unwrap();
    env.add_body("sess-b", &body_text("sess-b"));
    let conn = Connection::open(env.paths.workbuddy_db()).unwrap();
    conn.execute(
            "INSERT INTO sessions (id, user_id, title, cwd, created_at, updated_at, deleted_at, is_playground)
             VALUES ('sess-b', 'uid-b', '旧副本', '/ws/a', 1, 2, NULL, 0)",
            [],
        )
        .unwrap();
    drop(conn);

    // uid 相同、accountId 变了 → 仍复用（身份以 uid 为准）。
    let reused = copy(&env, "uid-b", &["sess-1"]);
    assert_eq!(reused["alreadyLinked"][0]["sessionId"], "sess-b");
    assert_eq!(env.body_files().len(), 2);

    // accountId 相同但 uid 不同 → 不得错误绑定，必须新建。
    let conn = Connection::open(env.paths.workbuddy_db()).unwrap();
    conn.execute(
        "UPDATE sessions SET user_id = 'uid-x' WHERE id = 'sess-b'",
        [],
    )
    .unwrap();
    drop(conn);
    let fresh = copy(&env, "uid-b", &["sess-1"]);
    assert_eq!(fresh["copied"].as_array().unwrap().len(), 1);
    assert_eq!(env.store().groups[0].members.len(), 3);
}

#[test]
fn variant_isolation_keeps_groups_separate() {
    let env = ready_env("variants");
    let cn = copy(&env, "uid-b", &["sess-1"]);
    assert_eq!(cn["copied"].as_str(), None);
    assert_eq!(cn["copied"].as_array().unwrap().len(), 1);

    // 国际版数据根：单独一套数据（同 store 根），身份字符串相同但档位不同。
    let mut ai_paths = env.paths();
    ai_paths.data_root = env.root.join("data-ai");
    std::fs::create_dir_all(ai_paths.projects_dir().join("ws-a")).unwrap();
    let conn = Connection::open(ai_paths.workbuddy_db()).unwrap();
    conn.execute_batch(
            "CREATE TABLE sessions (id TEXT PRIMARY KEY, user_id TEXT NOT NULL, title TEXT, custom_title TEXT, cwd TEXT, created_at INTEGER, updated_at INTEGER, deleted_at INTEGER, is_playground INTEGER);",
        )
        .unwrap();
    conn.execute(
            "INSERT INTO sessions (id, user_id, title, cwd, created_at, updated_at, deleted_at, is_playground)
             VALUES ('ai-sess-1', 'uid-a', 'AI 会话', '/ws/a', 1, 2, NULL, 0)",
            [],
        )
        .unwrap();
    drop(conn);
    std::fs::write(
        ai_paths.projects_dir().join("ws-a").join("ai-sess-1.jsonl"),
        body_text("ai-sess-1"),
    )
    .unwrap();
    let conn = Connection::open(ai_paths.edge_sync_db(WbVariant::Ai)).unwrap();
    conn.execute_batch(
            "CREATE TABLE edge_sync_mapping (session_id TEXT, conversation_id TEXT, msg_channel TEXT, created_at INTEGER);",
        )
        .unwrap();
    drop(conn);

    let ai_report = copy_sessions_for_switch_at(
        &ai_paths,
        WbVariant::Ai,
        &json!({"id": "acc-uid-b", "uid": "uid-b", "variant": "ai"}),
        &["ai-sess-1".to_string()],
        |_| false,
    )
    .unwrap();
    assert_eq!(
        ai_report["copied"].as_array().unwrap().len(),
        1,
        "同档位身份不同，必须新复制"
    );

    let store = env.store();
    assert_eq!(store.groups.len(), 2);
    let variants: Vec<&str> = store
        .groups
        .iter()
        .map(|group| group.variant.as_str())
        .collect();
    assert!(
        variants.contains(&"cn") && variants.contains(&"ai"),
        "{variants:?}"
    );
    // 两档位不串数据：成员会话 id 不相交，国际版组里带着国际版来源会话。
    let mut seen = std::collections::HashSet::new();
    for group in &store.groups {
        for member in &group.members {
            assert!(
                seen.insert(member.session_id.clone()),
                "会话 {} 同时出现在两个档位的组里",
                member.session_id
            );
        }
    }
    let ai_group = store
        .groups
        .iter()
        .find(|group| group.variant == WbVariant::Ai)
        .unwrap();
    assert!(ai_group
        .members
        .iter()
        .any(|member| member.session_id == "ai-sess-1"));
}

/// 报告契约：copied / alreadyLinked / errors 同时出现时字段完整（桌面与 webui 同形）。
#[test]
fn copy_report_carries_copied_already_linked_and_errors_together() {
    let env = ready_env("contract");
    env.add_session("sess-2", "uid-a", "标题二");
    env.add_body("sess-2", &body_text_with("sess-2", "契约用例的独立内容"));

    let first = copy(&env, "uid-b", &["sess-1", "sess-2"]);
    assert_eq!(first["copied"].as_array().unwrap().len(), 2);

    // 第二个会话的正文丢失：本次一个复用、一个失败。
    std::fs::remove_file(env.body_path("sess-2")).unwrap();
    let mixed = copy(&env, "uid-b", &["sess-1", "sess-2"]);
    assert_eq!(mixed["sourceUid"], "uid-a");
    assert_eq!(mixed["targetUid"], "uid-b");
    assert_eq!(mixed["copied"].as_array().unwrap().len(), 0);
    assert_eq!(mixed["alreadyLinked"].as_array().unwrap().len(), 1);
    assert_eq!(mixed["alreadyLinked"][0]["id"], "sess-1");
    assert_eq!(mixed["errors"].as_array().unwrap().len(), 1);
    assert_eq!(mixed["errors"][0]["id"], "sess-2");
    assert_eq!(mixed["errors"][0]["error"], "会话内容不存在，未复制");
    // 复用/失败都不算未完成写入。
    assert!(mixed.get("needsRecovery").is_none());
}

// ---------------------------------------------------------------------------
// 失败必须可见、可恢复（R1 / R5）
// ---------------------------------------------------------------------------

#[test]
fn missing_body_is_not_reported_as_success() {
    let env = ready_env("no-body");
    std::fs::remove_file(env.body_path("sess-1")).unwrap();

    let report = copy(&env, "uid-b", &["sess-1"]);
    assert_eq!(report["copied"].as_array().unwrap().len(), 0);
    assert_eq!(report["errors"][0]["error"], "会话内容不存在，未复制");
    assert_eq!(env.rows_for("uid-b").len(), 0, "不得写出半成品会话记录");
    assert_eq!(env.body_files().len(), 0);
}

#[test]
fn truncated_body_is_not_reported_as_success() {
    let env = ready_env("truncated");
    env.add_body("sess-1", "{\"sessionId\":\"sess-1\"\n");

    let report = copy(&env, "uid-b", &["sess-1"]);
    assert_eq!(report["copied"].as_array().unwrap().len(), 0);
    let error = report["errors"][0]["error"].as_str().unwrap();
    assert!(error.contains("会话内容无法验证"), "{error}");
    assert_eq!(env.rows_for("uid-b").len(), 0);
}

#[test]
fn missing_source_row_is_not_reported_as_success() {
    let env = ready_env("no-row");
    env.delete_row("sess-1");

    let report = copy(&env, "uid-b", &["sess-1"]);
    assert_eq!(report["copied"].as_array().unwrap().len(), 0);
    assert_eq!(
        report["errors"][0]["error"],
        "数据库中找不到源会话记录，未复制"
    );
    assert_eq!(env.body_files().len(), 1, "不得写出无数据库行的内容");
}

#[test]
fn source_row_of_another_account_is_rejected() {
    let env = ready_env("other-owner");
    let conn = Connection::open(env.paths.workbuddy_db()).unwrap();
    conn.execute(
        "UPDATE sessions SET user_id = 'uid-x' WHERE id = 'sess-1'",
        [],
    )
    .unwrap();
    drop(conn);

    let report = copy(&env, "uid-b", &["sess-1"]);
    assert_eq!(report["copied"].as_array().unwrap().len(), 0);
    assert_eq!(report["errors"][0]["error"], "源会话不属于当前账号，未复制");
}

/// 复制不再依赖云端映射库：库缺失也照常成功，不重建、不写行。
#[test]
fn copy_succeeds_without_edge_sync_db() {
    let env = ready_env("mapping-absent");
    std::fs::remove_file(env.paths.edge_sync_db(WbVariant::Cn)).unwrap();

    let report = copy(&env, "uid-b", &["sess-1"]);
    assert_eq!(report["copied"].as_array().unwrap().len(), 1);
    assert!(report.get("errors").is_none(), "{report}");
    assert_eq!(env.body_files().len(), 2);
    assert_eq!(env.rows_for("uid-b").len(), 1);
    assert!(
        !env.paths().edge_sync_db(WbVariant::Cn).exists(),
        "复制不得创建或写入映射库"
    );
    assert!(session_link::pending_operations(&env.paths(), WbVariant::Cn).is_empty());
}

/// 关联提交失败（存储目录不可写）→ 不报成功；恢复后同一 UUID 完成。
#[cfg(unix)]
#[test]
fn link_commit_failure_keeps_pending_then_recovers_with_same_uuid() {
    use std::os::unix::fs::PermissionsExt;

    let env = ready_env("link-commit");
    // 先跑一次成功，建立锁文件与存储文件，避免把「无法加锁」当成关联提交失败。
    copy(&env, "uid-c", &["sess-1"]);
    if std::fs::write(env.paths.store_root.join(".probe"), b"x").is_err() {
        return;
    }

    std::fs::set_permissions(
        &env.paths.store_root,
        std::fs::Permissions::from_mode(0o555),
    )
    .unwrap();
    if std::fs::write(env.paths.store_root.join(".probe"), b"x").is_ok() {
        // root / 特殊 ACL 环境写保护无效：跳过（不误报）。
        std::fs::set_permissions(
            &env.paths.store_root,
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        return;
    }

    let failed = copy(&env, "uid-b", &["sess-1"]);
    std::fs::set_permissions(
        &env.paths.store_root,
        std::fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    assert_eq!(failed["copied"].as_array().unwrap().len(), 0);
    let error = failed["errors"][0]["error"].as_str().unwrap();
    assert!(
        error.contains("保存失败") || error.contains("同步记录"),
        "{error}"
    );
    assert_eq!(failed["needsRecovery"], true);

    let pending = session_link::pending_operations(&env.paths(), WbVariant::Cn);
    let target = pending
        .iter()
        .find(|operation| operation.target.uid == "uid-b")
        .expect("应保留未完成操作");
    let new_id = target.target.session_id.clone();

    let retry = copy(&env, "uid-b", &["sess-1"]);
    assert_eq!(retry["copied"].as_array().unwrap().len(), 0);
    assert_eq!(retry["alreadyLinked"][0]["sessionId"], new_id);
    assert_eq!(
        env.body_files().len(),
        4,
        "两个目标各一份副本，重试不得新增"
    );
    assert_eq!(env.rows_for("uid-b"), vec![new_id]);
}

/// 正文写入失败：不得写出数据库行，也不报成功。
#[cfg(unix)]
#[test]
fn body_write_failure_reports_error_without_db_row() {
    use std::os::unix::fs::PermissionsExt;

    let env = ready_env("body-write");
    let ws = env.paths.projects_dir().join("ws-a");
    std::fs::set_permissions(&ws, std::fs::Permissions::from_mode(0o555)).unwrap();
    let writable = std::fs::write(ws.join(".probe"), b"x").is_ok();
    let report = copy(&env, "uid-b", &["sess-1"]);
    std::fs::set_permissions(&ws, std::fs::Permissions::from_mode(0o755)).unwrap();
    if writable {
        // root / 特殊 ACL 环境：写保护无效，跳过断言。
        return;
    }
    assert_eq!(report["copied"].as_array().unwrap().len(), 0);
    let error = report["errors"][0]["error"].as_str().unwrap();
    assert!(error.contains("复制后的内容保存失败"), "{error}");
    assert_eq!(env.rows_for("uid-b").len(), 0);
    assert_eq!(env.body_files().len(), 1);
}

#[test]
fn corrupt_store_blocks_copy_and_preserves_original() {
    let env = ready_env("corrupt-store");
    std::fs::create_dir_all(&env.paths.store_root).unwrap();
    std::fs::write(env.paths.session_links_file(), "not-json").unwrap();

    let report = copy(&env, "uid-b", &["sess-1"]);
    assert_eq!(report["copied"].as_array().unwrap().len(), 0);
    let error = report["errors"][0]["error"].as_str().unwrap();
    assert!(
        error.contains("同步记录") && error.contains("已阻止复制"),
        "{error}"
    );
    assert_eq!(env.rows_for("uid-b").len(), 0);
    assert_eq!(env.body_files().len(), 1);
    assert_eq!(
        std::fs::read_to_string(env.paths.session_links_file()).unwrap(),
        "not-json",
        "必须保留现场，不得当空表覆盖"
    );
}

#[test]
fn unknown_store_version_blocks_copy() {
    let env = ready_env("unknown-version");
    std::fs::create_dir_all(&env.paths.store_root).unwrap();
    std::fs::write(
        env.paths.session_links_file(),
        json!({"version": 99, "revision": 1, "groups": []}).to_string(),
    )
    .unwrap();

    let report = copy(&env, "uid-b", &["sess-1"]);
    assert_eq!(report["copied"].as_array().unwrap().len(), 0);
    let error = report["errors"][0]["error"].as_str().unwrap();
    assert!(error.contains("版本"), "{error}");
    assert_eq!(env.rows_for("uid-b").len(), 0);
}

/// 主文件被删但基线文件仍在 → 检测到痕迹，不得当首次使用重建空表，复制被阻止。
#[test]
fn copy_blocked_when_store_file_missing_but_baselines_remain() {
    let env = ready_env("baseline-trace");
    let first = copy(&env, "uid-b", &["sess-1"]);
    let new_id = env.first_copy_id(&first);
    assert!(env.baseline_files() > 0, "首次复制应留下基线文件");

    std::fs::remove_file(env.paths.session_links_file()).unwrap();
    let report = copy(&env, "uid-b", &["sess-1"]);
    assert_eq!(report["copied"].as_array().unwrap().len(), 0);
    assert_eq!(report["alreadyLinked"].as_array().unwrap().len(), 0);
    let error = report["errors"][0]["error"].as_str().unwrap();
    assert!(error.contains("保留现场"), "{error}");
    assert!(
        !env.paths.session_links_file().exists(),
        "不得重建空表覆盖现场"
    );
    assert_eq!(env.body_files().len(), 2, "不得产生第二个副本");
    assert_eq!(env.rows_for("uid-b"), vec![new_id]);
}

/// 并发请求：档位操作锁被占用时直接拒绝，不产生第二个副本。
#[test]
fn concurrent_request_is_rejected_without_second_copy() {
    let env = ready_env("concurrent");
    let held = session_link::try_acquire_variant_ops_lock(&env.paths(), WbVariant::Cn).unwrap();

    let err = copy_sessions_for_switch_at(
        &env.paths(),
        WbVariant::Cn,
        &env.target("uid-b"),
        &["sess-1".to_string()],
        |_| false,
    )
    .expect_err("持锁期间必须拒绝");
    assert!(err.contains("会话操作"), "{err}");
    assert_eq!(env.body_files().len(), 1);

    drop(held);
    let report = copy(&env, "uid-b", &["sess-1"]);
    assert_eq!(report["copied"].as_array().unwrap().len(), 1);
}

/// 目标账号不是当前登录账号（auth=uid-a）时，App 运行中不再拦截写入（S5 实测口径）；
/// 快拒与锁后复查仍各执行一次，且复查必须发生在持锁之后、写入之前。
#[test]
fn copy_runs_while_app_running_for_non_current_account() {
    let env = ready_env("app-running-non-current");
    let probes = std::cell::Cell::new(0usize);
    let lock_held_on_recheck = std::cell::Cell::new(false);
    let lock_path = env.paths.variant_ops_lock_file(WbVariant::Cn);
    let probe = |_: WbVariant| {
        let n = probes.get() + 1;
        probes.set(n);
        if n == 1 {
            true
        } else {
            // 第二次必须发生在持锁之后：此时再抢同一把锁应为 Busy。
            match session_link::try_lock_file(&lock_path) {
                Err(session_link::LockError::Busy) => lock_held_on_recheck.set(true),
                Err(session_link::LockError::Unavailable(reason)) => {
                    panic!("第二次探针时期望档位锁已被持有，实际 Unavailable: {reason}")
                }
                Ok(_) => panic!("第二次探针时期望档位锁已被持有，实际拿到了锁"),
            }
            true
        }
    };
    let report = copy_sessions_for_switch_at(
        &env.paths(),
        WbVariant::Cn,
        &env.target("uid-b"),
        &["sess-1".to_string()],
        probe,
    )
    .expect("目标非当前登录账号：运行中必须放行");
    assert_eq!(report["copied"].as_array().unwrap().len(), 1);
    assert_eq!(probes.get(), 2, "快拒与锁后复查各检查一次");
    assert!(
        lock_held_on_recheck.get(),
        "复查必须发生在已经拿到档位锁之后、任何写入之前"
    );
    assert_eq!(env.rows_for("uid-b").len(), 1, "副本行已写入");
    assert!(env.paths.session_links_file().exists(), "关联记录已建立");
}

// ---------------------------------------------------------------------------
// 恢复
// ---------------------------------------------------------------------------

#[test]
fn recovery_abandons_prepared_operation_when_source_is_gone() {
    let env = ready_env("abandon");
    let paths = env.paths();
    session_link::save_operation(
        &paths,
        &Operation {
            version: crate::modules::session_link::OPERATION_VERSION,
            operation_id: "op-gone".to_string(),
            kind: "copy".to_string(),
            variant: WbVariant::Cn,
            source_variant: None,
            group_id: "g-gone".to_string(),
            source: OperationMember {
                account_id: None,
                uid: "uid-a".to_string(),
                session_id: "sess-gone".to_string(),
            },
            target: OperationMember {
                account_id: None,
                uid: "uid-b".to_string(),
                session_id: "new-gone".to_string(),
            },
            expected_content_digest: "d".to_string(),
            expected_record_count: 1,
            phase: crate::modules::session_link::OpPhase::Prepared,
            backup: None,
            lifecycle_version: None,
            cleanup_state: None,
            last_error: None,
            created_at: 1,
            updated_at: 1,
        },
    )
    .unwrap();

    let report = recover_pending_session_operations_at(&paths, WbVariant::Cn);
    assert_eq!(report.abandoned, vec!["op-gone".to_string()]);
    assert!(report.is_clean());
    assert!(session_link::pending_operations(&paths, WbVariant::Cn).is_empty());
    assert_eq!(env.body_files().len(), 1);
}

#[test]
fn recovery_stops_when_intermediate_body_was_modified() {
    let env = ready_env("recovery-stop");
    let first = copy(&env, "uid-b", &["sess-1"]);
    let new_id = env.first_copy_id(&first);
    let paths = env.paths();

    // 模拟"正文已写、后续阶段中断"：把已完成的操作日志回退到 BodyWritten。
    let mut operation = session_link::scan_operations(&paths)
        .operations
        .into_iter()
        .find(|operation| operation.target.session_id == new_id)
        .expect("应能找到该副本的操作记录");
    operation.phase = OpPhase::BodyWritten;
    session_link::save_operation(&paths, &operation).unwrap();

    // 中间产物被其它程序改动 → 停止恢复，不覆盖。
    let tampered = format!(
        "{}\n",
        json!({"type": "user", "sessionId": new_id, "text": "别人改的"})
    );
    std::fs::write(env.body_path(&new_id), &tampered).unwrap();

    let report = recover_pending_session_operations_at(&paths, WbVariant::Cn);
    assert!(!report.is_clean());
    let issue = &report.needs_recovery[0];
    assert!(!issue.retryable);
    assert!(
        issue.reason.contains("目标内容与操作记录不一致"),
        "{}",
        issue.reason
    );
    assert_eq!(
        std::fs::read_to_string(env.body_path(&new_id)).unwrap(),
        tampered
    );

    // 该会话再次请求时不新建副本，而是报告未完成。
    let again = copy(&env, "uid-b", &["sess-1"]);
    assert_eq!(again["copied"].as_array().unwrap().len(), 0);
    assert_eq!(again["errors"].as_array().unwrap().len(), 1);
    assert!(again["errors"][0]["error"]
        .as_str()
        .unwrap()
        .contains("上一次复制尚未完成"));
    assert_eq!(env.body_files().len(), 2, "不得产生第二个副本");
}

/// 复制完成后再次请求同一会话：报告 alreadyLinked，不新建副本。
#[test]
fn retry_after_completed_copy_does_not_duplicate() {
    let env = ready_env("partial-resume");
    let first = copy(&env, "uid-b", &["sess-1"]);
    let new_id = env.first_copy_id(&first);
    let body_count = env.body_files().len();
    assert_eq!(body_count, 2);

    let again = copy(&env, "uid-b", &["sess-1"]);
    assert_eq!(again["copied"].as_array().unwrap().len(), 0);
    assert_eq!(again["alreadyLinked"][0]["sessionId"], new_id);
    assert_eq!(env.body_files().len(), body_count, "不得产生第二个副本");
    assert_eq!(env.rows_for("uid-b").len(), 1);
}

/// 恢复不重放已完成的阶段：产物全部就位、只剩 Completed 未写时，只补写阶段标记，
/// 不重写关联存储与基线、不重复登记映射（design §5）。
#[test]
fn recovery_completes_without_replaying_finished_stages() {
    let env = ready_env("recover-no-replay");
    let first = copy(&env, "uid-b", &["sess-1"]);
    let new_id = env.first_copy_id(&first);
    let paths = env.paths();

    // 模拟「关联已提交、Completed 写入失败」：把已完成的操作日志回退到 LinksCommitted。
    let mut operation = session_link::scan_operations(&paths)
        .operations
        .into_iter()
        .find(|operation| operation.target.session_id == new_id)
        .expect("应能找到该副本的操作记录");
    operation.phase = OpPhase::LinksCommitted;
    session_link::save_operation(&paths, &operation).unwrap();
    let operation_id = operation.operation_id.clone();

    let before = env.store();
    let revision_before = before.revision;
    let pair_bases_before = before.groups[0].pair_bases.clone();
    let baselines_before = env.baseline_files();
    let members_before = before.groups[0].members.len();

    let report = recover_pending_session_operations_at(&paths, WbVariant::Cn);
    assert_eq!(report.recovered, vec![operation_id]);
    assert!(report.is_clean(), "{:?}", report.needs_recovery);

    let after = env.store();
    assert_eq!(after.revision, revision_before, "恢复不得重写同步记录");
    assert_eq!(after.groups[0].pair_bases.len(), pair_bases_before.len());
    assert_eq!(
        after.groups[0].pair_bases[0].baseline_ref, pair_bases_before[0].baseline_ref,
        "不得重写配对基线"
    );
    assert_eq!(after.groups[0].members.len(), members_before);
    assert_eq!(env.baseline_files(), baselines_before, "不得新增基线文件");
    assert_eq!(
        env.mapping_rows(),
        0,
        "复制不再预写云端映射，恢复也不得补写"
    );
    assert_eq!(env.body_files().len(), 2, "不得产生第二个副本");
    assert_eq!(env.rows_for("uid-b"), vec![new_id]);
    assert!(session_link::pending_operations(&paths, WbVariant::Cn).is_empty());
}

/// phase 已是 LinksCommitted，但关联主文件被删：不得跳过核验后标 Completed。
#[test]
fn recovery_stops_when_committed_links_are_missing() {
    let env = ready_env("recover-links-gone");
    let first = copy(&env, "uid-b", &["sess-1"]);
    let new_id = env.first_copy_id(&first);
    let paths = env.paths();

    let mut operation = session_link::scan_operations(&paths)
        .operations
        .into_iter()
        .find(|operation| operation.target.session_id == new_id)
        .expect("应能找到该副本的操作记录");
    operation.phase = OpPhase::LinksCommitted;
    session_link::save_operation(&paths, &operation).unwrap();
    std::fs::remove_file(paths.session_links_file()).unwrap();

    let report = recover_pending_session_operations_at(&paths, WbVariant::Cn);
    assert!(report.recovered.is_empty(), "{:?}", report.recovered);
    assert_eq!(report.needs_recovery.len(), 1);
    assert!(!report.needs_recovery[0].retryable);
    assert!(
        report.needs_recovery[0].reason.contains("同步记录"),
        "{}",
        report.needs_recovery[0].reason
    );
    assert!(
        !paths.session_links_file().exists(),
        "不得把缺失的主文件当成空表重建"
    );
    assert_eq!(env.body_files().len(), 2, "不得产生第二个副本");
    assert_eq!(
        session_link::pending_operations(&paths, WbVariant::Cn).len(),
        1,
        "必须保留未完成操作，不能标 Completed"
    );
}

// ---------------------------------------------------------------------------
// 账户/环境辅助
// ---------------------------------------------------------------------------

#[test]
fn unparseable_operation_log_blocks_copy_without_second_replica() {
    let env = ready_env("bad-op-json");
    std::fs::create_dir_all(env.paths.operations_dir()).unwrap();
    std::fs::write(env.paths.operations_dir().join("broken.json"), "not-json").unwrap();

    let recovery = recover_pending_session_operations_at(&env.paths(), WbVariant::Cn);
    assert!(!recovery.is_clean());
    assert_eq!(recovery.needs_recovery.len(), 1);
    assert!(!recovery.needs_recovery[0].retryable);
    assert!(
        recovery.needs_recovery[0]
            .reason
            .contains(UNPARSEABLE_OPERATION_REASON),
        "{}",
        recovery.needs_recovery[0].reason
    );

    let report = copy(&env, "uid-b", &["sess-1"]);
    assert_eq!(report["copied"].as_array().unwrap().len(), 0);
    assert_eq!(report["alreadyLinked"].as_array().unwrap().len(), 0);
    assert_eq!(report["needsRecovery"], true);
    let error = report["errors"][0]["error"].as_str().unwrap();
    assert!(error.contains(UNPARSEABLE_OPERATION_REASON), "{error}");
    assert_eq!(
        env.body_files().len(),
        1,
        "不得绕过损坏的操作记录写出第二个副本"
    );
    assert_eq!(env.rows_for("uid-b").len(), 0);
}

#[test]
fn recovery_reports_store_unavailable_instead_of_writing() {
    let env = ready_env("store-broken-recovery");
    std::fs::create_dir_all(&env.paths.store_root).unwrap();
    std::fs::write(env.paths.session_links_file(), "not-json").unwrap();

    let report = recover_pending_session_operations_at(&env.paths(), WbVariant::Cn);
    assert!(report.is_clean(), "没有未完成操作时不做任何事");
    assert_eq!(
        std::fs::read_to_string(env.paths.session_links_file()).unwrap(),
        "not-json"
    );
}

#[test]
fn session_display_title_prefers_custom_title() {
    assert_eq!(
        session_display_title(Some("自动标题".into()), Some("美团每日自动领券".into())),
        "美团每日自动领券"
    );
    assert_eq!(
        session_display_title(None, Some("美团每日自动领券".into())),
        "美团每日自动领券"
    );
    assert_eq!(
        session_display_title(Some("汉字详情页".into()), None),
        "汉字详情页"
    );
    assert_eq!(session_display_title(None, None), "(无标题)");
    assert_eq!(
        session_display_title(Some("  ".into()), Some("".into())),
        "(无标题)"
    );
}

#[test]
fn claw_workspace_detected_by_folder_name() {
    assert!(is_claw_workspace("/Users/apple/WorkBuddy/Claw"));
    assert!(is_claw_workspace("/Users/apple/WorkBuddy/claw/"));
    assert!(is_claw_workspace(r"C:\Users\me\WorkBuddy\Claw"));
    assert!(!is_claw_workspace("/Users/apple/WorkBuddy/ClawBot"));
    assert!(!is_claw_workspace(
        "/Users/apple/Documents/AI-PROJECT/LetterTotTown"
    ));
}

#[test]
fn list_sessions_marks_has_history() {
    let env = ready_env("list-sessions");
    let sessions = list_sessions_for_user_at(&env.paths(), "uid-a");
    assert_eq!(sessions.as_array().unwrap().len(), 1);
    assert_eq!(sessions[0]["id"], "sess-1");
    assert_eq!(sessions[0]["hasHistory"], true);

    std::fs::remove_file(env.body_path("sess-1")).unwrap();
    let sessions = list_sessions_for_user_at(&env.paths(), "uid-a");
    assert_eq!(sessions[0]["hasHistory"], false);
}

#[test]
fn insert_session_copy_duplicates_row_with_target_uid() {
    let env = ready_env("insert");
    let conn = Connection::open(env.paths.workbuddy_db()).unwrap();
    conn.execute(
            "INSERT INTO sessions (id, user_id, title, cwd, created_at, updated_at, deleted_at, is_playground)
             VALUES ('src-1', 'uid-a', '旧标题', '/ws', 1000, 2000, NULL, 0)",
            [],
        )
        .unwrap();
    drop(conn);

    let outcome = insert_session_copy(
        &env.paths(),
        &env.paths(),
        "new-uuid-1",
        "src-1",
        "uid-a",
        "uid-b",
    )
    .unwrap();
    assert_eq!(outcome, DbCopyOutcome::Inserted);

    let conn = Connection::open(env.paths.workbuddy_db()).unwrap();
    let (id, user_id, title, deleted_at, is_playground, created_at, updated_at): (
        String,
        String,
        String,
        Option<i64>,
        i64,
        Option<i64>,
        Option<i64>,
    ) = conn
        .query_row(
            "SELECT id, user_id, title, deleted_at, is_playground, created_at, updated_at \
                 FROM sessions WHERE id = 'new-uuid-1'",
            [],
            |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                    r.get(6)?,
                ))
            },
        )
        .unwrap();
    assert_eq!(id, "new-uuid-1");
    assert_eq!(user_id, "uid-b");
    assert_eq!(title, "旧标题"); // 普通列原样保留
    assert_eq!(deleted_at, None);
    assert_eq!(is_playground, 0);
    assert_eq!(created_at, Some(1000), "created_at 必须是源的会话起点");
    assert!(
        updated_at.is_some_and(|value| value > 2000),
        "updated_at 必须刷新为本次时间：{updated_at:?}"
    );
}

#[test]
fn insert_session_copy_normalizes_visibility_fields() {
    type NormalizedCols = (
        Option<String>,
        Option<String>,
        Option<i64>,
        Option<String>,
        Option<i64>,
    );
    let env = ready_env("insert-normalize");
    let conn = Connection::open(env.paths.workbuddy_db()).unwrap();
    conn.execute_batch(
        "ALTER TABLE sessions ADD COLUMN project_id TEXT;
             ALTER TABLE sessions ADD COLUMN status TEXT;
             ALTER TABLE sessions ADD COLUMN last_activity_at INTEGER;
             ALTER TABLE sessions ADD COLUMN source_mode TEXT;",
    )
    .unwrap();
    conn.execute(
        "UPDATE sessions SET project_id = 'p_source', status = 'terminated', \
             last_activity_at = NULL, source_mode = NULL, created_at = 1234 \
             WHERE id = 'sess-1'",
        [],
    )
    .unwrap();
    drop(conn);

    let outcome = insert_session_copy(
        &env.paths(),
        &env.paths(),
        "normalized-copy",
        "sess-1",
        "uid-a",
        "uid-b",
    )
    .unwrap();
    assert_eq!(outcome, DbCopyOutcome::Inserted);

    let conn = Connection::open(env.paths.workbuddy_db()).unwrap();
    let (project_id, status, last_activity_at, source_mode, created_at): NormalizedCols = conn
        .query_row(
            "SELECT project_id, status, last_activity_at, source_mode, created_at \
                 FROM sessions WHERE id = 'normalized-copy'",
            [],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            },
        )
        .unwrap();
    assert_eq!(project_id, None, "项目归属不得跨账号继承");
    assert_eq!(
        status.as_deref(),
        Some("completed"),
        "非空闲/终态状态应归一为 completed"
    );
    assert!(
        last_activity_at.is_some_and(|value| value > 0),
        "last_activity_at 为 NULL 时必须补当前时间"
    );
    assert_eq!(source_mode.as_deref(), Some("craft"));
    assert_eq!(created_at, Some(1234), "created_at 必须保留源会话起点");
}

#[test]
fn insert_session_copy_reports_missing_source_and_db() {
    let env = ready_env("insert-missing");
    assert_eq!(
        insert_session_copy(
            &env.paths(),
            &env.paths(),
            "new-1",
            "missing",
            "uid-a",
            "uid-b"
        )
        .unwrap(),
        DbCopyOutcome::SourceRowMissing,
        "源行缺失必须显式上报，不能当成功（旧实现的假成功）"
    );

    std::fs::remove_file(env.paths.workbuddy_db()).unwrap();
    assert_eq!(
        insert_session_copy(
            &env.paths(),
            &env.paths(),
            "new-1",
            "sess-1",
            "uid-a",
            "uid-b"
        )
        .unwrap(),
        DbCopyOutcome::NoDb
    );
}

#[test]
fn backup_failure_is_propagated_instead_of_claimed_success() {
    let env = ready_env("backup-fail");
    std::fs::remove_file(env.paths.workbuddy_db()).unwrap();
    let err = backup_workbuddy_db(&env.paths(), &env.paths.backup_root()).unwrap_err();
    assert!(err.contains("会话数据不存在"), "{err}");

    // 正常备份返回主库路径且大小一致。
    env.create_db();
    env.add_session("sess-1", "uid-a", "标题一");
    let backup = backup_workbuddy_db(&env.paths(), &env.paths.backup_root()).unwrap();
    assert!(backup.ends_with("workbuddy.db"));
    let conn = open_db(&backup, true).unwrap();
    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM sessions", [], |row| row.get(0))
        .unwrap();
    assert_eq!(count, 1);
}

#[test]
fn session_row_owner_reads_target_uid() {
    let env = ready_env("row-owner");
    assert_eq!(
        session_row_owner(&env.paths(), "sess-1").as_deref(),
        Some("uid-a")
    );
    assert_eq!(session_row_owner(&env.paths(), "missing"), None);
    env.delete_row("sess-1");
    assert_eq!(session_row_owner(&env.paths(), "sess-1"), None);
}

// ---------------------------------------------------------------------------
// 跨档复制（WorkBuddy 国内版 ↔ 国际版）
// ---------------------------------------------------------------------------

/// 双档测试环境：共享工具存储根（关联表/锁/备份）+ 两个档位数据根（CN 与 AI）。
struct CrossEnv {
    root: PathBuf,
    store_root: PathBuf,
    cn_data: PathBuf,
    ai_data: PathBuf,
}

impl CrossEnv {
    fn new(name: &str) -> Self {
        let root = std::env::temp_dir().join(format!(
            "wb_switch_cross_test_{}_{name}",
            uuid::Uuid::new_v4().simple()
        ));
        let store_root = root.join("store");
        let cn_data = root.join("cn-data");
        let ai_data = root.join("ai-data");
        for data in [&cn_data, &ai_data] {
            std::fs::create_dir_all(data.join("projects").join("ws-a")).unwrap();
        }
        CrossEnv {
            root,
            store_root,
            cn_data,
            ai_data,
        }
    }

    fn paths(&self, variant: WbVariant) -> SessionPaths {
        SessionPaths {
            store_root: self.store_root.clone(),
            data_root: match variant {
                WbVariant::Cn => self.cn_data.clone(),
                WbVariant::Ai => self.ai_data.clone(),
            },
            // 跨档入口不读登录态（源 uid 由调用方显式传入）；门禁用它判断
            // 目标账号是否为该档当前登录账号。
            auth_file: self
                .store_root
                .join(format!("auth-{}.info", variant.as_str())),
            link_namespace: LinkNamespace::WorkBuddy,
        }
    }

    /// 写入指定档位的登录态（门禁读 `account.uid` 判定目标是否当前登录账号）。
    fn set_login(&self, variant: WbVariant, uid: &str) {
        let path = self.paths(variant).auth_file;
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, json!({"account": {"uid": uid}}).to_string()).unwrap();
    }

    /// 建 sessions 表；`extra` 追加一列，用于模拟两档列集合差异。
    fn create_db(&self, variant: WbVariant, extra: &str) {
        let conn = Connection::open(self.paths(variant).workbuddy_db()).unwrap();
        conn.execute_batch(&format!(
            "CREATE TABLE sessions (
                    id TEXT PRIMARY KEY,
                    user_id TEXT NOT NULL,
                    title TEXT,
                    custom_title TEXT,
                    cwd TEXT,
                    created_at INTEGER,
                    updated_at INTEGER,
                    deleted_at INTEGER,
                    is_playground INTEGER{extra}
                );"
        ))
        .unwrap();
    }

    fn create_edge_db(&self, variant: WbVariant) {
        let conn = Connection::open(self.paths(variant).edge_sync_db(variant)).unwrap();
        conn.execute_batch(
            "CREATE TABLE edge_sync_mapping (
                    session_id TEXT,
                    conversation_id TEXT,
                    msg_channel TEXT,
                    created_at INTEGER
                );",
        )
        .unwrap();
    }

    fn add_session(&self, variant: WbVariant, id: &str, uid: &str, title: &str) {
        let conn = Connection::open(self.paths(variant).workbuddy_db()).unwrap();
        conn.execute(
                "INSERT INTO sessions (id, user_id, title, custom_title, cwd, created_at, updated_at, deleted_at, is_playground)
                 VALUES (?1, ?2, ?3, NULL, '/ws/a', 1000, 2000, NULL, 0)",
                rusqlite::params![id, uid, title],
            )
            .unwrap();
    }

    fn add_body(&self, variant: WbVariant, cid: &str, text: &str) {
        std::fs::write(
            self.paths(variant)
                .projects_dir()
                .join("ws-a")
                .join(format!("{cid}.jsonl")),
            text,
        )
        .unwrap();
    }

    fn body_path(&self, variant: WbVariant, cid: &str) -> PathBuf {
        self.paths(variant)
            .projects_dir()
            .join("ws-a")
            .join(format!("{cid}.jsonl"))
    }

    fn rows_for(&self, variant: WbVariant, uid: &str) -> Vec<String> {
        let conn = Connection::open(self.paths(variant).workbuddy_db()).unwrap();
        let mut stmt = conn
            .prepare("SELECT id FROM sessions WHERE user_id = ?1 AND deleted_at IS NULL")
            .unwrap();
        let mut rows: Vec<String> = stmt
            .query_map([uid], |row| row.get::<_, String>(0))
            .unwrap()
            .flatten()
            .collect();
        rows.sort();
        rows
    }

    fn mapping_rows(&self, variant: WbVariant) -> Vec<(String, String)> {
        let conn = Connection::open(self.paths(variant).edge_sync_db(variant)).unwrap();
        let mut stmt = conn
            .prepare("SELECT session_id, msg_channel FROM edge_sync_mapping ORDER BY session_id")
            .unwrap();
        stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .flatten()
            .collect()
    }

    fn delete_session(&self, variant: WbVariant, id: &str) {
        let conn = Connection::open(self.paths(variant).workbuddy_db()).unwrap();
        conn.execute("DELETE FROM sessions WHERE id = ?1", [id])
            .unwrap();
    }
}

impl Drop for CrossEnv {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn cross_side<'a>(paths: &'a SessionPaths, variant: WbVariant, uid: &str) -> CopySide<'a> {
    CopySide {
        paths,
        variant,
        uid: uid.to_string(),
        account_id: Some(format!("acc-{uid}")),
    }
}

/// 可注入双档临时目录的复制调用（解析器与两条 side 一致，不触碰真实路径）。
fn cross_copy(
    env: &CrossEnv,
    source_variant: WbVariant,
    source_uid: &str,
    target_variant: WbVariant,
    target_uid: &str,
    ids: &[&str],
) -> Value {
    let source_paths = env.paths(source_variant);
    let target_paths = env.paths(target_variant);
    let ids: Vec<String> = ids.iter().map(|id| id.to_string()).collect();
    copy_sessions_cross_at(
        cross_side(&source_paths, source_variant, source_uid),
        cross_side(&target_paths, target_variant, target_uid),
        &ids,
        |_| false,
        |variant| env.paths(variant),
    )
    .unwrap()
}

/// 与 `cross_copy` 相同，但注入「App 是否运行」探针（门禁测试用）。
fn cross_copy_with_probe(
    env: &CrossEnv,
    source_variant: WbVariant,
    source_uid: &str,
    target_variant: WbVariant,
    target_uid: &str,
    ids: &[&str],
    is_app_running: impl Fn(WbVariant) -> bool,
) -> Result<Value, String> {
    let source_paths = env.paths(source_variant);
    let target_paths = env.paths(target_variant);
    let ids: Vec<String> = ids.iter().map(|id| id.to_string()).collect();
    copy_sessions_cross_at(
        cross_side(&source_paths, source_variant, source_uid),
        cross_side(&target_paths, target_variant, target_uid),
        &ids,
        is_app_running,
        |variant| env.paths(variant),
    )
}

/// 跨档门禁只拦「目标账号 = 目标档当前登录账号」：App 运行中
/// 目标≠当前登录 → 放行；目标=当前登录 → 拒绝；读不到登录态 → 保守拒绝。
#[test]
fn cross_copy_gate_only_blocks_current_login_target() {
    let env = CrossEnv::new("cross-gate");
    env.create_db(WbVariant::Cn, "");
    env.create_edge_db(WbVariant::Cn);
    env.create_db(WbVariant::Ai, "");
    env.create_edge_db(WbVariant::Ai);
    env.add_session(WbVariant::Cn, "sess-1", "uid-a", "标题一");
    env.add_body(WbVariant::Cn, "sess-1", &body_text("sess-1"));

    let run = |uid: &str| {
        cross_copy_with_probe(
            &env,
            WbVariant::Cn,
            "uid-a",
            WbVariant::Ai,
            uid,
            &["sess-1"],
            |_| true,
        )
    };

    // 目标档登录的是别人（uid-x）：运行中放行，副本落库。
    env.set_login(WbVariant::Ai, "uid-x");
    let report = run("uid-b").expect("目标≠当前登录：运行中必须放行");
    let new_id = report["copied"][0]["newId"].as_str().unwrap().to_string();
    assert_eq!(env.rows_for(WbVariant::Ai, "uid-b"), vec![new_id]);

    // 目标账号正是目标档当前登录账号：运行中拒绝，且不再写入。
    env.set_login(WbVariant::Ai, "uid-b");
    let err = run("uid-b").unwrap_err();
    assert_eq!(err, SESSION_COPY_APP_RUNNING);
    assert_eq!(
        env.rows_for(WbVariant::Ai, "uid-b").len(),
        1,
        "未新增副本行"
    );

    // 读不到登录态（认证文件缺失）：保守拒绝。
    std::fs::remove_file(env.paths(WbVariant::Ai).auth_file).unwrap();
    let err = run("uid-b").unwrap_err();
    assert_eq!(err, SESSION_COPY_APP_RUNNING);

    // App 未运行：任何目标都放行（登录态缺失也不再拦截）；此时源会话已有
    // 有效副本，返回 alreadyLinked 而非再次复制。
    let report = cross_copy_with_probe(
        &env,
        WbVariant::Cn,
        "uid-a",
        WbVariant::Ai,
        "uid-b",
        &["sess-1"],
        |_| false,
    )
    .unwrap();
    assert!(
        !report["alreadyLinked"].as_array().unwrap().is_empty(),
        "{report}"
    );
    assert!(report.get("errors").is_none(), "{report}");
}

/// 基础跨档复制（CN → AI）：正文按源工作区同名目录落到目标档，行写目标档、
/// 映射行交给客户端，关联组两个成员各记自己的档位。
#[test]
fn cross_copy_lands_body_row_and_member_variants() {
    let env = CrossEnv::new("cn-to-ai");
    env.create_db(WbVariant::Cn, "");
    env.create_edge_db(WbVariant::Cn);
    env.create_db(WbVariant::Ai, "");
    env.create_edge_db(WbVariant::Ai);
    env.add_session(WbVariant::Cn, "sess-1", "uid-a", "标题一");
    env.add_body(WbVariant::Cn, "sess-1", &body_text("sess-1"));

    let report = cross_copy(
        &env,
        WbVariant::Cn,
        "uid-a",
        WbVariant::Ai,
        "uid-b",
        &["sess-1"],
    );
    assert_eq!(report["sourceVariant"], "cn");
    assert_eq!(report["targetVariant"], "ai");
    assert_eq!(report["sourceUid"], "uid-a");
    assert_eq!(report["targetUid"], "uid-b");
    let new_id = report["copied"][0]["newId"].as_str().unwrap().to_string();
    assert_ne!(new_id, "sess-1");

    // 正文：目标档同名工作区目录，id 全量替换；源档不新增文件。
    let target_body = env.body_path(WbVariant::Ai, &new_id);
    assert!(target_body.is_file());
    let text = std::fs::read_to_string(&target_body).unwrap();
    assert!(text.contains(&new_id));
    assert!(!text.contains("sess-1"));
    assert!(!env.body_path(WbVariant::Cn, &new_id).exists());

    assert_eq!(env.rows_for(WbVariant::Ai, "uid-b"), vec![new_id.clone()]);
    assert_eq!(
        env.rows_for(WbVariant::Cn, "uid-a"),
        vec!["sess-1".to_string()]
    );

    // 不预写映射行：云端登记交接给客户端，两档映射表都不应出现工具写入。
    assert!(env.mapping_rows(WbVariant::Ai).is_empty());
    assert!(env.mapping_rows(WbVariant::Cn).is_empty());

    // 关联组：跨档组，两成员各记自己的档位。
    let store = match session_link::load_store(&env.paths(WbVariant::Ai)) {
        StoreState::Ready(store) => store,
        other => panic!("同步记录应为 Ready，实际 {other:?}"),
    };
    assert_eq!(store.groups.len(), 1);
    let group = &store.groups[0];
    let source_member = session_link::find_member(group, "uid-a", "sess-1").unwrap();
    assert_eq!(source_member.variant, Some(WbVariant::Cn));
    let target_member = session_link::find_member(group, "uid-b", &new_id).unwrap();
    assert_eq!(target_member.variant, Some(WbVariant::Ai));
}

/// 跨档目标已有等价副本但没有任何关联记录时，必须复用该副本，不能新建随机 UUID。
///
/// 这是跨档内容兜底回归：源元数据必须从源档数据库读取；目标档数据库里没有源 cid。
#[test]
fn cross_copy_reuses_existing_equivalent_target_without_link() {
    let env = CrossEnv::new("cross-equivalent-no-link");
    env.create_db(WbVariant::Cn, "");
    env.create_edge_db(WbVariant::Cn);
    env.create_db(WbVariant::Ai, "");
    env.create_edge_db(WbVariant::Ai);
    env.add_session(WbVariant::Cn, "sess-1", "uid-a", "标题一");
    env.add_body(WbVariant::Cn, "sess-1", &body_text("sess-1"));
    // 目标账号已有同 cwd、同标题、正文等价的历史副本，但没有关联组。
    env.add_session(WbVariant::Ai, "sess-b", "uid-b", "标题一");
    env.add_body(WbVariant::Ai, "sess-b", &body_text("sess-b"));

    let report = cross_copy(
        &env,
        WbVariant::Cn,
        "uid-a",
        WbVariant::Ai,
        "uid-b",
        &["sess-1"],
    );
    assert_eq!(report["copied"].as_array().unwrap().len(), 0, "{report}");
    assert_eq!(
        report["alreadyLinked"].as_array().unwrap().len(),
        1,
        "{report}"
    );
    assert_eq!(report["alreadyLinked"][0]["sessionId"], "sess-b");
    assert_eq!(
        env.rows_for(WbVariant::Ai, "uid-b"),
        vec!["sess-b".to_string()]
    );
}

/// P1a 只读审计：按消息序列公共前缀分类，不修改会话。
/// P1c：软隐藏精确重复组时保留正文与保留项，只写 deleted_at。
/// P1c：解除关联并隐藏时，成员从关联组移除，但正文保留。
#[test]
fn hide_duplicate_group_unlinks_alias_before_soft_hide() {
    let env = Env::new("hide-linked");
    env.create_db();
    env.create_edge_db(WbVariant::Cn);
    for id in ["keep", "alias"] {
        env.add_session(id, "uid-a", "same");
        env.add_body(id, &body_text(id));
    }
    let paths = env.paths();
    session_link::with_link_store_write(&paths, |store| {
        for (group_id, member_id, session_id) in [
            ("g-keep", "m-keep", "keep"),
            ("g-alias", "m-alias", "alias"),
        ] {
            store.groups.push(LinkGroup {
                id: group_id.to_string(),
                variant: WbVariant::Cn,
                created_at: 1,
                members: vec![LinkMember {
                    member_id: member_id.to_string(),
                    account_id: None,
                    uid: "uid-a".to_string(),
                    session_id: session_id.to_string(),
                    variant: None,
                    state: MemberState::Active,
                    linked_at: 1,
                    last_synced_at: None,
                }],
                pair_bases: vec![],
            });
        }
        Ok(())
    })
    .unwrap();

    let report = hide_duplicate_group_for_user_at(
        &paths,
        WbVariant::Cn,
        "uid-a",
        "/ws/a",
        "same",
        "keep",
        true,
    );
    assert_eq!(report["ok"], true, "{report}");
    assert_eq!(report["hidden"], 1, "{report}");
    assert_eq!(report["unlinked"], 1, "{report}");
    assert!(env.body_path("keep").is_file());
    assert!(env.body_path("alias").is_file());
    let store = match session_link::load_store(&paths) {
        session_link::StoreState::Ready(store) => store,
        other => panic!("关联存储应为 Ready，实际 {other:?}"),
    };
    assert!(store.groups.iter().any(|group| group.id == "g-keep"));
    let alias_group = store
        .groups
        .iter()
        .find(|group| group.id == "g-alias")
        .expect("stale 关联组必须保留，便于恢复");
    assert_eq!(
        alias_group.members[0].state,
        session_link::MemberState::Stale
    );

    let hides = list_duplicate_hides_for_user_at(&paths, WbVariant::Cn, "uid-a");
    let hide_id = hides["hides"][0]["id"].as_str().unwrap().to_string();
    let restored = restore_duplicate_hide_for_user_at(&paths, WbVariant::Cn, "uid-a", &hide_id);
    assert_eq!(restored["ok"], true, "{restored}");
    assert_eq!(restored["restored"], 1, "{restored}");
    assert_eq!(restored["reactivated"], 1, "{restored}");
    let conn = Connection::open(env.paths().workbuddy_db()).unwrap();
    let alias_deleted: Option<i64> = conn
        .query_row(
            "SELECT deleted_at FROM sessions WHERE id='alias'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(alias_deleted.is_none());
    let store = match session_link::load_store(&paths) {
        session_link::StoreState::Ready(store) => store,
        other => panic!("关联存储应为 Ready，实际 {other:?}"),
    };
    let alias_group = store
        .groups
        .iter()
        .find(|group| group.id == "g-alias")
        .unwrap();
    assert_eq!(
        alias_group.members[0].state,
        session_link::MemberState::Active
    );
    let hides = list_duplicate_hides_for_user_at(&paths, WbVariant::Cn, "uid-a");
    assert!(hides["hides"].as_array().unwrap().is_empty());
}

#[test]
fn hide_duplicate_group_soft_hides_exact_aliases_and_keeps_jsonl() {
    let env = Env::new("hide-exact");
    env.create_db();
    env.create_edge_db(WbVariant::Cn);
    for id in ["keep", "alias"] {
        env.add_session(id, "uid-a", "same");
        env.add_body(id, &body_text(id));
    }
    let report = hide_duplicate_group_for_user_at(
        &env.paths(),
        WbVariant::Cn,
        "uid-a",
        "/ws/a",
        "same",
        "keep",
        false,
    );
    assert_eq!(report["ok"], true, "{report}");
    assert_eq!(report["hidden"], 1, "{report}");
    assert_eq!(report["skippedLinked"], 0, "{report}");
    // 两条正文都仍在；只软隐藏 alias。
    assert!(env.body_path("keep").is_file());
    assert!(env.body_path("alias").is_file());
    let conn = Connection::open(env.paths().workbuddy_db()).unwrap();
    let keep_deleted: Option<i64> = conn
        .query_row("SELECT deleted_at FROM sessions WHERE id='keep'", [], |r| {
            r.get(0)
        })
        .unwrap();
    let alias_deleted: Option<i64> = conn
        .query_row(
            "SELECT deleted_at FROM sessions WHERE id='alias'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(keep_deleted.is_none());
    assert!(alias_deleted.is_some());
}

#[test]
fn audit_session_duplicates_classifies_exact_fast_forward_divergent_and_independent() {
    let env = Env::new("audit-duplicates");
    env.create_db();
    env.create_edge_db(WbVariant::Cn);

    for (id, title) in [("exact-a", "exact"), ("exact-b", "exact")] {
        env.add_session(id, "uid-a", title);
        env.add_body(id, &body_text(id));
    }
    let mut fast_long = body_text("fast-a");
    fast_long
        .push_str(&json!({"type": "user", "sessionId": "fast-a", "text": "extra"}).to_string());
    fast_long.push('\n');
    for (id, body) in [
        ("fast-a", fast_long.as_str()),
        ("fast-b", body_text("fast-b").as_str()),
    ] {
        env.add_session(id, "uid-a", "fast");
        env.add_body(id, body);
    }
    for id in ["div-a", "div-b"] {
        env.add_session(id, "uid-a", "div");
        let body = if id == "div-a" {
            body_text_with(id, "共同")
        } else {
            format!(
                "{}\n{}\n",
                json!({"type": "user", "sessionId": id, "text": "共同"}),
                json!({"type": "assistant", "sessionId": id, "text": "不同"})
            )
        };
        env.add_body(id, &body);
    }
    for id in ["ind-a", "ind-b"] {
        env.add_session(id, "uid-a", "ind");
        env.add_body(
            id,
            &body_text_with(id, if id == "ind-a" { "甲" } else { "乙" }),
        );
    }

    let report = audit_session_duplicates_for_user_at(&env.paths(), WbVariant::Cn, "uid-a");
    assert_eq!(report["ok"], true, "{report}");
    let summary = &report["summary"];
    assert_eq!(summary["exact"], 1, "{report}");
    assert_eq!(summary["fastForward"], 1, "{report}");
    assert_eq!(summary["divergent"], 1, "{report}");
    assert_eq!(summary["independent"], 1, "{report}");
    assert_eq!(summary["unavailable"], 0, "{report}");
    assert_eq!(report["groups"].as_array().unwrap().len(), 4, "{report}");
}

/// 目标档列集合与源档不同（双向缺列）时按目标列取交集：不写 NULL、不报错。
#[test]
fn cross_copy_uses_target_columns_intersection() {
    let env = CrossEnv::new("columns");
    // 源多一列 transport；目标多一列 unread。
    env.create_db(WbVariant::Cn, ", transport TEXT");
    env.create_edge_db(WbVariant::Cn);
    env.create_db(WbVariant::Ai, ", unread INTEGER");
    env.create_edge_db(WbVariant::Ai);
    env.add_session(WbVariant::Cn, "sess-1", "uid-a", "标题一");
    env.add_body(WbVariant::Cn, "sess-1", &body_text("sess-1"));

    let report = cross_copy(
        &env,
        WbVariant::Cn,
        "uid-a",
        WbVariant::Ai,
        "uid-b",
        &["sess-1"],
    );
    let new_id = report["copied"][0]["newId"].as_str().unwrap().to_string();

    let conn = Connection::open(env.paths(WbVariant::Ai).workbuddy_db()).unwrap();
    let (title, cwd, unread): (String, String, Option<i64>) = conn
        .query_row(
            "SELECT title, cwd, unread FROM sessions WHERE id = ?1",
            [&new_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(title, "标题一");
    assert_eq!(cwd, "/ws/a");
    // 目标独有列由库默认值补齐（无默认值时 NULL），不因源列结构报错。
    assert_eq!(unread, None);
}

/// 反向（AI → CN）同路径可用：正文/行/映射落 CN，成员档位正确。
#[test]
fn cross_copy_ai_to_cn_uses_same_kernel() {
    let env = CrossEnv::new("ai-to-cn");
    env.create_db(WbVariant::Ai, "");
    env.create_edge_db(WbVariant::Ai);
    env.create_db(WbVariant::Cn, "");
    env.create_edge_db(WbVariant::Cn);
    env.add_session(WbVariant::Ai, "sess-ai", "uid-ai", "AI 标题");
    env.add_body(WbVariant::Ai, "sess-ai", &body_text("sess-ai"));

    let report = cross_copy(
        &env,
        WbVariant::Ai,
        "uid-ai",
        WbVariant::Cn,
        "uid-cn",
        &["sess-ai"],
    );
    assert_eq!(report["sourceVariant"], "ai");
    assert_eq!(report["targetVariant"], "cn");
    let new_id = report["copied"][0]["newId"].as_str().unwrap().to_string();
    assert!(env.body_path(WbVariant::Cn, &new_id).is_file());
    assert_eq!(env.rows_for(WbVariant::Cn, "uid-cn"), vec![new_id.clone()]);
    assert!(
        env.mapping_rows(WbVariant::Cn).is_empty(),
        "复制不得预写映射行"
    );

    let store = match session_link::load_store(&env.paths(WbVariant::Cn)) {
        StoreState::Ready(store) => store,
        other => panic!("同步记录应为 Ready，实际 {other:?}"),
    };
    let group = &store.groups[0];
    assert_eq!(
        session_link::find_member(group, "uid-ai", "sess-ai")
            .unwrap()
            .variant,
        Some(WbVariant::Ai)
    );
    assert_eq!(
        session_link::find_member(group, "uid-cn", &new_id)
            .unwrap()
            .variant,
        Some(WbVariant::Cn)
    );
}

/// 同一跨档请求重复执行：第二次复用已有有效副本（alreadyLinked），不写第二个副本。
#[test]
fn cross_copy_reuses_existing_target_link() {
    let env = CrossEnv::new("already");
    env.create_db(WbVariant::Cn, "");
    env.create_edge_db(WbVariant::Cn);
    env.create_db(WbVariant::Ai, "");
    env.create_edge_db(WbVariant::Ai);
    env.add_session(WbVariant::Cn, "sess-1", "uid-a", "标题一");
    env.add_body(WbVariant::Cn, "sess-1", &body_text("sess-1"));

    let first = cross_copy(
        &env,
        WbVariant::Cn,
        "uid-a",
        WbVariant::Ai,
        "uid-b",
        &["sess-1"],
    );
    let new_id = first["copied"][0]["newId"].as_str().unwrap().to_string();
    let second = cross_copy(
        &env,
        WbVariant::Cn,
        "uid-a",
        WbVariant::Ai,
        "uid-b",
        &["sess-1"],
    );
    assert!(second["copied"].as_array().unwrap().is_empty(), "{second}");
    assert_eq!(
        second["alreadyLinked"][0]["sessionId"],
        new_id.as_str(),
        "{second}"
    );
    assert_eq!(env.rows_for(WbVariant::Ai, "uid-b"), vec![new_id]);
}

/// 跨档操作中断后恢复：源正文与源行从源档（sourceVariant）读取，产物落目标档。
#[test]
fn cross_recovery_reads_source_from_operation_variant() {
    let env = CrossEnv::new("recovery");
    env.create_db(WbVariant::Cn, "");
    env.create_edge_db(WbVariant::Cn);
    env.create_db(WbVariant::Ai, "");
    env.create_edge_db(WbVariant::Ai);
    env.add_session(WbVariant::Cn, "sess-1", "uid-a", "标题一");
    env.add_body(WbVariant::Cn, "sess-1", &body_text("sess-1"));

    let ai_paths = env.paths(WbVariant::Ai);
    // 与生产一致：操作产生于一次复制，关联存储主文件必然已存在（先建空表）。
    session_link::with_link_store_write(&ai_paths, |_| Ok(())).unwrap();
    // 手工留下一条 Prepared 阶段的跨档操作（CN → AI），模拟正文写入前中断。
    session_link::save_operation(
        &ai_paths,
        &Operation {
            version: crate::modules::session_link::OPERATION_VERSION,
            operation_id: "op-cross".to_string(),
            kind: "copy".to_string(),
            variant: WbVariant::Ai,
            source_variant: Some(WbVariant::Cn),
            group_id: "g-cross".to_string(),
            source: OperationMember {
                account_id: None,
                uid: "uid-a".to_string(),
                session_id: "sess-1".to_string(),
            },
            target: OperationMember {
                account_id: None,
                uid: "uid-b".to_string(),
                session_id: "new-cross".to_string(),
            },
            expected_content_digest: "将在恢复时按源内容刷新".to_string(),
            expected_record_count: 0,
            phase: crate::modules::session_link::OpPhase::Prepared,
            backup: None,
            lifecycle_version: None,
            cleanup_state: None,
            last_error: None,
            created_at: 1,
            updated_at: 1,
        },
    )
    .unwrap();

    let report = recover_pending_session_operations_at_with(&ai_paths, WbVariant::Ai, |variant| {
        env.paths(variant)
    });
    assert!(report.needs_recovery.is_empty(), "{report:?}");
    assert_eq!(report.recovered, vec!["op-cross".to_string()]);

    // 产物全部落到 AI（目标档）。
    assert!(env.body_path(WbVariant::Ai, "new-cross").is_file());
    assert_eq!(
        env.rows_for(WbVariant::Ai, "uid-b"),
        vec!["new-cross".to_string()]
    );
    assert!(
        env.mapping_rows(WbVariant::Ai).is_empty(),
        "恢复不得补写映射行"
    );
    let store = match session_link::load_store(&ai_paths) {
        StoreState::Ready(store) => store,
        other => panic!("同步记录应为 Ready，实际 {other:?}"),
    };
    let group = store
        .groups
        .iter()
        .find(|group| group.id == "g-cross")
        .expect("恢复后应提交关联组");
    assert_eq!(
        session_link::find_member(group, "uid-a", "sess-1")
            .unwrap()
            .variant,
        Some(WbVariant::Cn)
    );
}

/// 跨档预览：复制建组后判 identical；源追加 → fastForward；两边分别追加 → diverge；
/// 源行删除但正文仍在 → 仍可判定（来源只读）；目标行删除 → unknown。
/// 内容与标题按成员档位读取（源读 CN、目标读 AI）。
#[test]
fn cross_preview_verdicts_follow_member_content() {
    let env = CrossEnv::new("preview");
    env.create_db(WbVariant::Cn, "");
    env.create_edge_db(WbVariant::Cn);
    env.create_db(WbVariant::Ai, "");
    env.create_edge_db(WbVariant::Ai);
    env.add_session(WbVariant::Cn, "sess-1", "uid-a", "标题一");
    env.add_body(WbVariant::Cn, "sess-1", &body_text("sess-1"));

    let source_paths = env.paths(WbVariant::Cn);
    let target_paths = env.paths(WbVariant::Ai);
    let source_acc = json!({"id": "acc-uid-a", "uid": "uid-a", "variant": "cn"});
    let target_acc = json!({"id": "acc-uid-b", "uid": "uid-b", "variant": "ai"});
    let preview = || {
        session_links_preview_cross_at(
            &source_paths,
            WbVariant::Cn,
            &source_acc,
            &target_paths,
            WbVariant::Ai,
            &target_acc,
            |variant| env.paths(variant),
        )
        .unwrap()
    };

    // 复制建组：CN → AI（正文落在目标档同名工作区目录）。
    let report = cross_copy(
        &env,
        WbVariant::Cn,
        "uid-a",
        WbVariant::Ai,
        "uid-b",
        &["sess-1"],
    );
    let new_id = report["copied"][0]["newId"].as_str().unwrap().to_string();

    // 复制完成即一致；报告带两侧档位，标题取自源档。
    let first = preview();
    assert_eq!(first["sourceVariant"], "cn");
    assert_eq!(first["targetVariant"], "ai");
    assert_eq!(first["storeStatus"], "ready");
    assert_eq!(first["groups"][0]["verdict"], "identical");
    assert_eq!(first["groups"][0]["title"], "标题一");
    assert_eq!(first["groups"][0]["source"]["sessionId"], "sess-1");
    assert_eq!(first["groups"][0]["target"]["sessionId"], new_id.as_str());

    // 源追加 3 条 → fastForward（extraA=3），给出可执行的 previewToken。
    append_records(&env.body_path(WbVariant::Cn, "sess-1"), "sess-1", 2, 3);
    let second = preview();
    let item = &second["groups"][0];
    assert_eq!(item["verdict"], "fastForward");
    assert_eq!(item["extraA"], 3);
    assert_eq!(item["extraB"], 0);
    assert_eq!(item["defaultChecked"], true);
    assert_eq!(item["recordCount"]["source"], 5);
    assert_eq!(item["recordCount"]["target"], 2);
    assert!(item["previewToken"]
        .as_str()
        .is_some_and(|token| !token.is_empty()));

    // 目标追加自己独有的 2 条 → diverge（两边各有独有内容，只能显式覆盖）。
    append_records(&env.body_path(WbVariant::Ai, &new_id), &new_id, 100, 2);
    let third = preview();
    let item = &third["groups"][0];
    assert_eq!(item["verdict"], "diverge");
    assert_eq!(item["extraA"], 3);
    assert_eq!(item["extraB"], 2);
    assert_eq!(item["availableModes"][0], "overwrite");

    // 源行被删除（正文仍在）→ 来源只读，仍可继续判定并给出覆盖动作；
    // 否则「折叠/清理把源会话藏起来」会让整组永久无法同步。
    env.delete_session(WbVariant::Cn, "sess-1");
    let fourth = preview();
    let item = &fourth["groups"][0];
    assert_eq!(item["verdict"], "diverge");
    assert_eq!(item["availableModes"][0], "overwrite");
    assert!(item["previewToken"]
        .as_str()
        .is_some_and(|token| !token.is_empty()));

    // 目标行被删除 → 没有可写入/可展示的对象，必须 unknown 且不可执行。
    env.delete_session(WbVariant::Ai, &new_id);
    let fifth = preview();
    let item = &fifth["groups"][0];
    assert_eq!(item["verdict"], "unknown");
    assert_eq!(item["defaultChecked"], false);
    assert!(item["availableModes"].as_array().unwrap().is_empty());
    assert!(item.get("previewToken").is_none());
    assert!(
        item["reason"]
            .as_str()
            .is_some_and(|reason| reason.contains("目标账号")),
        "应明确指出目标会话失效：{}",
        item["reason"]
    );
}

/// 历史脏关联的通用对账：同账号存在正文完全一致的存活会话 → 原地换绑；
/// 找不到可证明一致的副本 → 成员置为失效，而不是继续伪装成 active。
#[test]
fn stale_link_member_relinks_or_supersedes_by_content() {
    let env = CrossEnv::new("relink-stale");
    env.create_db(WbVariant::Cn, "");
    env.create_edge_db(WbVariant::Cn);
    for (id, uid, title) in [
        ("old", "uid-a", "标题一"),
        ("keep", "uid-a", "标题一"),
        ("orphan", "uid-c", "标题二"),
        ("gone", "uid-d", "标题四"),
        ("target", "uid-b", "标题三"),
    ] {
        env.add_session(WbVariant::Cn, id, uid, title);
    }
    env.add_body(WbVariant::Cn, "old", &body_text("old"));
    env.add_body(WbVariant::Cn, "keep", &body_text("keep"));
    env.add_body(
        WbVariant::Cn,
        "orphan",
        "{\"type\":\"user\",\"sessionId\":\"orphan\",\"text\":\"only-here\"}\n",
    );

    // 模拟折叠：行软删、正文保留。
    let conn = Connection::open(env.paths(WbVariant::Cn).workbuddy_db()).unwrap();
    conn.execute(
        "UPDATE sessions SET deleted_at = 1 WHERE id IN ('old','orphan','gone')",
        [],
    )
    .unwrap();
    drop(conn);

    let paths = env.paths(WbVariant::Cn);
    session_link::with_link_store_write(&paths, |store| {
        store.groups.push(LinkGroup {
            id: "g-1".to_string(),
            variant: WbVariant::Cn,
            created_at: 1,
            members: vec![
                LinkMember {
                    member_id: "m-old".to_string(),
                    account_id: None,
                    uid: "uid-a".to_string(),
                    session_id: "old".to_string(),
                    variant: None,
                    state: MemberState::Active,
                    linked_at: 1,
                    last_synced_at: None,
                },
                LinkMember {
                    member_id: "m-orphan".to_string(),
                    account_id: None,
                    uid: "uid-c".to_string(),
                    session_id: "orphan".to_string(),
                    variant: None,
                    state: MemberState::Active,
                    linked_at: 1,
                    last_synced_at: None,
                },
                LinkMember {
                    member_id: "m-gone".to_string(),
                    account_id: None,
                    uid: "uid-d".to_string(),
                    session_id: "gone".to_string(),
                    variant: None,
                    state: MemberState::Active,
                    linked_at: 1,
                    last_synced_at: None,
                },
                LinkMember {
                    member_id: "m-target".to_string(),
                    account_id: None,
                    uid: "uid-b".to_string(),
                    session_id: "target".to_string(),
                    variant: None,
                    state: MemberState::Active,
                    linked_at: 1,
                    last_synced_at: None,
                },
            ],
            pair_bases: Vec::new(),
        });
        Ok(())
    })
    .unwrap();

    let report = reconcile_stale_link_members(&paths, WbVariant::Cn);
    assert_eq!(report.relinked, 1);
    assert_eq!(report.superseded, 1);

    let StoreState::Ready(store) = session_link::load_store(&paths) else {
        panic!("关联存储应可用");
    };
    let group = &store.groups[0];
    let old = group
        .members
        .iter()
        .find(|member| member.member_id == "m-old")
        .unwrap();
    assert_eq!(old.session_id, "keep", "内容一致应原地换绑到存活副本");
    assert_eq!(old.state, MemberState::Active);
    let orphan = group
        .members
        .iter()
        .find(|member| member.member_id == "m-orphan")
        .unwrap();
    assert_eq!(
        orphan.state,
        MemberState::Active,
        "正文仍完整时保留为只读来源，不强行失效"
    );
    let gone = group
        .members
        .iter()
        .find(|member| member.member_id == "m-gone")
        .unwrap();
    assert_eq!(
        gone.state,
        MemberState::Superseded,
        "正文不可读的成员必须明确失效"
    );
}

/// 跨档预览入口也覆盖同档组（两个国内版账号）：判定路径一致，报告不带档位字段。
#[test]
fn cross_preview_supports_same_variant_groups() {
    let env = CrossEnv::new("preview-same");
    env.create_db(WbVariant::Cn, "");
    env.create_edge_db(WbVariant::Cn);
    env.add_session(WbVariant::Cn, "sess-1", "uid-a", "标题一");
    env.add_body(WbVariant::Cn, "sess-1", &body_text("sess-1"));

    cross_copy(
        &env,
        WbVariant::Cn,
        "uid-a",
        WbVariant::Cn,
        "uid-b",
        &["sess-1"],
    );

    let paths = env.paths(WbVariant::Cn);
    let source_acc = json!({"id": "acc-uid-a", "uid": "uid-a", "variant": "cn"});
    let target_acc = json!({"id": "acc-uid-b", "uid": "uid-b", "variant": "cn"});
    let report = session_links_preview_cross_at(
        &paths,
        WbVariant::Cn,
        &source_acc,
        &paths,
        WbVariant::Cn,
        &target_acc,
        |variant| env.paths(variant),
    )
    .unwrap();
    assert!(report.get("sourceVariant").is_none());
    assert_eq!(report["groups"][0]["verdict"], "identical");
}

/// 跨档同步的公共准备：CN 源 sess-1 → AI 目标复制建组，返回（环境、源/目标路径、newId）。
fn cross_sync_env(name: &str) -> (CrossEnv, SessionPaths, SessionPaths, String) {
    let env = CrossEnv::new(name);
    env.create_db(WbVariant::Cn, "");
    env.create_edge_db(WbVariant::Cn);
    env.create_db(WbVariant::Ai, "");
    env.create_edge_db(WbVariant::Ai);
    env.add_session(WbVariant::Cn, "sess-1", "uid-a", "标题一");
    env.add_body(WbVariant::Cn, "sess-1", &body_text("sess-1"));
    let report = cross_copy(
        &env,
        WbVariant::Cn,
        "uid-a",
        WbVariant::Ai,
        "uid-b",
        &["sess-1"],
    );
    let new_id = report["copied"][0]["newId"].as_str().unwrap().to_string();
    let cn_paths = env.paths(WbVariant::Cn);
    let ai_paths = env.paths(WbVariant::Ai);
    (env, cn_paths, ai_paths, new_id)
}

/// 跨档同步（fastForward）：源追加后把新增同步到目标，判定回到 identical。
#[test]
fn cross_sync_fast_forward_catches_target_up() {
    let (env, cn_paths, ai_paths, new_id) = cross_sync_env("sync-ff");
    let source_acc = json!({"id": "acc-uid-a", "uid": "uid-a", "variant": "cn"});
    let target_acc = json!({"id": "acc-uid-b", "uid": "uid-b", "variant": "ai"});

    // 源产生 3 条新增 → 预览给 fastForward + 可执行凭据。
    append_records(&env.body_path(WbVariant::Cn, "sess-1"), "sess-1", 2, 3);
    let before = session_links_preview_cross_at(
        &cn_paths,
        WbVariant::Cn,
        &source_acc,
        &ai_paths,
        WbVariant::Ai,
        &target_acc,
        |variant| env.paths(variant),
    )
    .unwrap();
    let item = &before["groups"][0];
    assert_eq!(item["verdict"], "fastForward");
    let selection = SyncSelection {
        group_id: item["groupId"].as_str().unwrap().to_string(),
        preview_token: item["previewToken"].as_str().unwrap().to_string(),
        mode: SyncMode::FastForward,
    };

    let report = sync_sessions_cross_at(
        &cross_side(&cn_paths, WbVariant::Cn, "uid-a"),
        &cross_side(&ai_paths, WbVariant::Ai, "uid-b"),
        &[selection],
        |_| false,
        |variant| env.paths(variant),
    )
    .unwrap();
    assert!(report["errors"].as_array().unwrap().is_empty(), "{report}");
    assert_eq!(report["synced"][0]["status"], "synced");
    assert_eq!(report["synced"][0]["mode"], "fastForward");

    // 目标正文追平源（保留自己的 sessionId）；再判定为 identical。
    let source_text = std::fs::read_to_string(env.body_path(WbVariant::Cn, "sess-1")).unwrap();
    let target_text = std::fs::read_to_string(env.body_path(WbVariant::Ai, &new_id)).unwrap();
    assert_eq!(target_text, source_text.replace("sess-1", &new_id));

    let after = session_links_preview_cross_at(
        &cn_paths,
        WbVariant::Cn,
        &source_acc,
        &ai_paths,
        WbVariant::Ai,
        &target_acc,
        |variant| env.paths(variant),
    )
    .unwrap();
    assert_eq!(after["groups"][0]["verdict"], "identical");
}

/// 跨档同步（overwrite）：冲突经显式覆盖后目标 = 源（目标独有内容被替换）。
#[test]
fn cross_sync_overwrite_resolves_diverge() {
    let (env, cn_paths, ai_paths, new_id) = cross_sync_env("sync-overwrite");
    let source_acc = json!({"id": "acc-uid-a", "uid": "uid-a", "variant": "cn"});
    let target_acc = json!({"id": "acc-uid-b", "uid": "uid-b", "variant": "ai"});

    append_records(&env.body_path(WbVariant::Cn, "sess-1"), "sess-1", 2, 3);
    append_records(&env.body_path(WbVariant::Ai, &new_id), &new_id, 100, 2);

    let before = session_links_preview_cross_at(
        &cn_paths,
        WbVariant::Cn,
        &source_acc,
        &ai_paths,
        WbVariant::Ai,
        &target_acc,
        |variant| env.paths(variant),
    )
    .unwrap();
    let item = &before["groups"][0];
    assert_eq!(item["verdict"], "diverge");
    assert_eq!(item["availableModes"][0], "overwrite");
    let selection = SyncSelection {
        group_id: item["groupId"].as_str().unwrap().to_string(),
        preview_token: item["previewToken"].as_str().unwrap().to_string(),
        mode: SyncMode::Overwrite,
    };

    let report = sync_sessions_cross_at(
        &cross_side(&cn_paths, WbVariant::Cn, "uid-a"),
        &cross_side(&ai_paths, WbVariant::Ai, "uid-b"),
        &[selection],
        |_| false,
        |variant| env.paths(variant),
    )
    .unwrap();
    assert!(report["errors"].as_array().unwrap().is_empty(), "{report}");
    assert_eq!(report["synced"][0]["mode"], "overwrite");

    let source_text = std::fs::read_to_string(env.body_path(WbVariant::Cn, "sess-1")).unwrap();
    let target_text = std::fs::read_to_string(env.body_path(WbVariant::Ai, &new_id)).unwrap();
    assert_eq!(target_text, source_text.replace("sess-1", &new_id));
    assert!(
        !target_text.contains("\"index\":100"),
        "目标独有内容应被覆盖"
    );
}

/// 跨档同步：预览后源又变化 → 跳过（previewStale），目标零写入。
#[test]
fn cross_sync_skips_stale_preview() {
    let (env, cn_paths, ai_paths, new_id) = cross_sync_env("sync-stale");
    let source_acc = json!({"id": "acc-uid-a", "uid": "uid-a", "variant": "cn"});
    let target_acc = json!({"id": "acc-uid-b", "uid": "uid-b", "variant": "ai"});

    append_records(&env.body_path(WbVariant::Cn, "sess-1"), "sess-1", 2, 1);
    let before = session_links_preview_cross_at(
        &cn_paths,
        WbVariant::Cn,
        &source_acc,
        &ai_paths,
        WbVariant::Ai,
        &target_acc,
        |variant| env.paths(variant),
    )
    .unwrap();
    let item = &before["groups"][0];
    assert_eq!(item["verdict"], "fastForward");
    let selection = SyncSelection {
        group_id: item["groupId"].as_str().unwrap().to_string(),
        preview_token: item["previewToken"].as_str().unwrap().to_string(),
        mode: SyncMode::FastForward,
    };

    // 预览之后源再变化：凭据失效，执行必须跳过并保持目标不变。
    let target_before = std::fs::read_to_string(env.body_path(WbVariant::Ai, &new_id)).unwrap();
    append_records(&env.body_path(WbVariant::Cn, "sess-1"), "sess-1", 3, 1);
    let report = sync_sessions_cross_at(
        &cross_side(&cn_paths, WbVariant::Cn, "uid-a"),
        &cross_side(&ai_paths, WbVariant::Ai, "uid-b"),
        &[selection],
        |_| false,
        |variant| env.paths(variant),
    )
    .unwrap();
    assert!(report["synced"].as_array().unwrap().is_empty());
    assert_eq!(report["skipped"][0]["reasonCode"], "previewStale");
    let target_after = std::fs::read_to_string(env.body_path(WbVariant::Ai, &new_id)).unwrap();
    assert_eq!(target_after, target_before);
}

// ---------------------------------------------------------------------------
// 同步预览与执行契约（S5）
// ---------------------------------------------------------------------------

/// 追加 `count` 条有序记录（模拟用户在来源账号继续对话）。
fn append_records(path: &Path, cid: &str, from: usize, count: usize) {
    let mut text = std::fs::read_to_string(path).unwrap();
    if !text.ends_with('\n') {
        text.push('\n');
    }
    for index in from..from + count {
        text.push_str(&json!({"type": "assistant", "sessionId": cid, "index": index}).to_string());
        text.push('\n');
    }
    std::fs::write(path, &text).unwrap();
}

fn preview(env: &Env, target_uid: &str) -> Value {
    session_links_preview_at(&env.paths(), WbVariant::Cn, &env.target(target_uid)).unwrap()
}

fn selection(group_id: &str, preview_token: &str, mode: SyncMode) -> SyncSelection {
    SyncSelection {
        group_id: group_id.to_string(),
        preview_token: preview_token.to_string(),
        mode,
    }
}

fn sync(env: &Env, target_uid: &str, selections: &[SyncSelection]) -> Value {
    sync_sessions_for_switch_at(
        &env.paths(),
        WbVariant::Cn,
        &env.target(target_uid),
        selections,
        |_| false,
    )
    .unwrap()
}

/// 组内所有配对基线的快照：同步不得推进任何关联版本（含第三方成员的配对）。
fn pair_snapshot(env: &Env) -> Vec<String> {
    let mut items: Vec<String> = env
        .store()
        .groups
        .iter()
        .flat_map(|group| {
            group
                .pair_bases
                .iter()
                .map(|pair| format!("{}/{}", pair.member_ids.join("+"), pair.baseline_ref))
        })
        .collect();
    items.sort();
    items
}

/// 造一个可快进的场景：A→B 复制后来源追加 3 条记录，返回 (groupId, 预览凭据, 目标会话 id)。
fn fast_forward_scene(env: &Env) -> (String, String, String) {
    let report = copy(env, "uid-b", &["sess-1"]);
    let target_id = env.first_copy_id(&report);
    append_records(&env.body_path("sess-1"), "sess-1", 0, 3);

    let preview = preview(env, "uid-b");
    assert_eq!(preview["groups"].as_array().unwrap().len(), 1, "{preview}");
    let group = &preview["groups"][0];
    assert_eq!(group["verdict"], "fastForward", "{preview}");
    (
        group["groupId"].as_str().unwrap().to_string(),
        group["previewToken"].as_str().unwrap().to_string(),
        target_id,
    )
}

/// 预览报告 fastForward 与默认勾选；预览本身是只读的（不改正文与关联版本）。
#[test]
fn preview_reports_fast_forward_and_stays_read_only() {
    let env = ready_env("sync-preview-ff");
    let (_, _, target_id) = fast_forward_scene(&env);
    let target_body_before = std::fs::read_to_string(env.body_path(&target_id)).unwrap();
    let revision_before = env.store().revision;
    let baselines_before = env.baseline_files();

    let preview = preview(&env, "uid-b");
    assert_eq!(preview["supported"], true);
    assert_eq!(preview["storeStatus"], "ready");
    assert_eq!(preview["sourceUid"], "uid-a");
    assert_eq!(preview["targetUid"], "uid-b");
    let group = &preview["groups"][0];
    assert_eq!(group["title"], "标题一");
    assert_eq!(group["cwd"], "/ws/a");
    assert_eq!(group["verdict"], "fastForward");
    assert_eq!(group["defaultChecked"], true);
    assert_eq!(group["extraA"], 3);
    assert_eq!(group["extraB"], 0);
    assert_eq!(group["common"], 2);
    assert_eq!(group["availableModes"], json!(["fastForward"]));
    assert_eq!(group["recordCount"]["source"], 5);
    assert_eq!(group["recordCount"]["target"], 2);
    assert_eq!(group["recordCount"]["baseline"], 2);
    assert_eq!(group["source"]["uid"], "uid-a");
    assert_eq!(group["target"]["sessionId"], target_id);
    assert_eq!(group["target"]["state"], "active");
    assert!(group["previewToken"].as_str().is_some());
    assert!(group["reason"].as_str().unwrap().contains("可以直接同步"));
    // 预览是只读的：目标正文与关联版本都不变。
    assert_eq!(
        std::fs::read_to_string(env.body_path(&target_id)).unwrap(),
        target_body_before
    );
    assert_eq!(env.store().revision, revision_before);
    assert_eq!(env.baseline_files(), baselines_before);
}

/// 仅目标变化 → ahead：普通同步不可勾选，但为显式整组统一保留校验凭据。
#[test]
fn preview_reports_ahead_when_only_target_changed() {
    let env = ready_env("sync-preview-ahead");
    let report = copy(&env, "uid-b", &["sess-1"]);
    let target_id = env.first_copy_id(&report);
    append_records(&env.body_path(&target_id), &target_id, 0, 5);
    let target_body_before = std::fs::read_to_string(env.body_path(&target_id)).unwrap();
    let revision_before = env.store().revision;

    let preview = preview(&env, "uid-b");
    let group = &preview["groups"][0];
    assert_eq!(group["verdict"], "ahead");
    assert_eq!(group["defaultChecked"], false);
    assert_eq!(group["extraA"], 0);
    assert_eq!(group["extraB"], 5);
    assert_eq!(group["availableModes"], json!([]));
    assert!(
        group["previewToken"].as_str().is_some(),
        "显式整组统一需要绑定当前内容"
    );
    assert!(group["reason"]
        .as_str()
        .unwrap()
        .contains("只有目标账号新增"));

    // 预览不写目标：正文与关联版本都不变。
    assert_eq!(
        std::fs::read_to_string(env.body_path(&target_id)).unwrap(),
        target_body_before
    );
    assert_eq!(env.store().revision, revision_before);
}

#[test]
fn explicit_group_unify_can_replace_ahead_but_ordinary_overwrite_cannot() {
    let env = ready_env("sync-unify-ahead");
    let report = copy(&env, "uid-b", &["sess-1"]);
    let target_id = env.first_copy_id(&report);
    append_records(&env.body_path(&target_id), &target_id, 0, 5);
    let group = preview(&env, "uid-b")["groups"][0].clone();
    assert_eq!(group["verdict"], "ahead");
    assert_eq!(group["availableModes"], json!([]));
    let group_id = group["groupId"].as_str().unwrap();
    let token = group["previewToken"].as_str().unwrap();

    let rejected = sync(
        &env,
        "uid-b",
        &[selection(group_id, token, SyncMode::Overwrite)],
    );
    assert!(rejected["synced"].as_array().unwrap().is_empty());
    assert_eq!(rejected["errors"].as_array().unwrap().len(), 1);

    let applied = sync(
        &env,
        "uid-b",
        &[selection(group_id, token, SyncMode::UnifyOverwrite)],
    );
    assert_eq!(applied["synced"].as_array().unwrap().len(), 1, "{applied}");
    let source_text = std::fs::read_to_string(env.body_path("sess-1")).unwrap();
    let target_text = std::fs::read_to_string(env.body_path(&target_id)).unwrap();
    assert_eq!(
        session_link::normalize_jsonl(&source_text, "sess-1").unwrap(),
        session_link::normalize_jsonl(&target_text, &target_id).unwrap(),
    );
}

/// 一方有效成员缺失（失效/被替换）→ 不提供写入动作，记录数按契约给 0 而不是 null。
#[test]
fn preview_reports_invalid_member_as_unknown_without_null_record_counts() {
    let env = ready_env("sync-preview-invalid-member");
    let (group_id, _token, target_id) = fast_forward_scene(&env);
    session_link::with_link_store_write(&env.paths(), |store| {
        let group = store
            .groups
            .iter_mut()
            .find(|group| group.id == group_id)
            .expect("组必须存在");
        let member_id = session_link::active_member_for(group, "uid-b")
            .expect("目标成员原本有效")
            .member_id
            .clone();
        assert!(session_link::set_member_state(
            group,
            &member_id,
            MemberState::Stale
        ));
        Ok(())
    })
    .unwrap();

    let preview = preview(&env, "uid-b");
    let group = &preview["groups"][0];
    assert_eq!(group["verdict"], "unknown", "{preview}");
    assert_eq!(group["defaultChecked"], false);
    assert_eq!(group["availableModes"], json!([]));
    assert!(group.get("previewToken").is_none(), "不可执行的组不发凭据");
    assert!(
        group["reason"]
            .as_str()
            .unwrap()
            .contains("对应的会话已失效"),
        "{preview}"
    );
    // 契约：不可验证时 source/target 为 0、baseline 为 null（前端类型据此声明）。
    assert_eq!(
        group["recordCount"],
        json!({"source": 0, "target": 0, "baseline": null}),
        "{preview}"
    );
    // 两侧成员状态仍要展示（用户据此判断是哪一侧失效）。
    assert_eq!(group["target"]["sessionId"], target_id);
    assert_eq!(group["target"]["state"], "stale");
    assert_eq!(group["source"]["state"], "active");
}

/// 预览后来源追加 → 执行时跳过该组（previewStale），不继承用户旧选择。
#[test]
fn sync_skips_when_source_changed_after_preview() {
    let env = ready_env("sync-stale-source");
    let (group_id, token, target_id) = fast_forward_scene(&env);
    let target_body_before = std::fs::read_to_string(env.body_path(&target_id)).unwrap();

    append_records(&env.body_path("sess-1"), "sess-1", 3, 1);
    let report = sync(
        &env,
        "uid-b",
        &[selection(&group_id, &token, SyncMode::FastForward)],
    );
    assert!(report["synced"].as_array().unwrap().is_empty());
    assert!(report["errors"].as_array().unwrap().is_empty(), "{report}");
    let skipped = &report["skipped"][0];
    assert_eq!(skipped["reasonCode"], REASON_PREVIEW_STALE);
    assert!(
        skipped["message"]
            .as_str()
            .unwrap()
            .contains("来源账号的内容已变化"),
        "{skipped}"
    );
    assert_eq!(
        std::fs::read_to_string(env.body_path(&target_id)).unwrap(),
        target_body_before
    );
}

/// 预览后目标被改动 → 执行时跳过（显式覆盖也不能绕过版本校验）。
#[test]
fn sync_skips_when_target_changed_after_preview_even_with_overwrite() {
    let env = ready_env("sync-stale-target");
    let (group_id, token, target_id) = fast_forward_scene(&env);
    append_records(&env.body_path(&target_id), &target_id, 0, 1);
    let target_body_before = std::fs::read_to_string(env.body_path(&target_id)).unwrap();

    for mode in [SyncMode::FastForward, SyncMode::Overwrite] {
        let report = sync(&env, "uid-b", &[selection(&group_id, &token, mode)]);
        assert!(report["errors"].as_array().unwrap().is_empty(), "{report}");
        let skipped = &report["skipped"][0];
        assert_eq!(skipped["reasonCode"], REASON_PREVIEW_STALE, "{report}");
        assert!(
            skipped["message"]
                .as_str()
                .unwrap()
                .contains("目标账号的内容已变化"),
            "{skipped}"
        );
    }
    assert_eq!(
        std::fs::read_to_string(env.body_path(&target_id)).unwrap(),
        target_body_before
    );
}

/// 预览后换了账号 → 执行时跳过（身份由后端从登录态读取，不沿用令牌里的身份）。
#[test]
fn sync_skips_when_account_changed_after_preview() {
    let env = ready_env("sync-stale-account");
    let (group_id, token, _) = fast_forward_scene(&env);
    let revision_before = env.store().revision;

    env.set_login("uid-c");
    let report = sync(
        &env,
        "uid-b",
        &[selection(&group_id, &token, SyncMode::FastForward)],
    );
    assert!(report["errors"].as_array().unwrap().is_empty(), "{report}");
    let skipped = &report["skipped"][0];
    assert_eq!(skipped["reasonCode"], REASON_PREVIEW_STALE);
    assert!(
        skipped["message"].as_str().unwrap().contains("账号已变化"),
        "{skipped}"
    );
    assert_eq!(env.store().revision, revision_before);
}

/// 预览后关联组或基线变化 → 执行时跳过。
#[test]
fn sync_skips_when_group_or_baseline_changed_after_preview() {
    // 组变化：同组新增成员（例如并发复制把第三方账号加进来）。
    let env = ready_env("sync-stale-group");
    let (group_id, token, _) = fast_forward_scene(&env);
    let paths = env.paths();
    session_link::with_link_store_write(&paths, |store| {
        let group = store
            .groups
            .iter_mut()
            .find(|group| group.id == group_id)
            .expect("组必须存在");
        session_link::add_active_member(
            group,
            LinkMember {
                member_id: "m-uid-c".to_string(),
                account_id: None,
                uid: "uid-c".to_string(),
                session_id: "sess-c".to_string(),
                variant: None,
                state: MemberState::Active,
                linked_at: 1,
                last_synced_at: None,
            },
        );
        Ok(())
    })
    .unwrap();
    let report = sync(
        &env,
        "uid-b",
        &[selection(&group_id, &token, SyncMode::FastForward)],
    );
    let skipped = &report["skipped"][0];
    assert_eq!(skipped["reasonCode"], REASON_PREVIEW_STALE, "{report}");
    assert!(
        skipped["message"]
            .as_str()
            .unwrap()
            .contains("会话的关联关系或同步记录已变化"),
        "{skipped}"
    );

    // 基线变化：同一个基线引用被改写成另一份内容。
    let env = ready_env("sync-stale-baseline");
    let (group_id, token, _) = fast_forward_scene(&env);
    let baseline_ref = env.store().groups[0].pair_bases[0].baseline_ref.clone();
    let drifted = session_link::BaselineRecord {
        version: session_link::BASELINE_VERSION,
        baseline_ref: baseline_ref.clone(),
        normalization_version: session_link::NORMALIZATION_VERSION,
        created_at: 1,
        record_count: 1,
        total_digest: session_link::total_digest_of(&["00".to_string()]),
        line_digests: vec!["00".to_string()],
    };
    std::fs::write(
        env.paths
            .baselines_dir()
            .join(format!("{baseline_ref}.json")),
        serde_json::to_string(&drifted).unwrap(),
    )
    .unwrap();
    let report = sync(
        &env,
        "uid-b",
        &[selection(&group_id, &token, SyncMode::FastForward)],
    );
    let skipped = &report["skipped"][0];
    assert_eq!(skipped["reasonCode"], REASON_PREVIEW_STALE, "{report}");
    assert!(
        skipped["message"]
            .as_str()
            .unwrap()
            .contains("上次同步的内容已变化"),
        "{skipped}"
    );
}

/// 伪造 token / 张冠李戴的组 → 拒绝，不静默执行。
#[test]
fn sync_rejects_forged_or_mismatched_preview_token() {
    let env = ready_env("sync-forged");
    let (group_id, token, target_id) = fast_forward_scene(&env);

    for forged in [
        "11111111-2222-3333-4444-555555555555",
        "../../../../etc/passwd",
        "sess-1.json",
    ] {
        let report = sync(
            &env,
            "uid-b",
            &[selection(&group_id, forged, SyncMode::FastForward)],
        );
        assert!(report["skipped"].as_array().unwrap().is_empty(), "{report}");
        let errors = report["errors"].as_array().unwrap();
        assert_eq!(errors.len(), 1, "{report}");
        assert!(
            errors[0]["error"]
                .as_str()
                .unwrap()
                .contains("检查结果不存在"),
            "{report}"
        );
    }

    // 真实凭据 + 别的组 id：张冠李戴同样拒绝。
    let report = sync(
        &env,
        "uid-b",
        &[selection("g-other", &token, SyncMode::FastForward)],
    );
    assert!(report["skipped"].as_array().unwrap().is_empty(), "{report}");
    assert!(
        report["errors"][0]["error"]
            .as_str()
            .unwrap()
            .contains("不匹配"),
        "{report}"
    );

    // 未知模式在解析阶段就拒绝。
    assert!(parse_sync_selections(Some(
        &json!([{"groupId": group_id, "previewToken": token, "mode": "force"}])
    ))
    .unwrap_err()
    .contains("未知的同步模式"));

    // 全程没有任何写入。
    assert_eq!(env.body_files().len(), 2);
    assert_eq!(
        session_row_owner(&env.paths(), &target_id).as_deref(),
        Some("uid-b")
    );
}

/// 判定为 unknown 时强制覆盖 → 拒绝（不得绕过版本校验）。
#[test]
fn sync_rejects_forced_overwrite_when_verdict_is_unknown() {
    let env = ready_env("sync-unknown-overwrite");
    let (group_id, token, target_id) = fast_forward_scene(&env);
    // 目标加入独有内容：双方互不为前缀，删掉基线后无法用内容关系判定 → unknown。
    append_records(&env.body_path(&target_id), &target_id, 100, 2);
    let target_body_before = std::fs::read_to_string(env.body_path(&target_id)).unwrap();
    let revision_before = env.store().revision;

    // 基线文件被删除：共同基线不可验证 → 重新判定必然 unknown。
    let store = env.store();
    let group = store
        .groups
        .iter()
        .find(|group| group.id == group_id)
        .unwrap();
    let baseline_ref = group.pair_bases[0].baseline_ref.clone();
    std::fs::remove_file(
        env.paths
            .baselines_dir()
            .join(format!("{baseline_ref}.json")),
    )
    .unwrap();

    let paths = env.paths();
    let source_member = session_link::active_member_for(group, "uid-a").unwrap();
    let target_member = session_link::active_member_for(group, "uid-b").unwrap();
    let source_content = member_content_state(&paths, &source_member.session_id);
    let target_content = member_content_state(&paths, &target_member.session_id);
    let baseline = session_link::load_pair_baseline(
        &paths,
        group,
        &source_member.member_id,
        &target_member.member_id,
    );
    assert_eq!(
        session_link::decide_sync(&source_content, &target_content, &baseline).verdict,
        SyncVerdict::Unknown
    );
    // 前端只能回传凭据 id；这里直接构造一份「unknown + 空可选模式」的服务端凭据，
    // 模拟强行以覆盖模式执行。
    let forged = session_link::save_preview_token(
        &paths,
        live_preview_binding(
            group,
            source_member,
            target_member,
            &source_content,
            &target_content,
            &baseline,
            SyncVerdict::Unknown,
            &PreviewArchiveState::default(),
        ),
    )
    .unwrap();

    for mode in [SyncMode::Overwrite, SyncMode::FastForward] {
        let report = sync(&env, "uid-b", &[selection(&group_id, &forged, mode)]);
        assert!(
            report["skipped"].as_array().unwrap().is_empty(),
            "unknown 不得进入校验通过：{report}"
        );
        let errors = report["errors"].as_array().unwrap();
        assert_eq!(errors.len(), 1, "{report}");
        assert!(
            errors[0]["error"].as_str().unwrap().contains("不允许以"),
            "{report}"
        );
    }

    // 以原凭据 + 覆盖模式 → 版本校验先拦下（判定已变），同样不写。
    let report = sync(
        &env,
        "uid-b",
        &[selection(&group_id, &token, SyncMode::Overwrite)],
    );
    assert_eq!(report["skipped"][0]["reasonCode"], REASON_PREVIEW_STALE);

    assert_eq!(
        std::fs::read_to_string(env.body_path(&target_id)).unwrap(),
        target_body_before
    );
    assert_eq!(env.store().revision, revision_before);
}

/// 无配对基线时，目标内容被来源完整包含 → 预览照常发放可勾选凭据，执行成功。
///
/// 覆盖轮换链的最后一跳（C 切回 A）：隔跳配对没有基线记录，但目标内容是来源内容的
/// 严格有序前缀，追加同步零覆盖，因此无需基线佐证（对齐 git fast-forward）。
#[test]
fn sync_fast_forward_without_baseline_when_target_is_ordered_prefix() {
    let env = ready_env("sync-ff-no-baseline");
    let (group_id, _, target_id) = fast_forward_scene(&env);
    let target_body_before = body_bytes(&env, &target_id);

    // 删除基线文件：等价于「隔跳配对从未登记过共同基线」的不可验证状态。
    let group = group_snapshot(&env, &group_id);
    let baseline_ref = group.pair_bases[0].baseline_ref.clone();
    std::fs::remove_file(
        env.paths
            .baselines_dir()
            .join(format!("{baseline_ref}.json")),
    )
    .unwrap();

    // 预览：没有可验证基线也照常判快进，并发放可勾选凭据。
    let preview = preview(&env, "uid-b");
    let item = &preview["groups"][0];
    assert_eq!(item["verdict"], "fastForward", "{preview}");
    assert_eq!(item["defaultChecked"], true, "{preview}");
    assert_eq!(item["availableModes"], json!(["fastForward"]), "{preview}");
    assert!(item["recordCount"]["baseline"].is_null(), "{preview}");
    assert!(
        item["reason"].as_str().unwrap().contains("新增 3 条"),
        "{preview}"
    );
    let token = item["previewToken"]
        .as_str()
        .expect("可勾选项必须发放凭据")
        .to_string();

    // 执行：无基线不阻塞写入，目标收敛到来源内容，并补建配对基线。
    let report = sync(
        &env,
        "uid-b",
        &[selection(&group_id, &token, SyncMode::FastForward)],
    );
    assert!(report["errors"].as_array().unwrap().is_empty(), "{report}");
    assert!(report["skipped"].as_array().unwrap().is_empty(), "{report}");
    assert_eq!(report["synced"].as_array().unwrap().len(), 1, "{report}");
    assert_ne!(body_bytes(&env, &target_id), target_body_before);
    assert_eq!(
        std::fs::read_to_string(env.body_path(&target_id)).unwrap(),
        incoming_text(&env, "sess-1", &target_id)
    );
    let group = group_snapshot(&env, &group_id);
    let baseline = session_link::load_pair_baseline(
        &env.paths(),
        &group,
        &member_id_of(&group, "uid-a"),
        &member_id_of(&group, "uid-b"),
    );
    assert!(baseline.ready().is_some(), "执行后必须补建可验证的配对基线");
}

/// 第三方账号：只处理 A 与 B 共同参与的组，同步只推进 A/B 的基线与正文。
#[test]
fn sync_never_touches_third_account_member_or_baseline() {
    let env = ready_env("sync-third-account");
    let a_to_b = copy(&env, "uid-b", &["sess-1"]);
    let b_id = env.first_copy_id(&a_to_b);
    let a_to_c = copy(&env, "uid-c", &["sess-1"]);
    let c_id = env.first_copy_id(&a_to_c);
    append_records(&env.body_path("sess-1"), "sess-1", 0, 2);
    append_records(&env.body_path(&c_id), &c_id, 0, 7);

    let pairs_before = pair_snapshot(&env);
    let baselines_before = env.baseline_files();
    let c_body_before = std::fs::read_to_string(env.body_path(&c_id)).unwrap();
    let b_body_before = std::fs::read_to_string(env.body_path(&b_id)).unwrap();

    // 只列出 A 与 B 共同参与的组；C 的改动不影响 A/B 的判定。
    let preview = preview(&env, "uid-b");
    let groups = preview["groups"].as_array().unwrap();
    assert_eq!(groups.len(), 1, "{preview}");
    assert_eq!(groups[0]["verdict"], "fastForward", "{preview}");
    assert_eq!(groups[0]["extraB"], 0);
    let group_id = groups[0]["groupId"].as_str().unwrap().to_string();
    let token = groups[0]["previewToken"].as_str().unwrap().to_string();

    let report = sync(
        &env,
        "uid-b",
        &[selection(&group_id, &token, SyncMode::FastForward)],
    );
    assert_eq!(report["synced"].as_array().unwrap().len(), 1, "{report}");
    assert!(report["errors"].as_array().unwrap().is_empty(), "{report}");
    assert!(report.get("needsRecovery").is_none(), "{report}");

    // 只有 A/B 的配对基线被推进（旧引用消失、新引用出现），A/C 与 B/C 原样保留。
    let store = env.store();
    let group = store
        .groups
        .iter()
        .find(|group| group.id == group_id)
        .unwrap();
    let pairs_after = pair_snapshot(&env);
    let removed: Vec<&String> = pairs_before
        .iter()
        .filter(|entry| !pairs_after.contains(entry))
        .collect();
    let added: Vec<&String> = pairs_after
        .iter()
        .filter(|entry| !pairs_before.contains(entry))
        .collect();
    assert_eq!(removed.len(), 1, "只有 A/B 的基线被改写：{pairs_after:?}");
    let (a_member, b_member) = (member_id_of(group, "uid-a"), member_id_of(group, "uid-b"));
    assert!(
        removed[0].starts_with(&format!("{a_member}+{b_member}"))
            || removed[0].starts_with(&format!("{b_member}+{a_member}")),
        "被改写的必须是 A/B 成员对：{}",
        removed[0]
    );
    assert_eq!(added.len(), 1);
    assert_eq!(env.baseline_files(), baselines_before + 1, "只新增一份基线");

    // C 的正文、基线与同步时间都不动；来源账号也不被标成已同步。
    assert_eq!(
        std::fs::read_to_string(env.body_path(&c_id)).unwrap(),
        c_body_before,
        "C 的内容不得被同步改写"
    );
    assert_ne!(
        std::fs::read_to_string(env.body_path(&b_id)).unwrap(),
        b_body_before,
        "B 的内容应被同步替换"
    );
    let group = env
        .store()
        .groups
        .into_iter()
        .find(|group| group.id == group_id)
        .unwrap();
    assert!(group
        .members
        .iter()
        .find(|member| member.uid == "uid-c")
        .unwrap()
        .last_synced_at
        .is_none());
    assert!(session_link::active_member_for(&group, "uid-a")
        .unwrap()
        .last_synced_at
        .is_none());
    assert!(session_link::active_member_for(&group, "uid-b")
        .unwrap()
        .last_synced_at
        .is_some());
}

fn member_id_of(group: &LinkGroup, uid: &str) -> String {
    group
        .members
        .iter()
        .find(|member| member.uid == uid)
        .unwrap()
        .member_id
        .clone()
}

/// 关联存储不可用/缺失 → 不把校验当成通过。
#[test]
fn sync_reports_store_problems_instead_of_passing_checks() {
    // 损坏：保留现场，全部选择项报错并要求恢复。
    let env = ready_env("sync-store-broken");
    let (group_id, token, _) = fast_forward_scene(&env);
    std::fs::write(env.paths.session_links_file(), "not-json").unwrap();
    let report = sync(
        &env,
        "uid-b",
        &[selection(&group_id, &token, SyncMode::FastForward)],
    );
    assert!(report["skipped"].as_array().unwrap().is_empty(), "{report}");
    assert!(
        report["errors"][0]["error"]
            .as_str()
            .unwrap()
            .contains("同步记录"),
        "{report}"
    );
    assert_eq!(report["needsRecovery"], true, "关系表损坏必须要求恢复");

    // 主文件与基线都被清掉（首次使用）：凭据无从校验 → 跳过。
    let env = ready_env("sync-store-missing");
    let (group_id, token, target_id) = fast_forward_scene(&env);
    std::fs::remove_file(env.paths.session_links_file()).unwrap();
    std::fs::remove_dir_all(env.paths.baselines_dir()).unwrap();
    let report = sync(
        &env,
        "uid-b",
        &[selection(&group_id, &token, SyncMode::FastForward)],
    );
    assert!(report["errors"].as_array().unwrap().is_empty(), "{report}");
    assert_eq!(report["skipped"][0]["reasonCode"], REASON_PREVIEW_STALE);
    assert_eq!(env.body_files().len(), 2);
    assert!(env.body_path(&target_id).exists());
}

/// 预览的能力与状态上报：档位不支持、关联存储未初始化、入参错误。
#[test]
fn preview_reports_supported_and_store_status() {
    let env = Env::new("sync-preview-status");
    env.set_login("uid-a");

    // 关联存储还没建立 → missing，不是错误。
    let report = preview(&env, "uid-b");
    assert_eq!(report["supported"], true);
    assert_eq!(report["storeStatus"], "missing");
    assert_eq!(report["groups"].as_array().unwrap().len(), 0);

    // 国际版数据根不同构 → 不支持，前端据此隐藏同步区块。
    let ai_paths = SessionPaths {
        store_root: env.root.join("store-ai"),
        data_root: env.root.join("data-ai"),
        auth_file: env.paths.auth_file.clone(),
        link_namespace: LinkNamespace::WorkBuddy,
    };
    std::fs::create_dir_all(&ai_paths.data_root).unwrap();
    let report = session_links_preview_at(
        &ai_paths,
        WbVariant::Ai,
        &json!({"id": "ai-1", "uid": "uid-b", "variant": "ai"}),
    )
    .unwrap();
    assert_eq!(report["supported"], false);
    assert_eq!(report["storeStatus"], "unsupported");
    assert_eq!(report["groups"].as_array().unwrap().len(), 0);

    // 单对预览也必须遵守同一个能力门禁，不能因为只算一组就放行不可写预览。
    let pair = session_links_preview_pair_at(
        &ai_paths,
        WbVariant::Ai,
        &json!({"uid": "uid-a", "variant": "ai"}),
        &ai_paths,
        WbVariant::Ai,
        &json!({"uid": "uid-b", "variant": "ai"}),
        "any-group",
    )
    .unwrap();
    assert_eq!(pair["supported"], false);
    assert_eq!(pair["storeStatus"], "unsupported");
    assert_eq!(pair["groups"].as_array().unwrap().len(), 0);

    // 入参错误：缺 uid、同账号。
    assert!(
        session_links_preview_at(&env.paths(), WbVariant::Cn, &json!({"uid": " "}))
            .unwrap_err()
            .contains("缺少 uid")
    );
    assert!(
        session_links_preview_at(&env.paths(), WbVariant::Cn, &env.target("uid-a"))
            .unwrap_err()
            .contains("当前账号与目标账号相同")
    );
}

/// 执行入口的生命周期与入参保护。
#[test]
fn sync_requires_app_stopped_and_valid_target() {
    let env = ready_env("sync-lifecycle");
    let (group_id, token, target_id) = fast_forward_scene(&env);
    let target_body_before = std::fs::read_to_string(env.body_path(&target_id)).unwrap();
    let one = [selection(&group_id, &token, SyncMode::FastForward)];

    // 目标账号不是当前登录账号（auth=uid-a）：App 运行中不再拦截，正常同步（S5 实测口径）。
    let report = sync_sessions_for_switch_at(
        &env.paths(),
        WbVariant::Cn,
        &env.target("uid-b"),
        &one,
        |_| true,
    )
    .unwrap();
    assert_eq!(report["synced"].as_array().unwrap().len(), 1);
    let target_body_after = std::fs::read_to_string(env.body_path(&target_id)).unwrap();
    assert_ne!(target_body_before, target_body_after, "目标正文已追平");

    // 空选择项 → 什么都不做，也不要求关闭 App。
    let report = sync_sessions_for_switch_at(
        &env.paths(),
        WbVariant::Cn,
        &env.target("uid-b"),
        &[],
        |_| true,
    )
    .unwrap();
    assert!(report["skipped"].as_array().unwrap().is_empty());
    assert!(report["errors"].as_array().unwrap().is_empty());
    assert!(report["synced"].as_array().unwrap().is_empty());

    // 缺 uid / 同账号。
    let err = sync_sessions_for_switch_at(
        &env.paths(),
        WbVariant::Cn,
        &json!({"uid": "  "}),
        &one,
        |_| false,
    )
    .unwrap_err();
    assert_eq!(err, "目标账号缺少 uid，无法同步会话");
    let err = sync_sessions_for_switch_at(
        &env.paths(),
        WbVariant::Cn,
        &env.target("uid-a"),
        &one,
        |_| false,
    )
    .unwrap_err();
    assert!(err.contains("当前账号与目标账号相同"), "{err}");

    // 失败的两条（缺 uid / 同账号）都不应再触碰目标正文。
    assert_eq!(
        std::fs::read_to_string(env.body_path(&target_id)).unwrap(),
        target_body_after
    );
}

/// syncSelections 解析：缺字段/未知模式/非数组一律拒绝。
#[test]
fn sync_selection_parsing_rejects_invalid_input() {
    assert!(parse_sync_selections(None).unwrap().is_empty());
    assert!(parse_sync_selections(Some(&json!(null)))
        .unwrap()
        .is_empty());
    assert!(parse_sync_selections(Some(&json!("x")))
        .unwrap_err()
        .contains("必须是数组"));

    let parsed = parse_sync_selections(Some(&json!([
            {"groupId": "g-1", "previewToken": "11111111-2222-3333-4444-555555555555", "mode": "overwrite"}
        ])))
        .unwrap();
    assert_eq!(parsed.len(), 1);
    assert_eq!(parsed[0].group_id, "g-1");
    assert_eq!(parsed[0].mode, SyncMode::Overwrite);

    for (name, item) in [
        (
            "缺 groupId",
            json!({"previewToken": "t", "mode": "fastForward"}),
        ),
        (
            "缺 previewToken",
            json!({"groupId": "g-1", "mode": "fastForward"}),
        ),
        ("缺 mode", json!({"groupId": "g-1", "previewToken": "t"})),
        (
            "空白 groupId",
            json!({"groupId": "  ", "previewToken": "t", "mode": "fastForward"}),
        ),
    ] {
        let error = parse_sync_selections(Some(&json!([item]))).unwrap_err();
        assert!(!error.is_empty(), "{name}");
    }
}

// ---------------------------------------------------------------------------
// 备份、写入与中断恢复（S6）
// ---------------------------------------------------------------------------

/// 目标会话行的可观测状态（用户 id、标题、自定义标题、更新时间、删除时间）。
type RowView = (
    String,
    Option<String>,
    Option<String>,
    Option<i64>,
    Option<i64>,
);

fn try_session_row(env: &Env, cid: &str) -> Option<RowView> {
    let conn = Connection::open(env.paths.workbuddy_db()).unwrap();
    conn.query_row(
        "SELECT user_id, title, custom_title, updated_at, deleted_at FROM sessions WHERE id = ?1",
        [cid],
        |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
            ))
        },
    )
    .ok()
}

fn session_row(env: &Env, cid: &str) -> RowView {
    try_session_row(env, cid).expect("会话记录必须存在")
}

/// 改标题与自定义标题（模拟用户在目标账号改名；标题不是内容身份，不使预览失效）。
fn set_row_meta(env: &Env, cid: &str, title: &str, custom_title: Option<&str>) {
    let conn = Connection::open(env.paths.workbuddy_db()).unwrap();
    conn.execute(
        "UPDATE sessions SET title = ?1, custom_title = ?2 WHERE id = ?3",
        rusqlite::params![title, custom_title, cid],
    )
    .unwrap();
}

fn group_snapshot(env: &Env, group_id: &str) -> LinkGroup {
    env.store()
        .groups
        .into_iter()
        .find(|group| group.id == group_id)
        .expect("同步关系必须存在")
}

fn pair_ref_of(group: &LinkGroup, left_uid: &str, right_uid: &str) -> String {
    session_link::find_pair_base(
        group,
        &member_id_of(group, left_uid),
        &member_id_of(group, right_uid),
    )
    .expect("成员对基线必须存在")
    .baseline_ref
    .clone()
}

/// 同步后目标正文的预期内容：来源正文 + 目标 sessionId。
fn incoming_text(env: &Env, source_id: &str, target_id: &str) -> String {
    std::fs::read_to_string(env.body_path(source_id))
        .unwrap()
        .replace(source_id, target_id)
}

/// 让 `sessions` 的 UPDATE 在事务内失败（模拟数据库写入阶段中断）。
fn install_block_update_trigger(env: &Env) {
    let conn = Connection::open(env.paths.workbuddy_db()).unwrap();
    conn.execute_batch(
        "CREATE TRIGGER block_sync_update BEFORE UPDATE ON sessions
             BEGIN SELECT RAISE(ABORT, 'sessions 更新被测试拦截'); END;",
    )
    .unwrap();
}

fn drop_block_update_trigger(env: &Env) {
    let conn = Connection::open(env.paths.workbuddy_db()).unwrap();
    conn.execute_batch("DROP TRIGGER block_sync_update;")
        .unwrap();
}

fn body_bytes(env: &Env, cid: &str) -> Vec<u8> {
    std::fs::read(env.body_path(cid)).unwrap()
}

fn sync_operations(env: &Env) -> Vec<Operation> {
    session_link::scan_operations(&env.paths)
        .operations
        .into_iter()
        .filter(|operation| operation.kind == OPERATION_KIND_SYNC)
        .collect()
}

/// 成功路径：正文替换为来源内容，SID/标题/custom_title/归属保留，updated_at 更新，
/// A/B 基线推进、目标成员 lastSyncedAt 更新、不触碰映射库、备份可查看。
#[test]
fn sync_fast_forward_replaces_body_and_advances_pair_baseline() {
    let env = ready_env("sync-ff-write");
    let (group_id, token, target_id) = fast_forward_scene(&env);
    let source_body = std::fs::read_to_string(env.body_path("sess-1")).unwrap();
    let source_digest_before = full_digest_of(source_body.as_bytes());
    let target_body_before = body_bytes(&env, &target_id);
    // 目标账号已改过名：同步必须原样保留标题与自定义标题。
    set_row_meta(&env, &target_id, "改名后的标题", Some("自定义名"));
    let row_before = session_row(&env, &target_id);
    let pair_ref_before = pair_ref_of(&group_snapshot(&env, &group_id), "uid-a", "uid-b");
    let baselines_before = env.baseline_files();
    let edge_db_before = std::fs::read(env.paths.edge_sync_db(WbVariant::Cn)).unwrap();

    let report = sync(
        &env,
        "uid-b",
        &[selection(&group_id, &token, SyncMode::FastForward)],
    );
    assert!(report["errors"].as_array().unwrap().is_empty(), "{report}");
    assert!(report["skipped"].as_array().unwrap().is_empty(), "{report}");
    assert!(report.get("needsRecovery").is_none(), "{report}");
    let synced = report["synced"].as_array().unwrap();
    assert_eq!(synced.len(), 1, "{report}");
    assert_eq!(synced[0]["status"], "synced");
    assert_eq!(synced[0]["groupId"], group_id);
    assert_eq!(synced[0]["mode"], "fastForward");
    assert_eq!(synced[0]["verdict"], "fastForward");
    assert_eq!(synced[0]["sourceSessionId"], "sess-1");
    assert_eq!(synced[0]["targetSessionId"], target_id);
    assert_eq!(synced[0]["recordCount"]["source"], 5);
    assert_eq!(synced[0]["recordCount"]["targetBefore"], 2);
    assert_eq!(synced[0]["recordCount"]["target"], 5);

    // 成功清理：不展示可还原路径，本次临时目录与维护记录都已回收。
    assert!(synced[0]["backup"].is_null(), "{report}");
    assert!(synced[0]["backupManifest"].is_null(), "{report}");
    assert_eq!(synced[0]["cleanupState"], "cleaned", "{report}");
    assert_ne!(body_bytes(&env, &target_id), target_body_before);
    let operations = sync_operations(&env);
    assert_eq!(operations.len(), 1);
    let operation_id = operations[0].operation_id.clone();
    assert!(
        !session_backup::transaction_dir(&env.paths, WbVariant::Cn, &operation_id)
            .unwrap()
            .exists(),
        "成功路径必须回收本次操作专属目录"
    );
    assert!(
        session_backup::scan_lifecycle(&env.paths)
            .records
            .is_empty(),
        "成功路径不得残留维护记录"
    );
    assert_eq!(
        operations[0].cleanup_state.as_deref(),
        Some(session_backup::CLEANUP_STATE_CLEANED),
        "业务日志标注备份已清理"
    );
    assert!(
        operations[0].backup.is_none(),
        "已清理的操作不再展示备份位置"
    );

    // 目标正文被替换为来源内容（本副本 sessionId 换成目标 id）；来源正文不动。
    let expected = incoming_text(&env, "sess-1", &target_id);
    assert_eq!(
        std::fs::read_to_string(env.body_path(&target_id)).unwrap(),
        expected
    );
    assert_eq!(
        full_digest_of(std::fs::read(env.body_path("sess-1")).unwrap().as_slice()),
        source_digest_before
    );

    // 数据库：只更新 updated_at，SID/归属/标题/custom_title 原样保留。
    let row_after = session_row(&env, &target_id);
    assert_eq!(row_after.0, row_before.0);
    assert_eq!(row_after.1, row_before.1);
    assert_eq!(row_after.2, row_before.2);
    assert_eq!(row_after.3, Some(synced[0]["updatedAt"].as_i64().unwrap()));
    assert!(row_after.3.unwrap() > row_before.3.unwrap());
    assert_eq!(row_after.4, None);

    // 组表：A/B 基线推进到本次写入内容，目标成员 lastSyncedAt 更新。
    let incoming_normalized =
        session_link::normalize_jsonl(&expected, &target_id).expect("内容可归一化");
    let group = group_snapshot(&env, &group_id);
    let pair_ref_after = pair_ref_of(&group, "uid-a", "uid-b");
    assert_ne!(pair_ref_after, pair_ref_before);
    assert_eq!(env.baseline_files(), baselines_before + 1);
    let baseline = session_link::load_baseline(&env.paths, &pair_ref_after).unwrap();
    assert_eq!(baseline.total_digest, incoming_normalized.total_digest);
    assert_eq!(baseline.record_count, 5);
    let last_synced = session_link::active_member_for(&group, "uid-b")
        .unwrap()
        .last_synced_at
        .expect("目标成员必须记录本次同步时间");
    assert!(last_synced >= row_after.3.unwrap());
    assert!(session_link::active_member_for(&group, "uid-a")
        .unwrap()
        .last_synced_at
        .is_none());

    // 不修改 edge-sync-mapping 库；不产生第二个副本；操作日志只有一条且已完成。
    assert_eq!(
        std::fs::read(env.paths.edge_sync_db(WbVariant::Cn)).unwrap(),
        edge_db_before
    );
    assert_eq!(env.body_files().len(), 2);
    assert_eq!(operations[0].phase, OpPhase::Completed);
    assert_eq!(operations[0].target.session_id, target_id);
    assert!(session_link::pending_operations(&env.paths, WbVariant::Cn).is_empty());
}

/// 故障状态下的备份必须保持完整可核验：清单、一致性快照、覆盖前/待写入正文与摘要
/// 齐全，位于本版专属临时目录根，且维护记录仍能追踪（design §8：安全断言保留在
/// 准备/故障阶段验证，而不是在成功路径上）。
#[test]
fn pending_sync_backup_stays_verifiable_until_recovery() {
    let env = ready_env("sync-backup-verify-on-failure");
    let (group_id, token, target_id) = fast_forward_scene(&env);
    set_row_meta(&env, &target_id, "改名后的标题", Some("自定义名"));
    let row_before = session_row(&env, &target_id);
    let target_body_before = body_bytes(&env, &target_id);

    install_block_update_trigger(&env);
    let report = sync(
        &env,
        "uid-b",
        &[selection(&group_id, &token, SyncMode::FastForward)],
    );
    assert_eq!(report["needsRecovery"], true, "{report}");
    drop_block_update_trigger(&env);

    let operation = session_link::pending_operations(&env.paths, WbVariant::Cn)
        .into_iter()
        .next()
        .expect("故障后必须保留未完成操作");
    let manifest_file = PathBuf::from(operation.backup.clone().expect("必须记录备份位置"));
    let backup_dir = manifest_file.parent().unwrap().to_path_buf();
    assert_eq!(
        manifest_file,
        backup_dir.join("manifest.json"),
        "清单是恢复的唯一依据"
    );
    let manifest = load_sync_manifest(&backup_dir).expect("备份清单必须可读");
    assert_eq!(
        backup_dir.file_name().unwrap().to_string_lossy(),
        manifest.operation_id,
        "备份目录按 operationId 唯一"
    );
    assert_eq!(manifest.group_id, group_id);
    assert_eq!(manifest.variant, WbVariant::Cn);
    assert_eq!(manifest.mode, SyncMode::FastForward);
    assert_eq!(manifest.verdict, SyncVerdict::FastForward);
    assert_eq!(manifest.source.uid, "uid-a");
    assert_eq!(manifest.source.session_id, "sess-1");
    assert_eq!(manifest.source.body_file, None);
    assert_eq!(manifest.target.uid, "uid-b");
    assert_eq!(manifest.target.session_id, target_id);
    assert_eq!(
        manifest.target_body_path,
        env.body_path(&target_id).to_string_lossy()
    );
    assert!(backup_dir.join(&manifest.db.snapshot_file).is_file());
    assert_eq!(manifest.db.method, DB_SNAPSHOT_METHOD);
    let before_row = manifest.db.target_row.as_ref().expect("必须记录目标行");
    assert_eq!(before_row.session_id, target_id);
    assert_eq!(before_row.user_id, "uid-b");
    assert_eq!(before_row.title.as_deref(), Some("改名后的标题"));
    assert_eq!(before_row.custom_title.as_deref(), Some("自定义名"));
    assert_eq!(before_row.updated_at, row_before.3);
    let original_backup = backup_dir.join(manifest.target.body_file.clone().unwrap());
    assert_eq!(std::fs::read(&original_backup).unwrap(), target_body_before);
    assert_eq!(
        full_digest_of(&std::fs::read(&original_backup).unwrap()),
        manifest.target.body_raw_digest
    );
    assert_eq!(manifest.target.record_count, 2);
    assert_eq!(manifest.incoming.record_count, 5);
    assert!(manifest.restore_steps.len() >= 4);
    verify_sync_backup(&backup_dir, &manifest).expect("备份必须可核验");
    // 备份位于本版专属临时目录根，维护记录仍能追踪同一 operationId。
    assert!(backup_dir.starts_with(
        env.paths
            .backup_root()
            .join(session_backup::TRANSACTIONS_DIR_NAME)
    ));
    assert!(session_backup::scan_lifecycle(&env.paths)
        .records
        .iter()
        .any(|record| record.operation_id == manifest.operation_id));
}

/// 显式覆盖：目标全文被替换为来源内容（目标独有记录不再保留），仍然保留 SID/标题。
#[test]
fn sync_overwrite_replaces_target_full_text() {
    let env = ready_env("sync-overwrite");
    let report = copy(&env, "uid-b", &["sess-1"]);
    let target_id = env.first_copy_id(&report);
    // 双方都变化：来源追加 2 条，目标也追加 3 条 → diverge。
    append_records(&env.body_path("sess-1"), "sess-1", 0, 2);
    append_records(&env.body_path(&target_id), &target_id, 10, 3);
    set_row_meta(&env, &target_id, "目标标题", Some("目标自定义名"));
    let row_before = session_row(&env, &target_id);

    let preview = preview(&env, "uid-b");
    let group = &preview["groups"][0];
    assert_eq!(group["verdict"], "diverge", "{preview}");
    assert_eq!(group["defaultChecked"], false);
    assert_eq!(group["availableModes"], json!(["overwrite"]));
    assert_eq!(group["extraB"], 3);
    let group_id = group["groupId"].as_str().unwrap().to_string();
    let token = group["previewToken"].as_str().unwrap().to_string();

    let report = sync(
        &env,
        "uid-b",
        &[selection(&group_id, &token, SyncMode::Overwrite)],
    );
    assert_eq!(report["synced"].as_array().unwrap().len(), 1, "{report}");
    assert!(report["errors"].as_array().unwrap().is_empty(), "{report}");
    let expected = incoming_text(&env, "sess-1", &target_id);
    assert_eq!(
        std::fs::read_to_string(env.body_path(&target_id)).unwrap(),
        expected,
        "覆盖必须替换目标全文"
    );
    let row_after = session_row(&env, &target_id);
    assert_eq!(row_after.1.as_deref(), Some("目标标题"));
    assert_eq!(row_after.2.as_deref(), Some("目标自定义名"));
    assert_ne!(row_after.3, row_before.3);
}

/// 双方一致（identical）不写正文：既不发凭据，强行执行也会被拒绝。
#[test]
fn sync_identical_verdict_never_writes_body() {
    let env = ready_env("sync-identical");
    let report = copy(&env, "uid-b", &["sess-1"]);
    let target_id = env.first_copy_id(&report);
    let target_before = body_bytes(&env, &target_id);
    let revision_before = env.store().revision;

    let preview = preview(&env, "uid-b");
    let group = &preview["groups"][0];
    assert_eq!(group["verdict"], "identical", "{preview}");
    assert_eq!(group["defaultChecked"], false);
    assert_eq!(group["availableModes"], json!([]));
    assert!(group.get("previewToken").is_none(), "不可执行的组不发凭据");
    let group_id = group["groupId"].as_str().unwrap().to_string();

    // 伪造一份 identical 的服务端凭据强行覆盖：判定不允许该模式 → 拒绝，不写正文。
    let paths = env.paths();
    let store = env.store();
    let g = store
        .groups
        .iter()
        .find(|candidate| candidate.id == group_id)
        .unwrap();
    let source_member = session_link::active_member_for(g, "uid-a").unwrap();
    let target_member = session_link::active_member_for(g, "uid-b").unwrap();
    let source_content = member_content_state(&paths, &source_member.session_id);
    let target_content = member_content_state(&paths, &target_member.session_id);
    let baseline = session_link::load_pair_baseline(
        &paths,
        g,
        &source_member.member_id,
        &target_member.member_id,
    );
    assert_eq!(
        session_link::decide_sync(&source_content, &target_content, &baseline).verdict,
        SyncVerdict::Identical
    );
    let forged = session_link::save_preview_token(
        &paths,
        live_preview_binding(
            g,
            source_member,
            target_member,
            &source_content,
            &target_content,
            &baseline,
            SyncVerdict::Identical,
            &PreviewArchiveState::default(),
        ),
    )
    .unwrap();
    for mode in [SyncMode::Overwrite, SyncMode::FastForward] {
        let report = sync(&env, "uid-b", &[selection(&group_id, &forged, mode)]);
        assert!(report["synced"].as_array().unwrap().is_empty(), "{report}");
        assert!(report["skipped"].as_array().unwrap().is_empty(), "{report}");
        assert!(
            report["errors"][0]["error"]
                .as_str()
                .unwrap()
                .contains("不允许以"),
            "{report}"
        );
    }
    assert_eq!(body_bytes(&env, &target_id), target_before);
    assert_eq!(env.store().revision, revision_before);
    assert_eq!(pair_snapshot(&env).len(), 1, "基线没有被推进");
}

/// 备份失败 → 零覆盖：正文、数据库、基线、关联版本都没有变化，也不留未完成操作。
#[test]
fn sync_backup_failure_leaves_target_and_store_untouched() {
    let env = ready_env("sync-backup-fail");
    let (group_id, token, target_id) = fast_forward_scene(&env);
    let target_before = body_bytes(&env, &target_id);
    let row_before = session_row(&env, &target_id);
    let revision_before = env.store().revision;
    let baselines_before = env.baseline_files();
    let pairs_before = pair_snapshot(&env);

    // 临时目录根被占成普通文件 → 操作专属目录创建失败。
    std::fs::create_dir_all(env.paths.backup_root()).unwrap();
    let transactions_root = env
        .paths
        .backup_root()
        .join(session_backup::TRANSACTIONS_DIR_NAME);
    let _ = std::fs::remove_dir_all(&transactions_root);
    std::fs::write(&transactions_root, b"occupied").unwrap();

    let report = sync(
        &env,
        "uid-b",
        &[selection(&group_id, &token, SyncMode::FastForward)],
    );
    assert!(report["synced"].as_array().unwrap().is_empty(), "{report}");
    let error = report["errors"][0]["error"].as_str().unwrap();
    assert!(error.contains("临时目录创建失败"), "{error}");

    // 零写入：正文、数据库行、基线、关联版本原样保留。
    assert_eq!(body_bytes(&env, &target_id), target_before);
    assert_eq!(session_row(&env, &target_id), row_before);
    assert_eq!(env.store().revision, revision_before);
    assert_eq!(env.baseline_files(), baselines_before);
    assert_eq!(pair_snapshot(&env), pairs_before);
    // 备份没成功就什么都不能留下：没有未完成操作，也不需要恢复。
    assert!(session_link::pending_operations(&env.paths, WbVariant::Cn).is_empty());
    assert!(sync_operations(&env).is_empty());
    assert!(report.get("needsRecovery").is_none(), "{report}");
}

/// 正文写入阶段中断：不报成功、保留未完成操作、恢复后补完且不产生第二份。
#[cfg(unix)]
#[test]
fn sync_body_write_failure_keeps_pending_then_recovers() {
    use std::os::unix::fs::PermissionsExt;

    let env = ready_env("sync-body-fail");
    let (group_id, token, target_id) = fast_forward_scene(&env);
    let row_before = session_row(&env, &target_id);
    let target_before = body_bytes(&env, &target_id);

    let ws = env.paths.projects_dir().join("ws-a");
    std::fs::set_permissions(&ws, std::fs::Permissions::from_mode(0o555)).unwrap();
    let writable = std::fs::write(ws.join(".probe"), b"x").is_ok();
    let report = sync(
        &env,
        "uid-b",
        &[selection(&group_id, &token, SyncMode::FastForward)],
    );
    std::fs::set_permissions(&ws, std::fs::Permissions::from_mode(0o755)).unwrap();
    if writable {
        // root / 特殊 ACL 环境：写保护无效，跳过断言。
        return;
    }
    assert!(report["synced"].as_array().unwrap().is_empty(), "{report}");
    let error = report["errors"][0]["error"].as_str().unwrap();
    assert!(error.contains("同步内容保存失败"), "{error}");
    assert_eq!(report["needsRecovery"], true);

    // 正文与数据库都没变；备份已生成并留下未完成操作（阶段停在 Prepared）。
    assert_eq!(body_bytes(&env, &target_id), target_before);
    assert_eq!(session_row(&env, &target_id), row_before);
    let pending = session_link::pending_operations(&env.paths, WbVariant::Cn);
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].phase, OpPhase::Prepared);
    let operation_id = pending[0].operation_id.clone();
    let backup_dir = PathBuf::from(pending[0].backup.clone().unwrap())
        .parent()
        .unwrap()
        .to_path_buf();
    let manifest = load_sync_manifest(&backup_dir).expect("备份清单必须可读");

    // 恢复：按同一份清单补完，复用同一个目标 UUID 与新基线引用。
    let recovery = recover_pending_session_operations_at(&env.paths, WbVariant::Cn);
    assert_eq!(recovery.recovered, vec![operation_id.clone()]);
    assert!(recovery.is_clean(), "{:?}", recovery.needs_recovery);
    let expected = incoming_text(&env, "sess-1", &target_id);
    assert_eq!(
        std::fs::read_to_string(env.body_path(&target_id)).unwrap(),
        expected
    );
    assert_ne!(session_row(&env, &target_id).3, row_before.3);
    assert_eq!(env.body_files().len(), 2, "恢复不得产生第二份副本");
    assert_eq!(
        Some(pair_ref_of(
            &group_snapshot(&env, &group_id),
            "uid-a",
            "uid-b"
        )),
        manifest.new_baseline_ref,
        "恢复复用同一个新基线引用"
    );
    assert!(session_link::pending_operations(&env.paths, WbVariant::Cn).is_empty());
    let operations = sync_operations(&env);
    assert_eq!(operations.len(), 1, "不得新增第二条操作记录");
    assert_eq!(operations[0].operation_id, operation_id);
    assert_eq!(operations[0].phase, OpPhase::Completed);
    assert_eq!(operations[0].target.session_id, target_id);
}

/// 数据库更新阶段中断：不报成功；解除故障后恢复补完，不产生第二份写入。
#[test]
fn sync_db_update_failure_is_not_reported_as_success_then_recovers() {
    let env = ready_env("sync-db-fail");
    let (group_id, token, target_id) = fast_forward_scene(&env);
    let row_before = session_row(&env, &target_id);
    let expected = incoming_text(&env, "sess-1", &target_id);
    let baselines_before = env.baseline_files();
    let pairs_before = pair_snapshot(&env);

    install_block_update_trigger(&env);
    let report = sync(
        &env,
        "uid-b",
        &[selection(&group_id, &token, SyncMode::FastForward)],
    );
    assert!(report["synced"].as_array().unwrap().is_empty(), "{report}");
    let error = report["errors"][0]["error"].as_str().unwrap();
    assert!(error.contains("目标会话记录更新失败"), "{error}");
    assert_eq!(report["needsRecovery"], true);

    // 正文已写入，但数据库未更新：绝不报成功；组表也未提交。
    assert_eq!(
        std::fs::read_to_string(env.body_path(&target_id)).unwrap(),
        expected
    );
    assert_eq!(session_row(&env, &target_id), row_before);
    assert_eq!(env.baseline_files(), baselines_before);
    assert_eq!(pair_snapshot(&env), pairs_before);
    let pending = session_link::pending_operations(&env.paths, WbVariant::Cn);
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].phase, OpPhase::BodyWritten);
    let manifest_dir = PathBuf::from(pending[0].backup.clone().unwrap())
        .parent()
        .unwrap()
        .to_path_buf();
    let manifest = load_sync_manifest(&manifest_dir).unwrap();

    // 同一组再次同步（故障未解除）：预览已过期，不得写入，也不得新建第二份。
    let again = sync(
        &env,
        "uid-b",
        &[selection(&group_id, &token, SyncMode::FastForward)],
    );
    assert!(again["synced"].as_array().unwrap().is_empty(), "{again}");
    assert!(again.get("needsRecovery").is_some(), "{again}");
    assert_eq!(
        std::fs::read_to_string(env.body_path(&target_id)).unwrap(),
        expected
    );
    assert_eq!(env.body_files().len(), 2);

    // 历史同步恢复仍失败时也必须阻断启动，不能仅依赖本轮新增 pending 差集。
    let recovery = recover_pending_session_operations_at(&env.paths, WbVariant::Cn);
    assert_eq!(recovery.needs_recovery.len(), 1);
    assert!(recovery.recovered.is_empty());
    assert!(recovery.abandoned.is_empty());
    assert!(crate::modules::switch::recovery_blocks_startup(&recovery));
    assert_eq!(sync_operations(&env)[0].phase, OpPhase::BodyWritten);
    assert_eq!(session_row(&env, &target_id), row_before);
    assert_eq!(pair_snapshot(&env), pairs_before);

    // 解除故障后恢复：补完数据库与组表，仍复用同一份清单与同一条操作记录。
    drop_block_update_trigger(&env);
    let recovery = recover_pending_session_operations_at(&env.paths, WbVariant::Cn);
    assert_eq!(recovery.recovered.len(), 1, "{:?}", recovery.needs_recovery);
    assert!(recovery.is_clean(), "{:?}", recovery.needs_recovery);
    assert_eq!(
        session_row(&env, &target_id).3,
        Some(manifest.db.new_updated_at)
    );
    assert_eq!(
        std::fs::read_to_string(env.body_path(&target_id)).unwrap(),
        expected
    );
    assert_eq!(env.body_files().len(), 2);
    assert_eq!(env.baseline_files(), baselines_before + 1);
    let group = group_snapshot(&env, &group_id);
    assert_eq!(
        Some(pair_ref_of(&group, "uid-a", "uid-b")),
        manifest.new_baseline_ref
    );
    assert_eq!(
        session_link::active_member_for(&group, "uid-b")
            .unwrap()
            .last_synced_at,
        Some(manifest.last_synced_at)
    );
    let operations = sync_operations(&env);
    assert_eq!(operations.len(), 1);
    assert_eq!(operations[0].phase, OpPhase::Completed);
    assert_eq!(operations[0].target.session_id, target_id);
}

/// 组表提交阶段中断：正文与数据库已写、不报成功；恢复只补组表，不重放已完成阶段。
#[test]
fn sync_link_commit_failure_keeps_pending_then_recovers() {
    let env = ready_env("sync-link-fail");
    let (group_id, token, target_id) = fast_forward_scene(&env);
    let row_before = session_row(&env, &target_id);
    let expected = incoming_text(&env, "sess-1", &target_id);
    let baselines_before = env.baseline_files();
    let revision_before = env.store().revision;

    // 关联存储锁被占用：只有组表提交这一步会失败。
    let held = session_link::try_lock_file(&env.paths.link_store_lock_file()).unwrap();
    let report = sync(
        &env,
        "uid-b",
        &[selection(&group_id, &token, SyncMode::FastForward)],
    );
    drop(held);

    assert!(report["synced"].as_array().unwrap().is_empty(), "{report}");
    assert_eq!(report["needsRecovery"], true);
    let error = report["errors"][0]["error"].as_str().unwrap();
    assert!(error.contains("同步记录"), "{error}");

    // 正文与数据库都已写入，但组表未提交 → 阶段停在 DbWritten。
    assert_eq!(
        std::fs::read_to_string(env.body_path(&target_id)).unwrap(),
        expected
    );
    assert_ne!(session_row(&env, &target_id).3, row_before.3);
    assert_eq!(
        env.baseline_files(),
        baselines_before,
        "组表未提交，基线文件也未落盘"
    );
    assert_eq!(env.store().revision, revision_before, "组表未写");
    let pending = session_link::pending_operations(&env.paths, WbVariant::Cn);
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].phase, OpPhase::DbWritten);
    let manifest_dir = PathBuf::from(pending[0].backup.clone().unwrap())
        .parent()
        .unwrap()
        .to_path_buf();
    let manifest = load_sync_manifest(&manifest_dir).unwrap();

    // 恢复：只补组表（复用清单里的新基线引用），不重写正文、不重复登记第二份。
    let recovery = recover_pending_session_operations_at(&env.paths, WbVariant::Cn);
    assert_eq!(recovery.recovered.len(), 1, "{:?}", recovery.needs_recovery);
    assert!(recovery.is_clean(), "{:?}", recovery.needs_recovery);
    assert_eq!(env.store().revision, revision_before + 1, "组表只提交一次");
    assert_eq!(env.baseline_files(), baselines_before + 1, "不新增重复基线");
    assert_eq!(
        std::fs::read_to_string(env.body_path(&target_id)).unwrap(),
        expected
    );
    assert_eq!(
        session_row(&env, &target_id).3,
        Some(manifest.db.new_updated_at)
    );
    let group = group_snapshot(&env, &group_id);
    assert_eq!(
        Some(pair_ref_of(&group, "uid-a", "uid-b")),
        manifest.new_baseline_ref
    );
    assert_eq!(
        session_link::active_member_for(&group, "uid-b")
            .unwrap()
            .last_synced_at,
        Some(manifest.last_synced_at)
    );
    let operations = sync_operations(&env);
    assert_eq!(operations.len(), 1);
    assert_eq!(operations[0].phase, OpPhase::Completed);
    assert_eq!(env.body_files().len(), 2);
}

// ---------------------------------------------------------------------------
// 归档状态同步（statusOnly，#143）
// ---------------------------------------------------------------------------

/// 带状态列的可同步环境（会话表含 `status`）。
fn ready_env_with_status(name: &str) -> Env {
    let env = Env::new(name);
    env.create_db_with_status();
    env.create_edge_db(WbVariant::Cn);
    env.set_login("uid-a");
    env.add_session("sess-1", "uid-a", "标题一");
    env.add_body("sess-1", &body_text("sess-1"));
    env
}

/// 同上的历史形态：`status` 列可空（用来验证 NULL 不授予归档权限）。
fn ready_env_with_nullable_status(name: &str) -> Env {
    let env = Env::new(name);
    env.create_db_with_nullable_status();
    env.create_edge_db(WbVariant::Cn);
    env.set_login("uid-a");
    env.add_session("sess-1", "uid-a", "标题一");
    env.add_body("sess-1", &body_text("sess-1"));
    env
}

fn member_last_synced_at(env: &Env, group_id: &str, uid: &str) -> Option<i64> {
    group_snapshot(env, group_id)
        .members
        .into_iter()
        .find(|member| member.uid == uid)
        .and_then(|member| member.last_synced_at)
}

/// 造一个「正文一致 + 来源已归档 + 目标为指定状态」的场景（不要求可勾选）。
fn archive_row_scene(env: &Env, target_status: &str) -> (String, String) {
    let report = copy(env, "uid-b", &["sess-1"]);
    let target_id = env.first_copy_id(&report);
    env.set_status("sess-1", "archived");
    env.set_status(&target_id, target_status);
    let preview = preview(env, "uid-b");
    let group = &preview["groups"][0];
    assert_eq!(group["verdict"], "identical", "{preview}");
    (group["groupId"].as_str().unwrap().to_string(), target_id)
}

/// 造一个「正文一致 + 来源已归档 + 目标在终态」的可勾选场景。
fn archive_scene(env: &Env, target_status: &str) -> (String, String, String) {
    let (group_id, target_id) = archive_row_scene(env, target_status);
    let preview = preview(env, "uid-b");
    let group = &preview["groups"][0];
    (
        group_id,
        group["previewToken"].as_str().unwrap().to_string(),
        target_id,
    )
}

/// 造一个「可快进 + 来源已归档」的场景。
fn fast_forward_archive_scene(env: &Env, target_status: &str) -> (String, String, String) {
    let report = copy(env, "uid-b", &["sess-1"]);
    let target_id = env.first_copy_id(&report);
    append_records(&env.body_path("sess-1"), "sess-1", 2, 3);
    env.set_status("sess-1", "archived");
    env.set_status(&target_id, target_status);
    let preview = preview(env, "uid-b");
    let group = &preview["groups"][0];
    assert_eq!(group["verdict"], "fastForward", "{preview}");
    (
        group["groupId"].as_str().unwrap().to_string(),
        group["previewToken"].as_str().unwrap().to_string(),
        target_id,
    )
}

/// 目标已归档且来源有可快进的新记录：正文照写、时间照更新，状态保持 archived。
#[test]
fn fast_forward_keeps_archived_target_archived() {
    let env = ready_env_with_status("status-ff-already-archived");
    let report = copy(&env, "uid-b", &["sess-1"]);
    let target_id = env.first_copy_id(&report);
    append_records(&env.body_path("sess-1"), "sess-1", 2, 3);
    env.set_status("sess-1", "completed");
    env.set_status(&target_id, "archived");
    let preview = preview(&env, "uid-b");
    let group = &preview["groups"][0];
    assert_eq!(group["verdict"], "fastForward", "{preview}");
    assert!(group["archiveAction"].is_null(), "{group}");
    let token = group["previewToken"].as_str().unwrap().to_string();
    let group_id = group["groupId"].as_str().unwrap().to_string();
    let updated_before = session_row(&env, &target_id).3;

    let report = sync(
        &env,
        "uid-b",
        &[selection(&group_id, &token, SyncMode::FastForward)],
    );
    assert_eq!(report["synced"].as_array().unwrap().len(), 1, "{report}");
    assert_eq!(env.status_of(&target_id).as_deref(), Some("archived"));
    assert_eq!(
        std::fs::read_to_string(env.body_path(&target_id)).unwrap(),
        incoming_text(&env, "sess-1", &target_id)
    );
    assert_ne!(
        session_row(&env, &target_id).3,
        updated_before,
        "正文写入照旧更新时间"
    );
}

/// 造一个「双方都有改动（diverge）+ 来源已归档」的场景。
fn diverge_archive_scene(env: &Env, target_status: &str) -> (String, String, String) {
    let report = copy(env, "uid-b", &["sess-1"]);
    let target_id = env.first_copy_id(&report);
    // 目标追加一条来源没有的记录（index 100 与来源的 2/3 都不同）→ 双方互不为前缀。
    append_records(&env.body_path("sess-1"), "sess-1", 2, 2);
    append_records(&env.body_path(&target_id), &target_id, 100, 1);
    env.set_status("sess-1", "archived");
    env.set_status(&target_id, target_status);
    let preview = preview(env, "uid-b");
    let group = &preview["groups"][0];
    assert_eq!(group["verdict"], "diverge", "{preview}");
    (
        group["groupId"].as_str().unwrap().to_string(),
        group["previewToken"].as_str().unwrap().to_string(),
        target_id,
    )
}

/// 正文一致且归档资格成立：只改状态，正文字节、配对基线与 lastSyncedAt 全不动。
#[test]
fn status_sync_archives_target_without_touching_body() {
    let env = ready_env_with_status("status-identical");
    let (group_id, token, target_id) = archive_scene(&env, "completed");
    let preview = preview(&env, "uid-b");
    let group = &preview["groups"][0];
    assert_eq!(
        group["availableModes"],
        json!([]),
        "正文模式仍为空：归档不是正文覆盖"
    );
    assert_eq!(group["archiveAction"], "statusOnly");
    assert_eq!(group["defaultChecked"], false);

    let body_before = body_bytes(&env, &target_id);
    let pairs_before = pair_snapshot(&env);
    let baselines_before = env.baseline_files();
    let synced_before = member_last_synced_at(&env, &group_id, "uid-b");

    let report = sync(
        &env,
        "uid-b",
        &[selection(&group_id, &token, SyncMode::StatusOnly)],
    );
    assert_eq!(report["synced"].as_array().unwrap().len(), 1, "{report}");
    let item = &report["synced"][0];
    assert_eq!(item["mode"], "statusOnly");
    assert_eq!(item["verdict"], "identical");
    assert_eq!(item["recordCount"]["target"], 2, "记录数来自目标原正文");
    assert_eq!(env.status_of(&target_id).as_deref(), Some("archived"));
    assert_eq!(
        body_bytes(&env, &target_id),
        body_before,
        "正文字节不得变化"
    );
    assert_eq!(pair_snapshot(&env), pairs_before, "配对基线不得推进");
    assert_eq!(env.baseline_files(), baselines_before, "不得产生新基线");
    assert_eq!(
        member_last_synced_at(&env, &group_id, "uid-b"),
        synced_before,
        "正文 lastSyncedAt 保持原值"
    );
    assert_eq!(env.body_files().len(), 2, "不得产生第二份正文");
}

/// 终态白名单：completed / error / terminated / active 可归档；`working`、`Pending`、
/// NULL 与未知值一律不授权。
///
/// `active` 在列内（issue #204）：切号前客户端被关掉常留下陈旧 `active`，排除它会让
/// 归档状态永远无法与来源收敛；真正的活跃会话由写入侧三重门禁兜底。
#[test]
fn status_sync_accepts_only_terminal_target_statuses() {
    for allowed in ["completed", "error", "terminated", "active"] {
        let env = ready_env_with_status(&format!("status-allow-{allowed}"));
        let (group_id, token, target_id) = archive_scene(&env, allowed);
        let report = sync(
            &env,
            "uid-b",
            &[selection(&group_id, &token, SyncMode::StatusOnly)],
        );
        assert_eq!(
            report["synced"].as_array().unwrap().len(),
            1,
            "{allowed}: {report}"
        );
        assert_eq!(
            env.status_of(&target_id).as_deref(),
            Some("archived"),
            "{allowed}"
        );
    }
    for denied in ["working", "Pending", "unknown-value"] {
        let env = ready_env_with_status(&format!("status-deny-{denied}"));
        let (_, target_id) = archive_row_scene(&env, denied);
        let preview_report = preview(&env, "uid-b");
        let group = &preview_report["groups"][0];
        assert!(group["archiveAction"].is_null(), "{denied}: {group}");
        assert!(
            group["previewToken"].is_null(),
            "{denied}: 无动作即无凭据 {group}"
        );
        assert_eq!(
            env.status_of(&target_id).as_deref(),
            Some(denied),
            "{denied}: 不得写入状态"
        );
    }
    // status 为 NULL：非终态，不授权（列可空的旧库才写得进 NULL）。
    let env = ready_env_with_nullable_status("status-deny-null");
    let report = copy(&env, "uid-b", &["sess-1"]);
    let target_id = env.first_copy_id(&report);
    env.set_status("sess-1", "archived");
    env.set_status_null(&target_id);
    let preview_report = preview(&env, "uid-b");
    let group = &preview_report["groups"][0];
    assert_eq!(group["verdict"], "identical", "{preview_report}");
    assert!(group["archiveAction"].is_null(), "NULL: {group}");
    assert!(
        group["previewToken"].is_null(),
        "NULL: 无动作即无凭据 {group}"
    );
    assert_eq!(env.status_of(&target_id), None, "NULL: 不得写入状态");
}

/// issue #204：目标停在切号前残留的陈旧 `active` 时，归档必须仍能收敛。
///
/// 场景＝上次切号后客户端被关掉/崩掉，目标行停在 `active` 且没有在跑的生成；
/// 旧白名单把它当活跃态排除，会让该会话的归档状态永久卡住。
#[test]
fn status_sync_archives_stale_active_target() {
    let env = ready_env_with_status("status-stale-active");
    let (group_id, token, target_id) = archive_scene(&env, "active");
    let before = preview(&env, "uid-b");
    let group = &before["groups"][0];
    assert_eq!(group["archiveAction"], "statusOnly", "{before}");
    assert_eq!(group["availableModes"], json!([]), "归档不是正文覆盖");
    assert_eq!(group["defaultChecked"], false, "归档默认不勾选");
    let body_before = body_bytes(&env, &target_id);

    let report = sync(
        &env,
        "uid-b",
        &[selection(&group_id, &token, SyncMode::StatusOnly)],
    );
    assert_eq!(report["synced"].as_array().unwrap().len(), 1, "{report}");
    assert_eq!(report["synced"][0]["mode"], "statusOnly");
    assert_eq!(report["synced"][0]["verdict"], "identical");
    assert_eq!(
        env.status_of(&target_id).as_deref(),
        Some("archived"),
        "陈旧 active 目标必须被归档"
    );
    assert_eq!(
        body_bytes(&env, &target_id),
        body_before,
        "只改状态，正文不得变化"
    );
}

/// 陈旧的 `active` 目标一旦真的开始生成（`working`），归档立刻重新被拒绝。
///
/// 放行 `active` 靠的是「生成中的会话不会停在 active」这个前提：客户端在生成时
/// 先把状态写成 `working`，所以 `working` 仍在白名单外，仍是硬拦截。
#[test]
fn status_sync_denies_when_stale_active_target_starts_generating() {
    let env = ready_env_with_status("status-active-to-working");
    let (group_id, token, target_id) = archive_scene(&env, "active");
    env.set_status(&target_id, "working");

    let report = sync(
        &env,
        "uid-b",
        &[selection(&group_id, &token, SyncMode::StatusOnly)],
    );
    assert!(report["synced"].as_array().unwrap().is_empty(), "{report}");
    assert_eq!(report["skipped"][0]["reasonCode"], REASON_PREVIEW_STALE);
    assert_eq!(
        env.status_of(&target_id).as_deref(),
        Some("working"),
        "生成中的会话不得被收进归档"
    );
}

/// 单向粘滞：来源未归档、目标已归档时，绝不反向取消归档。
#[test]
fn status_sync_never_unarchives_target() {
    let env = ready_env_with_status("status-no-unarchive");
    let report = copy(&env, "uid-b", &["sess-1"]);
    let target_id = env.first_copy_id(&report);
    env.set_status("sess-1", "completed");
    env.set_status(&target_id, "archived");

    let preview = preview(&env, "uid-b");
    let group = &preview["groups"][0];
    assert_eq!(group["verdict"], "identical", "{preview}");
    assert!(group["archiveAction"].is_null(), "{group}");
    assert!(group["previewToken"].is_null(), "{group}");
    let updated_before = session_row(&env, &target_id).3;
    assert_eq!(env.status_of(&target_id).as_deref(), Some("archived"));
    assert_eq!(session_row(&env, &target_id).3, updated_before);
}

/// 双方都已归档：无动作、不写库、不抬时间戳。
#[test]
fn status_sync_is_noop_when_both_already_archived() {
    let env = ready_env_with_status("status-both-archived");
    let report = copy(&env, "uid-b", &["sess-1"]);
    let target_id = env.first_copy_id(&report);
    env.set_status("sess-1", "archived");
    env.set_status(&target_id, "archived");
    let updated_before = session_row(&env, &target_id).3;

    let preview = preview(&env, "uid-b");
    let group = &preview["groups"][0];
    assert!(group["archiveAction"].is_null(), "{group}");
    assert!(group["previewToken"].is_null(), "{group}");
    assert_eq!(
        session_row(&env, &target_id).3,
        updated_before,
        "不得刷新时间"
    );
}

/// Ahead 只传导归档：目标独有内容完整保留，后续判定不因状态操作虚假变成一致。
#[test]
fn status_sync_preserves_ahead_target_content() {
    let env = ready_env_with_status("status-ahead");
    let report = copy(&env, "uid-b", &["sess-1"]);
    let target_id = env.first_copy_id(&report);
    append_records(&env.body_path(&target_id), &target_id, 2, 2);
    env.set_status("sess-1", "archived");
    env.set_status(&target_id, "completed");

    let preview_report = preview(&env, "uid-b");
    let group = &preview_report["groups"][0];
    assert_eq!(group["verdict"], "ahead", "{preview_report}");
    assert_eq!(group["availableModes"], json!([]), "切号预览仍不给正文模式");
    assert_eq!(group["archiveAction"], "statusOnly");
    assert_eq!(group["defaultChecked"], false);
    let token = group["previewToken"].as_str().unwrap().to_string();
    let group_id = group["groupId"].as_str().unwrap().to_string();

    let body_before = std::fs::read_to_string(env.body_path(&target_id)).unwrap();
    let report = sync(
        &env,
        "uid-b",
        &[selection(&group_id, &token, SyncMode::StatusOnly)],
    );
    assert_eq!(report["synced"].as_array().unwrap().len(), 1, "{report}");
    assert_eq!(env.status_of(&target_id).as_deref(), Some("archived"));
    assert_eq!(
        std::fs::read_to_string(env.body_path(&target_id)).unwrap(),
        body_before,
        "目标独有内容必须保留"
    );
    // 状态操作不得让后续判定虚假变成 identical。
    let again = preview(&env, "uid-b");
    assert_eq!(again["groups"][0]["verdict"], "ahead", "{again}");
}

/// 快进且来源已归档：提交的是 fastForward，归档作为副作用随正文写入一并生效。
#[test]
fn fast_forward_archives_target_as_side_effect() {
    let env = ready_env_with_status("status-ff");
    let (group_id, token, target_id) = fast_forward_archive_scene(&env, "completed");
    let preview = preview(&env, "uid-b");
    let group = &preview["groups"][0];
    assert_eq!(group["availableModes"], json!(["fastForward"]));
    assert_eq!(group["archiveAction"], "statusOnly");

    let updated_before = session_row(&env, &target_id).3;
    let report = sync(
        &env,
        "uid-b",
        &[selection(&group_id, &token, SyncMode::FastForward)],
    );
    assert_eq!(report["synced"].as_array().unwrap().len(), 1, "{report}");
    assert_eq!(report["synced"][0]["mode"], "fastForward");
    assert_eq!(env.status_of(&target_id).as_deref(), Some("archived"));
    assert_eq!(
        std::fs::read_to_string(env.body_path(&target_id)).unwrap(),
        incoming_text(&env, "sess-1", &target_id),
        "正文必须含来源新记录"
    );
    assert_ne!(
        session_row(&env, &target_id).3,
        updated_before,
        "正文写入照旧更新时间"
    );
}

/// 陈旧 `active` 目标 + 正文快进：归档作为正文写入的副作用一并生效。
///
/// 正文模式的状态写入走 `planned_status_change`（写前重核，不查白名单）与事务内
/// 按前置状态等值匹配的条件 UPDATE，因此 active 目标既能被归档、现场被改成
/// `working` 时又会被跳过。
#[test]
fn fast_forward_archives_stale_active_target_as_side_effect() {
    let env = ready_env_with_status("status-ff-stale-active");
    let (group_id, token, target_id) = fast_forward_archive_scene(&env, "active");
    let preview = preview(&env, "uid-b");
    let group = &preview["groups"][0];
    assert_eq!(group["availableModes"], json!(["fastForward"]));
    assert_eq!(group["archiveAction"], "statusOnly");

    let updated_before = session_row(&env, &target_id).3;
    let report = sync(
        &env,
        "uid-b",
        &[selection(&group_id, &token, SyncMode::FastForward)],
    );
    assert_eq!(report["synced"].as_array().unwrap().len(), 1, "{report}");
    assert_eq!(report["synced"][0]["mode"], "fastForward");
    assert_eq!(
        env.status_of(&target_id).as_deref(),
        Some("archived"),
        "陈旧 active 目标必须随正文一并归档"
    );
    assert_eq!(
        std::fs::read_to_string(env.body_path(&target_id)).unwrap(),
        incoming_text(&env, "sess-1", &target_id),
        "正文必须含来源新记录"
    );
    assert_ne!(
        session_row(&env, &target_id).3,
        updated_before,
        "正文写入照旧更新时间"
    );
}

/// 冲突：归档资格成立也不默认勾选，只有显式提交 overwrite 才随正文归档。
#[test]
fn diverge_still_requires_explicit_overwrite_then_archives() {
    let env = ready_env_with_status("status-diverge");
    let (group_id, token, target_id) = diverge_archive_scene(&env, "completed");
    let preview = preview(&env, "uid-b");
    let group = &preview["groups"][0];
    assert_eq!(group["availableModes"], json!(["overwrite"]));
    assert_eq!(
        group["archiveAction"], "statusOnly",
        "预览时即固定，不随勾选变化"
    );
    assert_eq!(group["defaultChecked"], false, "冲突仍不默认勾选");

    let report = sync(
        &env,
        "uid-b",
        &[selection(&group_id, &token, SyncMode::Overwrite)],
    );
    assert_eq!(report["synced"].as_array().unwrap().len(), 1, "{report}");
    assert_eq!(env.status_of(&target_id).as_deref(), Some("archived"));
    assert_eq!(
        std::fs::read_to_string(env.body_path(&target_id)).unwrap(),
        incoming_text(&env, "sess-1", &target_id)
    );
}

/// 预览后来源取消归档：整体判为预览失效，零状态写入。
#[test]
fn status_sync_skips_when_source_unarchived_after_preview() {
    let env = ready_env_with_status("status-stale-source");
    let (group_id, token, target_id) = archive_scene(&env, "completed");
    env.set_status("sess-1", "completed");

    let report = sync(
        &env,
        "uid-b",
        &[selection(&group_id, &token, SyncMode::StatusOnly)],
    );
    assert!(report["synced"].as_array().unwrap().is_empty(), "{report}");
    assert_eq!(report["skipped"][0]["reasonCode"], REASON_PREVIEW_STALE);
    assert_eq!(env.status_of(&target_id).as_deref(), Some("completed"));
}

/// 预览后目标变为活跃：条件 UPDATE 拒绝，不得把活会话收进归档。
#[test]
fn status_sync_skips_when_target_becomes_active_after_preview() {
    let env = ready_env_with_status("status-stale-target");
    let (group_id, token, target_id) = archive_scene(&env, "completed");
    env.set_status(&target_id, "working");

    let report = sync(
        &env,
        "uid-b",
        &[selection(&group_id, &token, SyncMode::StatusOnly)],
    );
    assert!(report["synced"].as_array().unwrap().is_empty(), "{report}");
    assert_eq!(report["skipped"][0]["reasonCode"], REASON_PREVIEW_STALE);
    assert_eq!(env.status_of(&target_id).as_deref(), Some("working"));
}

/// 伪造 statusOnly 提交：凭据里没有授权即拒绝，不静默执行。
#[test]
fn status_sync_rejects_unauthorized_status_only() {
    // 不带 status 列的库：双方状态读数恒为 None，与伪造凭据一致，
    // 因此走的是「凭据未授权归档」的拒绝路径，而不是预览过期。
    let env = ready_env("status-unauthorized");
    let report = copy(&env, "uid-b", &["sess-1"]);
    let target_id = env.first_copy_id(&report);
    let preview = preview(&env, "uid-b");
    let group = &preview["groups"][0];
    assert_eq!(group["verdict"], "identical", "{preview}");
    assert!(group["previewToken"].is_null(), "无动作即无凭据 {group}");

    // 手工伪造一份「identical + statusOnly」凭据：执行必须拒绝。
    let store = env.store();
    let group_id = store.groups[0].id.clone();
    let source_member = store.groups[0]
        .members
        .iter()
        .find(|member| member.uid == "uid-a")
        .unwrap()
        .clone();
    let target_member = store.groups[0]
        .members
        .iter()
        .find(|member| member.uid == "uid-b")
        .unwrap()
        .clone();
    let source_content = member_content_state(&env.paths(), &source_member.session_id);
    let target_content = member_content_state(&env.paths(), &target_member.session_id);
    let baseline = session_link::load_pair_baseline(
        &env.paths(),
        &store.groups[0],
        &source_member.member_id,
        &target_member.member_id,
    );
    let forged = session_link::save_preview_token(
        &env.paths(),
        live_preview_binding(
            &store.groups[0],
            &source_member,
            &target_member,
            &source_content,
            &target_content,
            &baseline,
            SyncVerdict::Identical,
            &PreviewArchiveState::default(),
        ),
    )
    .unwrap();
    let report = sync(
        &env,
        "uid-b",
        &[selection(&group_id, &forged, SyncMode::StatusOnly)],
    );
    assert!(report["synced"].as_array().unwrap().is_empty(), "{report}");
    assert_eq!(report["errors"].as_array().unwrap().len(), 1, "{report}");
    assert!(
        report["errors"][0]["error"]
            .as_str()
            .unwrap()
            .contains("不提供仅同步归档"),
        "{report}"
    );
    let _ = target_id;
}

/// 数据库写入中断后恢复：补完归档，无正文写入、无新基线；重复恢复不二次写入。
#[test]
fn status_sync_recovers_after_interrupted_db_write() {
    let env = ready_env_with_status("status-recover");
    let (group_id, token, target_id) = archive_scene(&env, "completed");
    let body_before = body_bytes(&env, &target_id);
    let baselines_before = env.baseline_files();
    let pairs_before = pair_snapshot(&env);

    install_block_update_trigger(&env);
    let report = sync(
        &env,
        "uid-b",
        &[selection(&group_id, &token, SyncMode::StatusOnly)],
    );
    assert!(report["synced"].as_array().unwrap().is_empty(), "{report}");
    assert_eq!(report["errors"].as_array().unwrap().len(), 1, "{report}");
    assert_eq!(env.status_of(&target_id).as_deref(), Some("completed"));
    drop_block_update_trigger(&env);

    let recovery = recover_pending_session_operations_at(&env.paths, WbVariant::Cn);
    assert!(
        recovery.needs_recovery.is_empty(),
        "{:?}",
        recovery.needs_recovery
    );
    assert_eq!(recovery.recovered.len(), 1, "{:?}", recovery);
    assert_eq!(env.status_of(&target_id).as_deref(), Some("archived"));
    assert_eq!(body_bytes(&env, &target_id), body_before, "恢复不得写正文");
    assert_eq!(env.baseline_files(), baselines_before, "恢复不得产生基线");
    assert_eq!(pair_snapshot(&env), pairs_before);

    // 重复恢复：已完成的不再重放，也不再刷新时间。
    let updated = session_row(&env, &target_id).3;
    let again = recover_pending_session_operations_at(&env.paths, WbVariant::Cn);
    assert!(
        again.needs_recovery.is_empty(),
        "{:?}",
        again.needs_recovery
    );
    assert!(again.recovered.is_empty(), "{:?}", again);
    assert_eq!(session_row(&env, &target_id).3, updated);
}

/// 中断后目标正文被追加：保留并报告人工处理，不补写正文、不越权改状态。
#[test]
fn status_sync_reports_manual_handling_when_body_changed() {
    let env = ready_env_with_status("status-recover-tampered");
    let (group_id, token, target_id) = archive_scene(&env, "completed");
    install_block_update_trigger(&env);
    let report = sync(
        &env,
        "uid-b",
        &[selection(&group_id, &token, SyncMode::StatusOnly)],
    );
    assert!(report["synced"].as_array().unwrap().is_empty(), "{report}");
    drop_block_update_trigger(&env);

    append_records(&env.body_path(&target_id), &target_id, 100, 1);
    let recovery = recover_pending_session_operations_at(&env.paths, WbVariant::Cn);
    assert!(recovery.recovered.is_empty(), "{:?}", recovery);
    assert_eq!(recovery.needs_recovery.len(), 1, "{:?}", recovery);
    assert!(!recovery.needs_recovery[0].retryable);
    assert_eq!(
        env.status_of(&target_id).as_deref(),
        Some("completed"),
        "正文已变即不得补写归档"
    );
}

/// 旧清单缺 status / newStatus：正常解析并按旧行为恢复，不自动归档。
#[test]
fn legacy_manifest_without_status_fields_parses_with_old_behavior() {
    let env = ready_env_with_status("legacy-manifest");
    let (group_id, token, _target_id) = fast_forward_archive_scene(&env, "completed");
    install_block_update_trigger(&env);
    let report = sync(
        &env,
        "uid-b",
        &[selection(&group_id, &token, SyncMode::FastForward)],
    );
    assert!(report["synced"].as_array().unwrap().is_empty(), "{report}");
    drop_block_update_trigger(&env);
    let operation = sync_operations(&env)[0].clone();
    let manifest_path = operation.backup.clone().unwrap();
    let text = std::fs::read_to_string(&manifest_path).unwrap();
    let mut value: Value = serde_json::from_str(&text).unwrap();
    {
        let db = value.get_mut("db").unwrap().as_object_mut().unwrap();
        db.remove("newStatus");
        if let Some(row) = db.get_mut("targetRow").and_then(Value::as_object_mut) {
            row.remove("status");
        }
    }
    let dir = env.root.join("legacy-manifest-dir");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("manifest.json"),
        serde_json::to_string_pretty(&value).unwrap(),
    )
    .unwrap();
    let parsed = load_sync_manifest(&dir).expect("旧清单必须能解析");
    assert_eq!(parsed.db.new_status, None, "旧清单不得被当成归档意图");
    assert!(parsed
        .db
        .target_row
        .as_ref()
        .and_then(|row| row.status.clone())
        .is_none());
    assert!(
        parsed.new_baseline_ref.is_some(),
        "旧清单的字符串基线引用仍读为 Some"
    );
    assert_eq!(parsed.mode, SyncMode::FastForward);
}

/// 仅同步归档的清单不落新基线引用；正文模式必须有。
#[test]
fn status_only_manifest_has_no_new_baseline_ref() {
    let env = ready_env_with_status("status-manifest-baseline");
    let (group_id, token, _target_id) = archive_scene(&env, "completed");
    install_block_update_trigger(&env);
    let _ = sync(
        &env,
        "uid-b",
        &[selection(&group_id, &token, SyncMode::StatusOnly)],
    );
    drop_block_update_trigger(&env);
    let operation = sync_operations(&env)[0].clone();
    let manifest = load_sync_manifest(
        Path::new(&operation.backup.clone().unwrap())
            .parent()
            .unwrap(),
    )
    .expect("清单必须可读");
    assert_eq!(manifest.mode, SyncMode::StatusOnly);
    assert_eq!(manifest.new_baseline_ref, None, "状态同步不得写假基线引用");
    assert_eq!(manifest.db.new_status.as_deref(), Some("archived"));
    assert_eq!(
        manifest
            .db
            .target_row
            .as_ref()
            .and_then(|row| row.status.clone()),
        Some("completed".to_string())
    );
}

/// 会话表缺 status 列：不归档、不报列名错误，正文同步照旧可用。
#[test]
fn status_sync_degrades_when_status_column_missing() {
    let env = ready_env("status-no-column");
    let report = copy(&env, "uid-b", &["sess-1"]);
    let target_id = env.first_copy_id(&report);
    append_records(&env.body_path("sess-1"), "sess-1", 2, 3);

    let preview = preview(&env, "uid-b");
    let group = &preview["groups"][0];
    assert_eq!(group["verdict"], "fastForward", "{preview}");
    assert!(
        group["archiveAction"].is_null(),
        "缺列即不提供归档动作 {group}"
    );
    let token = group["previewToken"].as_str().unwrap().to_string();
    let group_id = group["groupId"].as_str().unwrap().to_string();

    // 正文同步不受影响。
    let report = sync(
        &env,
        "uid-b",
        &[selection(&group_id, &token, SyncMode::FastForward)],
    );
    assert_eq!(report["synced"].as_array().unwrap().len(), 1, "{report}");
    assert_eq!(
        std::fs::read_to_string(env.body_path(&target_id)).unwrap(),
        incoming_text(&env, "sess-1", &target_id)
    );
}

/// `read_session_row` 在 custom_title × status 四种列组合下索引都正确。
#[test]
fn read_session_row_handles_every_column_combination() {
    for (has_custom_title, has_status) in
        [(true, true), (true, false), (false, true), (false, false)]
    {
        let env = Env::new(&format!("row-cols-{has_custom_title}-{has_status}"));
        let mut columns = vec![
            "id TEXT PRIMARY KEY".to_string(),
            "user_id TEXT NOT NULL".to_string(),
            "title TEXT".to_string(),
        ];
        if has_custom_title {
            columns.push("custom_title TEXT".to_string());
        }
        if has_status {
            columns.push("status TEXT".to_string());
        }
        columns.push("updated_at INTEGER".to_string());
        columns.push("deleted_at INTEGER".to_string());
        let conn = Connection::open(env.paths.workbuddy_db()).unwrap();
        conn.execute_batch(&format!("CREATE TABLE sessions ({});", columns.join(", ")))
            .unwrap();
        conn.execute(
            "INSERT INTO sessions (id, user_id, title, updated_at, deleted_at) \
                 VALUES ('s1', 'uid-a', '标题', 2000, NULL)",
            [],
        )
        .unwrap();
        if has_custom_title {
            conn.execute(
                "UPDATE sessions SET custom_title = '自定义' WHERE id = 's1'",
                [],
            )
            .unwrap();
        }
        if has_status {
            conn.execute(
                "UPDATE sessions SET status = 'completed' WHERE id = 's1'",
                [],
            )
            .unwrap();
        }
        let row = read_session_row(&conn, "s1").unwrap().expect("行必须可读");
        assert_eq!(row.session_id, "s1");
        assert_eq!(row.user_id, "uid-a");
        assert_eq!(row.title.as_deref(), Some("标题"));
        assert_eq!(
            row.custom_title.as_deref(),
            has_custom_title.then_some("自定义"),
            "{has_custom_title}/{has_status}"
        );
        assert_eq!(
            row.status.as_deref(),
            has_status.then_some("completed"),
            "{has_custom_title}/{has_status}"
        );
        assert_eq!(
            row.updated_at,
            Some(2000),
            "{has_custom_title}/{has_status}"
        );
        assert_eq!(row.deleted_at, None);
    }
}

/// `statusOnly` 只能由归档资格授权，不属于任何正文判定的权限。
#[test]
fn status_only_mode_is_not_a_body_permission() {
    assert_eq!(SyncMode::parse("statusOnly").unwrap(), SyncMode::StatusOnly);
    assert_eq!(SyncMode::StatusOnly.as_str(), "statusOnly");
    for verdict in [
        SyncVerdict::Identical,
        SyncVerdict::FastForward,
        SyncVerdict::Ahead,
        SyncVerdict::Diverge,
        SyncVerdict::Unknown,
    ] {
        assert!(!verdict.allows(SyncMode::StatusOnly), "{verdict:?}");
        assert!(
            !verdict.available_modes().contains(&SyncMode::StatusOnly),
            "{verdict:?} 的正文模式里不得出现 statusOnly"
        );
    }
    // 原有模式权限不变。
    assert!(SyncVerdict::FastForward.allows(SyncMode::FastForward));
    assert!(SyncVerdict::Diverge.allows(SyncMode::Overwrite));
    assert!(SyncVerdict::Ahead.allows(SyncMode::UnifyOverwrite));
}

/// 预览绑定的归档字段：旧凭据可反序列化，状态或动作变化必须报过期。
#[test]
fn preview_binding_archive_fields_round_trip_and_are_verified() {
    let env = ready_env_with_status("preview-binding-archive");
    let (group_id, token, _target_id) = archive_scene(&env, "completed");
    let stored = session_link::load_preview_token(&env.paths(), &token).expect("凭据必须存在");
    assert_eq!(stored.version, session_link::PREVIEW_TOKEN_VERSION);
    assert_eq!(stored.binding.source_status.as_deref(), Some("archived"));
    assert_eq!(stored.binding.target_status.as_deref(), Some("completed"));
    assert_eq!(stored.binding.archive_action.as_deref(), Some("statusOnly"));

    // 旧格式（无归档字段）仍能反序列化，且一律按「未授权」处理。
    let mut value = serde_json::to_value(&stored.binding).unwrap();
    let object = value.as_object_mut().unwrap();
    object.remove("sourceStatus");
    object.remove("targetStatus");
    object.remove("archiveAction");
    let legacy: PreviewBinding = serde_json::from_value(value).unwrap();
    assert_eq!(legacy.source_status, None);
    assert_eq!(legacy.target_status, None);
    assert_eq!(legacy.archive_action, None);

    // 状态或动作任一项变化都必须报过期。
    let mut drifted_status = stored.binding.clone();
    drifted_status.target_status = Some("active".to_string());
    let stale = session_link::verify_preview(&stored, &drifted_status);
    assert!(
        stale.iter().any(|reason| reason.contains("归档状态")),
        "{stale:?}"
    );
    let mut drifted_action = stored.binding.clone();
    drifted_action.archive_action = None;
    let stale = session_link::verify_preview(&stored, &drifted_action);
    assert!(
        stale.iter().any(|reason| reason.contains("归档")),
        "{stale:?}"
    );
    let _ = group_id;
}

/// 旧清单在**带 status 列的库**上必须仍能按旧行为恢复完成。
///
/// 回归 guarding：旧清单反序列化后 `targetRow.status` 为 None，若拿它去和现场读出的
/// `Some("completed")` 比对，会恒不相等并把正常恢复误判成「状态被改动」。
#[test]
fn legacy_manifest_recovers_on_database_with_status_column() {
    let env = ready_env_with_status("legacy-manifest-status-db");
    let (group_id, token, target_id) = fast_forward_archive_scene(&env, "completed");
    install_block_update_trigger(&env);
    let report = sync(
        &env,
        "uid-b",
        &[selection(&group_id, &token, SyncMode::FastForward)],
    );
    assert!(report["synced"].as_array().unwrap().is_empty(), "{report}");
    drop_block_update_trigger(&env);

    // 把清单改写成改动之前的旧格式：去掉 status 与 newStatus。
    let operation = sync_operations(&env)[0].clone();
    let manifest_path = operation.backup.clone().unwrap();
    let text = std::fs::read_to_string(&manifest_path).unwrap();
    let mut value: Value = serde_json::from_str(&text).unwrap();
    {
        let db = value.get_mut("db").unwrap().as_object_mut().unwrap();
        db.remove("newStatus");
        if let Some(row) = db.get_mut("targetRow").and_then(Value::as_object_mut) {
            row.remove("status");
        }
    }
    std::fs::write(
        &manifest_path,
        serde_json::to_string_pretty(&value).unwrap(),
    )
    .unwrap();

    let recovery = recover_pending_session_operations_at(&env.paths, WbVariant::Cn);
    assert!(
        recovery.needs_recovery.is_empty(),
        "旧清单必须能恢复完成：{:?}",
        recovery.needs_recovery
    );
    assert_eq!(recovery.recovered.len(), 1, "{:?}", recovery);
    // 旧清单不含归档意图：正文补完即可，不得顺手归档。
    assert_eq!(env.status_of(&target_id).as_deref(), Some("completed"));
}

/// 只归档遇到正文文件被删：放弃本次归档，不写库、不补正文，也不永久阻断启动。
#[test]
fn status_sync_abandons_when_target_body_gone() {
    let env = ready_env_with_status("status-body-gone");
    let (group_id, token, target_id) = archive_scene(&env, "completed");
    install_block_update_trigger(&env);
    let report = sync(
        &env,
        "uid-b",
        &[selection(&group_id, &token, SyncMode::StatusOnly)],
    );
    assert!(report["synced"].as_array().unwrap().is_empty(), "{report}");
    drop_block_update_trigger(&env);
    std::fs::remove_file(env.body_path(&target_id)).unwrap();

    let recovery = recover_pending_session_operations_at(&env.paths, WbVariant::Cn);
    assert!(
        recovery.needs_recovery.is_empty(),
        "正文缺失不得变成需要人工处理：{:?}",
        recovery.needs_recovery
    );
    assert_eq!(recovery.abandoned.len(), 1, "{:?}", recovery);
    assert_eq!(
        env.status_of(&target_id).as_deref(),
        Some("completed"),
        "放弃时不得写状态"
    );
    assert!(!env.body_path(&target_id).exists(), "不得重建正文文件");
}

/// 不可验证（unknown）时不下发 archiveAction：没有凭据就勾不了，发了只会误导。
#[test]
fn unknown_verdict_never_offers_archive_action() {
    let env = ready_env_with_status("status-unknown");
    let report = copy(&env, "uid-b", &["sess-1"]);
    let target_id = env.first_copy_id(&report);
    env.set_status("sess-1", "archived");
    env.set_status(&target_id, "completed");
    // 删掉目标正文：内容不可验证 → unknown。
    std::fs::remove_file(env.body_path(&target_id)).unwrap();

    let preview = preview(&env, "uid-b");
    let group = &preview["groups"][0];
    assert_eq!(group["verdict"], "unknown", "{preview}");
    assert!(group["archiveAction"].is_null(), "{group}");
    assert!(group["previewToken"].is_null(), "{group}");
    assert_eq!(env.status_of(&target_id).as_deref(), Some("completed"));
}

/// 归档已提交、完成标记未落盘，之后正文丢失：放弃本次，保留已提交的状态。
///
/// `DbWritten` 晚于 `BodyWritten`，若放弃分支排在「已写正文却丢失」的通用拦截
/// 之后，这个现场会先被通用分支截走并永久阻断启动。
#[test]
fn status_sync_abandons_when_body_lost_after_db_commit() {
    let env = ready_env_with_status("status-gone-after-commit");
    let (group_id, token, target_id) = archive_scene(&env, "completed");
    install_block_update_trigger(&env);
    let report = sync(
        &env,
        "uid-b",
        &[selection(&group_id, &token, SyncMode::StatusOnly)],
    );
    assert!(report["synced"].as_array().unwrap().is_empty(), "{report}");
    drop_block_update_trigger(&env);

    // 手工推进到「数据库已提交、完成标记未落盘」的现场：状态置为本次新值，
    // 操作阶段停在 DbWritten。
    env.set_status(&target_id, "archived");
    let mut operation = sync_operations(&env)[0].clone();
    operation.phase = OpPhase::DbWritten;
    session_link::save_operation(&env.paths, &operation).unwrap();
    std::fs::remove_file(env.body_path(&target_id)).unwrap();

    let recovery = recover_pending_session_operations_at(&env.paths, WbVariant::Cn);
    assert!(
        recovery.needs_recovery.is_empty(),
        "归档已提交后正文丢失不得变成人工处理：{:?}",
        recovery.needs_recovery
    );
    assert_eq!(recovery.abandoned.len(), 1, "{:?}", recovery);
    assert_eq!(
        env.status_of(&target_id).as_deref(),
        Some("archived"),
        "已提交的状态必须保留"
    );
    assert!(!env.body_path(&target_id).exists(), "不得补写或重建正文");
}

/// 不写状态的正文恢复：目标状态被改成 active 也照常补完，不被状态变化阻断。
#[test]
fn body_only_recovery_tolerates_unrelated_status_change() {
    let env = ready_env_with_status("body-recovery-status-changed");
    let (group_id, token, target_id) = fast_forward_archive_scene(&env, "completed");
    install_block_update_trigger(&env);
    let report = sync(
        &env,
        "uid-b",
        &[selection(&group_id, &token, SyncMode::FastForward)],
    );
    assert!(report["synced"].as_array().unwrap().is_empty(), "{report}");
    drop_block_update_trigger(&env);
    // 中断期间目标被打开并开始生成：状态变为 active（与本次正文同步无关）。
    env.set_status(&target_id, "active");

    let recovery = recover_pending_session_operations_at(&env.paths, WbVariant::Cn);
    assert!(
        recovery.needs_recovery.is_empty(),
        "与状态无关的正文恢复不得被阻断：{:?}",
        recovery.needs_recovery
    );
    assert_eq!(recovery.recovered.len(), 1, "{:?}", recovery);
    assert_eq!(
        env.status_of(&target_id).as_deref(),
        Some("active"),
        "本次不写状态，必须保留当前状态"
    );
    assert_eq!(
        std::fs::read_to_string(env.body_path(&target_id)).unwrap(),
        incoming_text(&env, "sess-1", &target_id),
        "正文必须补完"
    );
}

/// 跳过归档副作用后「提交数据库 → 完成标记落盘前」再次崩溃：恢复必须能补完，
/// 不能把自己合法产生的「本次时间戳 + active」判成外部修改。
#[test]
fn body_mode_recovery_survives_second_crash_after_archive_skipped() {
    let env = ready_env_with_status("body-skip-second-crash");
    let (group_id, token, target_id) = fast_forward_archive_scene(&env, "completed");
    install_block_update_trigger(&env);
    let report = sync(
        &env,
        "uid-b",
        &[selection(&group_id, &token, SyncMode::FastForward)],
    );
    assert!(report["synced"].as_array().unwrap().is_empty(), "{report}");
    drop_block_update_trigger(&env);
    // 目标被打开并开始生成：归档副作用将被合法跳过。
    env.set_status(&target_id, "active");

    // 手工构造「已跳过归档并提交数据库、完成标记未落盘」的现场。
    let mut operation = sync_operations(&env)[0].clone();
    let manifest = load_sync_manifest(
        Path::new(operation.backup.as_ref().unwrap())
            .parent()
            .unwrap(),
    )
    .expect("清单必须可读");
    env.set_updated_at(&target_id, manifest.db.new_updated_at);
    operation.phase = OpPhase::DbWritten;
    session_link::save_operation(&env.paths, &operation).unwrap();

    let recovery = recover_pending_session_operations_at(&env.paths, WbVariant::Cn);
    assert!(
        recovery.needs_recovery.is_empty(),
        "跳过归档后的二次崩溃不得阻断恢复：{:?}",
        recovery.needs_recovery
    );
    assert_eq!(recovery.recovered.len(), 1, "{:?}", recovery);
    assert_eq!(
        env.status_of(&target_id).as_deref(),
        Some("active"),
        "活跃会话绝不能被归档"
    );
    assert_eq!(
        std::fs::read_to_string(env.body_path(&target_id)).unwrap(),
        incoming_text(&env, "sess-1", &target_id)
    );
}

/// 正文模式下「数据库已提交、归档曾被跳过、目标又回到终态」：恢复不得重做归档。
///
/// 跳过与否是现场状态的纯函数，但**纯函数不等于跨次恢复幂等**——输入状态会变，
/// 重新推导就可能从「跳过」翻成「可归档」。数据库步骤必须恰好执行一次。
#[test]
fn body_mode_recovery_does_not_reapply_skipped_archive() {
    let env = ready_env_with_status("body-no-rearchive");
    let (group_id, token, target_id) = fast_forward_archive_scene(&env, "completed");
    install_block_update_trigger(&env);
    let report = sync(
        &env,
        "uid-b",
        &[selection(&group_id, &token, SyncMode::FastForward)],
    );
    assert!(report["synced"].as_array().unwrap().is_empty(), "{report}");
    drop_block_update_trigger(&env);

    // 手工构造「数据库已提交且归档被跳过、完成标记未落盘」的现场：时间戳是本次值，
    // 状态仍是 completed（归档跳过后回到终态，或用户取消了归档）。
    let mut operation = sync_operations(&env)[0].clone();
    let manifest = load_sync_manifest(
        Path::new(operation.backup.as_ref().unwrap())
            .parent()
            .unwrap(),
    )
    .expect("清单必须可读");
    env.set_updated_at(&target_id, manifest.db.new_updated_at);
    operation.phase = OpPhase::DbWritten;
    session_link::save_operation(&env.paths, &operation).unwrap();

    let recovery = recover_pending_session_operations_at(&env.paths, WbVariant::Cn);
    assert!(
        recovery.needs_recovery.is_empty(),
        "数据库已提交后恢复不得再被阻断：{:?}",
        recovery.needs_recovery
    );
    assert_eq!(recovery.recovered.len(), 1, "{:?}", recovery);
    assert_eq!(
        env.status_of(&target_id).as_deref(),
        Some("completed"),
        "恢复不得重做已跳过的归档"
    );
    assert_eq!(
        std::fs::read_to_string(env.body_path(&target_id)).unwrap(),
        incoming_text(&env, "sess-1", &target_id),
        "正文必须已补完"
    );
}

/// 造一个「正文已写入、数据库未更新」的中断现场：返回 (目标会话 id, 覆盖前正文, 本次写入正文)。
fn interrupted_sync_scene(env: &Env) -> (String, Vec<u8>, String) {
    let (group_id, token, target_id) = fast_forward_scene(env);
    let target_before = body_bytes(env, &target_id);
    install_block_update_trigger(env);
    let report = sync(
        env,
        "uid-b",
        &[selection(&group_id, &token, SyncMode::FastForward)],
    );
    assert_eq!(report["needsRecovery"], true, "{report}");
    drop_block_update_trigger(env);
    let written = std::fs::read_to_string(env.body_path(&target_id)).unwrap();
    assert_eq!(written, incoming_text(env, "sess-1", &target_id));
    assert_eq!(
        session_link::pending_operations(&env.paths, WbVariant::Cn)[0].phase,
        OpPhase::BodyWritten
    );
    (target_id, target_before, written)
}

/// 恢复不覆盖后续无关修改：目标正文被改动过 → 停止恢复并要求人工处理。
#[test]
fn sync_recovery_stops_when_target_changed_after_interrupted_write() {
    let env = ready_env("sync-recovery-unknown");
    let (target_id, _, _) = interrupted_sync_scene(&env);
    let baselines_before = env.baseline_files();
    let pairs_before = pair_snapshot(&env);

    // 官方 App / 用户对中间产物继续追加了内容：属于「未知内容」，不得覆盖。
    append_records(&env.body_path(&target_id), &target_id, 100, 1);
    let tampered = std::fs::read_to_string(env.body_path(&target_id)).unwrap();

    let recovery = recover_pending_session_operations_at(&env.paths, WbVariant::Cn);
    assert!(recovery.recovered.is_empty(), "{:?}", recovery.recovered);
    assert_eq!(recovery.needs_recovery.len(), 1);
    assert!(
        !recovery.needs_recovery[0].retryable,
        "未知内容必须暂停启动：{:?}",
        recovery.needs_recovery[0]
    );
    assert!(
        recovery.needs_recovery[0].reason.contains("不一致")
            || recovery.needs_recovery[0].reason.contains("无法验证"),
        "{}",
        recovery.needs_recovery[0].reason
    );
    assert_eq!(
        std::fs::read_to_string(env.body_path(&target_id)).unwrap(),
        tampered,
        "不得覆盖后续无关修改"
    );
    // 数据库与组表都保持原样，未完成操作保留待人工处理。
    assert_eq!(env.baseline_files(), baselines_before);
    assert_eq!(pair_snapshot(&env), pairs_before);
    assert_eq!(
        session_link::pending_operations(&env.paths, WbVariant::Cn).len(),
        1
    );
    // 编排层据此暂停切换与启动 App。
    assert!(crate::modules::switch::recovery_blocks_startup(&recovery));
}

/// 目标行更新时间被其它程序改动 → 停止恢复（不覆盖未知修改）。
#[test]
fn sync_recovery_stops_when_target_row_changed_after_write() {
    let env = ready_env("sync-recovery-row-drift");
    let (target_id, _, written) = interrupted_sync_scene(&env);

    let conn = Connection::open(env.paths.workbuddy_db()).unwrap();
    conn.execute(
        "UPDATE sessions SET updated_at = 987654321 WHERE id = ?1",
        [&target_id],
    )
    .unwrap();
    drop(conn);

    let recovery = recover_pending_session_operations_at(&env.paths, WbVariant::Cn);
    assert!(recovery.recovered.is_empty(), "{:?}", recovery.recovered);
    assert_eq!(recovery.needs_recovery.len(), 1);
    assert!(!recovery.needs_recovery[0].retryable);
    assert!(
        recovery.needs_recovery[0].reason.contains("更新时间"),
        "{}",
        recovery.needs_recovery[0].reason
    );
    assert_eq!(
        std::fs::read_to_string(env.body_path(&target_id)).unwrap(),
        written
    );
}

/// 目标行归属不符（被改到别的账号）→ 停止恢复，不报成功。
#[test]
fn sync_recovery_stops_when_target_row_owner_changed() {
    let env = ready_env("sync-recovery-owner");
    let (target_id, _, written) = interrupted_sync_scene(&env);

    let conn = Connection::open(env.paths.workbuddy_db()).unwrap();
    conn.execute(
        "UPDATE sessions SET user_id = 'uid-x' WHERE id = ?1",
        [&target_id],
    )
    .unwrap();
    drop(conn);

    let recovery = recover_pending_session_operations_at(&env.paths, WbVariant::Cn);
    assert!(recovery.recovered.is_empty(), "{:?}", recovery.recovered);
    assert_eq!(recovery.needs_recovery.len(), 1);
    assert!(!recovery.needs_recovery[0].retryable);
    assert!(
        recovery.needs_recovery[0].reason.contains("归属"),
        "{}",
        recovery.needs_recovery[0].reason
    );
    assert_eq!(
        std::fs::read_to_string(env.body_path(&target_id)).unwrap(),
        written,
        "归属异常时不得继续改写内容"
    );
    assert_eq!(
        session_link::pending_operations(&env.paths, WbVariant::Cn).len(),
        1
    );
}

/// 恢复期间数据库不可读：不覆盖正文/行/基线，保留 pending 并暂停启动；解除后恢复成功。
#[test]
fn sync_recovery_preserves_committed_state_on_database_read_failure() {
    for fault in ["open", "schema", "query"] {
        let env = ready_env(&format!("sync-read-failure-{fault}"));
        // 写入阶段被故障中断：操作未完成、临时备份按契约保留（只在完成后才清理）。
        let (target_id, _, _) = interrupted_sync_scene(&env);
        let operation = session_link::pending_operations(&env.paths, WbVariant::Cn)
            .into_iter()
            .next()
            .expect("故障后必须保留未完成操作");
        let body_before = body_bytes(&env, &target_id);
        let row_before = session_row(&env, &target_id);
        let pairs_before = pair_snapshot(&env);
        let baselines_before = env.baseline_files();
        let db = env.paths.workbuddy_db();
        let saved_db = db.with_extension("saved");
        match fault {
            "open" => std::fs::rename(&db, &saved_db).unwrap(),
            "schema" => Connection::open(&db)
                .unwrap()
                .execute_batch("ALTER TABLE sessions RENAME TO unavailable_sessions")
                .unwrap(),
            "query" => Connection::open(&db)
                .unwrap()
                .execute_batch(
                    "ALTER TABLE sessions RENAME COLUMN updated_at TO unavailable_updated_at",
                )
                .unwrap(),
            _ => unreachable!(),
        }
        let existing_db = if fault == "open" { &saved_db } else { &db };
        let db_before = std::fs::read(existing_db).unwrap();
        let recovery = recover_pending_session_operations_at(&env.paths, WbVariant::Cn);
        assert!(recovery.recovered.is_empty(), "{fault}");
        assert!(recovery.abandoned.is_empty(), "{fault}");
        assert_eq!(recovery.needs_recovery.len(), 1, "{fault}");
        assert!(
            crate::modules::switch::recovery_blocks_startup(&recovery),
            "{fault}"
        );
        assert_eq!(body_bytes(&env, &target_id), body_before, "{fault}");
        assert_eq!(std::fs::read(existing_db).unwrap(), db_before, "{fault}");
        assert_eq!(pair_snapshot(&env), pairs_before, "{fault}");
        assert_eq!(env.baseline_files(), baselines_before, "{fault}");
        let pending = session_link::pending_operations(&env.paths, WbVariant::Cn);
        assert_eq!(pending.len(), 1, "{fault}");
        assert_eq!(pending[0].operation_id, operation.operation_id);
        assert_eq!(pending[0].phase, OpPhase::BodyWritten);

        match fault {
            "open" => std::fs::rename(&saved_db, &db).unwrap(),
            "schema" => Connection::open(&db)
                .unwrap()
                .execute_batch("ALTER TABLE unavailable_sessions RENAME TO sessions")
                .unwrap(),
            "query" => Connection::open(&db)
                .unwrap()
                .execute_batch(
                    "ALTER TABLE sessions RENAME COLUMN unavailable_updated_at TO updated_at",
                )
                .unwrap(),
            _ => unreachable!(),
        }
        let recovery = recover_pending_session_operations_at(&env.paths, WbVariant::Cn);
        assert!(
            recovery.is_clean(),
            "{fault}: {:?}",
            recovery.needs_recovery
        );
        assert_eq!(recovery.recovered, vec![operation.operation_id.clone()]);
        assert_eq!(body_bytes(&env, &target_id), body_before);
        // 补完之后才更新目标行与基线：只改 updated_at，身份/标题不动。
        let row_after = session_row(&env, &target_id);
        assert_eq!(row_after.0, row_before.0);
        assert_eq!(row_after.1, row_before.1);
        assert_eq!(row_after.2, row_before.2);
        assert!(row_after.3.unwrap() > row_before.3.unwrap());
        assert_eq!(row_after.4, None);
        assert_eq!(env.baseline_files(), baselines_before + 1);
        assert_ne!(pair_snapshot(&env), pairs_before);
        assert_eq!(sync_operations(&env)[0].phase, OpPhase::Completed);
        assert!(
            !session_backup::transaction_dir(&env.paths, WbVariant::Cn, &operation.operation_id)
                .unwrap()
                .exists(),
            "{fault}: 恢复完成后必须回收临时备份"
        );
    }
}

#[test]
fn sync_row_snapshot_supports_legacy_schema_and_distinguishes_missing_row() {
    let conn = Connection::open_in_memory().unwrap();
    assert!(read_session_row(&conn, "session").is_err());
    conn.execute_batch(
            "CREATE TABLE sessions (id TEXT, user_id TEXT, title TEXT, updated_at INTEGER, deleted_at INTEGER);
             INSERT INTO sessions VALUES ('session', 'uid', 'title', 123, NULL);",
        ).unwrap();
    let row = read_session_row(&conn, "session").unwrap().unwrap();
    assert_eq!(row.custom_title, None);
    assert_eq!(row.title.as_deref(), Some("title"));
    assert!(read_session_row(&conn, "absent").unwrap().is_none());
    conn.execute_batch("UPDATE sessions SET updated_at = 'invalid'")
        .unwrap();
    assert!(read_session_row(&conn, "session").is_err());
}

/// 目标行已不存在（硬删除）→ 无法补完，按备份回滚正文，不留无行的半成品。
#[test]
fn sync_recovery_rolls_back_body_when_target_row_is_gone() {
    let env = ready_env("sync-recovery-row-gone");
    let (target_id, target_before, _) = interrupted_sync_scene(&env);
    let baselines_before = env.baseline_files();
    env.delete_row(&target_id);

    let recovery = recover_pending_session_operations_at(&env.paths, WbVariant::Cn);
    assert_eq!(recovery.abandoned.len(), 1, "{:?}", recovery.recovered);
    assert!(recovery.is_clean(), "{:?}", recovery.needs_recovery);
    assert_eq!(
        body_bytes(&env, &target_id),
        target_before,
        "必须按备份回滚成覆盖前内容"
    );
    assert_eq!(env.baseline_files(), baselines_before, "基线未被提交");
    assert!(session_link::pending_operations(&env.paths, WbVariant::Cn).is_empty());
    assert!(try_session_row(&env, &target_id).is_none(), "目标行已删除");
}

/// 目标行被软删除（deleted_at 非空）→ 同样无法补完，回滚正文并记为放弃。
#[test]
fn sync_recovery_rolls_back_body_when_target_row_is_soft_deleted() {
    let env = ready_env("sync-recovery-row-deleted");
    let (target_id, target_before, _) = interrupted_sync_scene(&env);
    let conn = Connection::open(env.paths.workbuddy_db()).unwrap();
    conn.execute(
        "UPDATE sessions SET deleted_at = 123456 WHERE id = ?1",
        [&target_id],
    )
    .unwrap();
    drop(conn);

    let recovery = recover_pending_session_operations_at(&env.paths, WbVariant::Cn);
    assert_eq!(recovery.abandoned.len(), 1, "{:?}", recovery.recovered);
    assert!(recovery.is_clean(), "{:?}", recovery.needs_recovery);
    assert_eq!(body_bytes(&env, &target_id), target_before);
    assert!(session_link::pending_operations(&env.paths, WbVariant::Cn).is_empty());
}

/// 同一请求里重复勾选同一组：第一次写入后凭据即失效，第二次跳过，不重复写。
#[test]
fn sync_duplicate_selection_in_one_request_writes_once() {
    let env = ready_env("sync-duplicate-selection");
    let (group_id, token, target_id) = fast_forward_scene(&env);
    let baselines_before = env.baseline_files();
    let expected = incoming_text(&env, "sess-1", &target_id);
    let once = selection(&group_id, &token, SyncMode::FastForward);

    let report = sync(&env, "uid-b", &[once.clone(), once]);
    assert_eq!(report["synced"].as_array().unwrap().len(), 1, "{report}");
    assert!(report["errors"].as_array().unwrap().is_empty(), "{report}");
    let skipped = report["skipped"].as_array().unwrap();
    assert_eq!(skipped.len(), 1, "{report}");
    assert_eq!(skipped[0]["reasonCode"], REASON_PREVIEW_STALE);
    assert_eq!(
        std::fs::read_to_string(env.body_path(&target_id)).unwrap(),
        expected
    );
    assert_eq!(env.body_files().len(), 2, "不得产生第二份副本");
    assert_eq!(env.baseline_files(), baselines_before + 1, "只提交一次基线");
    assert_eq!(sync_operations(&env).len(), 1, "只留一条同步操作");
}

/// 同一目标仍有未完成写入时，本轮不得再写一次（等恢复完成）。
#[test]
fn sync_refuses_to_write_while_previous_operation_is_unfinished() {
    let env = ready_env("sync-pending-guard");
    let (group_id, token, target_id) = fast_forward_scene(&env);
    let target_before = body_bytes(&env, &target_id);
    let paths = env.paths();
    // 手工留下一条缺备份清单的未完成同步操作：恢复无法完成它，只能保持 pending。
    session_link::save_operation(
        &paths,
        &Operation {
            version: OPERATION_VERSION,
            operation_id: "op-sync-pending".to_string(),
            kind: OPERATION_KIND_SYNC.to_string(),
            variant: WbVariant::Cn,
            source_variant: None,
            group_id: group_id.clone(),
            source: OperationMember {
                account_id: None,
                uid: "uid-a".to_string(),
                session_id: "sess-1".to_string(),
            },
            target: OperationMember {
                account_id: None,
                uid: "uid-b".to_string(),
                session_id: target_id.clone(),
            },
            expected_content_digest: "d".to_string(),
            expected_record_count: 1,
            phase: OpPhase::Prepared,
            backup: None,
            lifecycle_version: None,
            cleanup_state: None,
            last_error: None,
            created_at: 1,
            updated_at: 1,
        },
    )
    .unwrap();

    let report = sync(
        &env,
        "uid-b",
        &[selection(&group_id, &token, SyncMode::FastForward)],
    );
    assert!(report["synced"].as_array().unwrap().is_empty(), "{report}");
    assert_eq!(report["needsRecovery"], true, "{report}");
    assert_eq!(report["errors"].as_array().unwrap().len(), 1, "{report}");
    let error = report["errors"][0]["error"].as_str().unwrap();
    assert!(error.contains("上一次会话保存尚未完成"), "{error}");
    assert_eq!(body_bytes(&env, &target_id), target_before);
    assert_eq!(env.body_files().len(), 2);
    assert_eq!(
        session_link::pending_operations(&paths, WbVariant::Cn).len(),
        1,
        "旧操作保留待恢复，不新增待写操作"
    );
}

/// 同步的备份清单被删/为空目录时不得盲目重放。
#[test]
fn sync_recovery_stops_without_usable_backup() {
    let env = ready_env("sync-recovery-no-backup");
    let (group_id, _token, target_id) = fast_forward_scene(&env);
    let target_before = body_bytes(&env, &target_id);
    let paths = env.paths();
    session_link::save_operation(
        &paths,
        &Operation {
            version: OPERATION_VERSION,
            operation_id: "op-sync-no-backup".to_string(),
            kind: OPERATION_KIND_SYNC.to_string(),
            variant: WbVariant::Cn,
            source_variant: None,
            group_id,
            source: OperationMember {
                account_id: None,
                uid: "uid-a".to_string(),
                session_id: "sess-1".to_string(),
            },
            target: OperationMember {
                account_id: None,
                uid: "uid-b".to_string(),
                session_id: target_id.clone(),
            },
            expected_content_digest: "d".to_string(),
            expected_record_count: 1,
            phase: OpPhase::BodyWritten,
            backup: Some(
                env.paths
                    .backup_root()
                    .join("session-transactions/cn/does-not-exist/manifest.json")
                    .to_string_lossy()
                    .to_string(),
            ),
            lifecycle_version: Some(OPERATION_LIFECYCLE_VERSION),
            cleanup_state: None,
            last_error: None,
            created_at: 1,
            updated_at: 1,
        },
    )
    .unwrap();

    let recovery = recover_pending_session_operations_at(&paths, WbVariant::Cn);
    assert!(recovery.recovered.is_empty(), "{:?}", recovery.recovered);
    assert_eq!(recovery.needs_recovery.len(), 1);
    assert!(!recovery.needs_recovery[0].retryable);
    assert!(
        recovery.needs_recovery[0].reason.contains("备份"),
        "{}",
        recovery.needs_recovery[0].reason
    );
    assert_eq!(body_bytes(&env, &target_id), target_before);
}

#[test]
fn equivalent_existing_target_is_really_linked() {
    let env = ready_env("equivalent-link");
    env.add_session("sess-b", "uid-b", "旧副本");
    env.add_body("sess-b", &body_text("sess-b"));

    let report = copy(&env, "uid-b", &["sess-1"]);
    assert_eq!(report["alreadyLinked"][0]["sessionId"], "sess-b");

    let store = match session_link::load_store(&env.paths()) {
        StoreState::Ready(store) => store,
        other => panic!("store not ready: {other:?}"),
    };
    let group = store
        .groups
        .iter()
        .find(|group| {
            group.members.iter().any(|member| {
                member.uid == "uid-b"
                    && member.session_id == "sess-b"
                    && member.state == MemberState::Active
            })
        })
        .expect("target member must be linked");
    assert!(group.members.iter().any(|member| {
        member.uid == "uid-a"
            && member.session_id == "sess-1"
            && member.state == MemberState::Active
    }));
    assert!(!group.pair_bases.is_empty());
}

#[test]
fn dedup_soft_deletes_rows_but_keeps_jsonl_bodies() {
    let env = ready_env("dedup-soft-delete");
    env.add_session("sess-2", "uid-a", "标题一");
    env.add_body("sess-2", &body_text("sess-2"));
    let before = env.body_files().len();

    let report = dedup_sessions_for_user_at(&env.paths(), WbVariant::Cn, "uid-a");
    assert!(report["removed"].as_u64().unwrap_or(0) >= 1, "{report}");

    // 严重回归门禁：去重只能软删数据库行，正文文件必须全部保留。
    assert_eq!(env.body_files().len(), before);
    let conn = Connection::open(env.paths.workbuddy_db()).unwrap();
    let deleted: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sessions WHERE user_id = 'uid-a' AND deleted_at IS NOT NULL",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(deleted, 1);
}
