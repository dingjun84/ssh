//! Session library backed by SQLite.
//!
//! Passwords never touch the database: the `sessions` table only records
//! *where* a secret lives (a Keychain account name), see [`crate::secrets`].

use std::path::PathBuf;

use rusqlite::{params, Connection, OptionalExtension};

/// How a session authenticates. The secret itself lives in the Keychain.
#[derive(Debug, Clone, PartialEq)]
pub enum AuthKind {
    Key { path: String },
    Password,
}

impl AuthKind {
    pub fn label(&self) -> &'static str {
        match self {
            AuthKind::Key { .. } => "私钥",
            AuthKind::Password => "密码",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Session {
    pub id: i64,
    pub group: String,
    pub label: String,
    pub host: String,
    pub port: u16,
    pub user: String,
    pub auth: AuthKind,
}

impl Session {
    /// `root · 私钥`, shown under the label in the sidebar.
    pub fn subtitle(&self) -> String {
        let mut s = format!("{} · {}", self.user, self.auth.label());
        if self.port != 22 {
            s.push_str(&format!(" · :{}", self.port));
        }
        s
    }
}

pub struct Store {
    conn: Connection,
}

impl Store {
    pub fn open_default() -> Result<Self, String> {
        let path = db_path()?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)
                .map_err(|e| format!("创建配置目录 {} 失败: {e}", dir.display()))?;
        }
        Self::open(&path)
    }

    pub fn open(path: &std::path::Path) -> Result<Self, String> {
        let conn = Connection::open(path)
            .map_err(|e| format!("打开会话库 {} 失败: {e}", path.display()))?;
        conn.pragma_update(None, "journal_mode", "WAL")
            .map_err(|e| format!("设置 WAL 失败: {e}"))?;
        conn.pragma_update(None, "foreign_keys", "ON")
            .map_err(|e| format!("启用外键失败: {e}"))?;

        let store = Store { conn };
        store.migrate()?;
        Ok(store)
    }

    fn migrate(&self) -> Result<(), String> {
        self.conn
            .execute_batch(
                r#"
                CREATE TABLE IF NOT EXISTS groups (
                    id         INTEGER PRIMARY KEY,
                    name       TEXT NOT NULL UNIQUE,
                    sort_order INTEGER NOT NULL DEFAULT 0
                );

                CREATE TABLE IF NOT EXISTS sessions (
                    id            INTEGER PRIMARY KEY,
                    group_id      INTEGER NOT NULL REFERENCES groups(id) ON DELETE CASCADE,
                    label         TEXT NOT NULL,
                    host          TEXT NOT NULL,
                    port          INTEGER NOT NULL DEFAULT 22,
                    user          TEXT NOT NULL,
                    auth_kind     TEXT NOT NULL CHECK (auth_kind IN ('key', 'password')),
                    key_path      TEXT,
                    sort_order    INTEGER NOT NULL DEFAULT 0,
                    connect_count INTEGER NOT NULL DEFAULT 0,
                    last_used_at  TEXT
                );

                CREATE INDEX IF NOT EXISTS idx_sessions_group ON sessions(group_id, sort_order);
                "#,
            )
            .map_err(|e| format!("初始化会话库失败: {e}"))?;
        Ok(())
    }

    /// True when the library has never been populated, which is when we seed
    /// it with the host this project was developed against.
    fn is_empty(&self) -> Result<bool, String> {
        let count: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM sessions", [], |row| row.get(0))
            .map_err(|e| format!("统计会话失败: {e}"))?;
        Ok(count == 0)
    }

    pub fn seed_if_empty(&self, sessions: &[NewSession]) -> Result<(), String> {
        if !self.is_empty()? {
            return Ok(());
        }
        for session in sessions {
            self.insert(session)?;
        }
        Ok(())
    }

    pub fn insert(&self, session: &NewSession) -> Result<i64, String> {
        let group_id = self.ensure_group(&session.group)?;
        self.conn
            .execute(
                r#"
                INSERT INTO sessions
                    (group_id, label, host, port, user, auth_kind, key_path, sort_order)
                VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                "#,
                params![
                    group_id,
                    session.label,
                    session.host,
                    session.port as i64,
                    session.user,
                    match session.auth {
                        AuthKind::Key { .. } => "key",
                        AuthKind::Password => "password",
                    },
                    match &session.auth {
                        AuthKind::Key { path } => Some(path.clone()),
                        AuthKind::Password => None,
                    },
                    self.next_sort_order(group_id)?,
                ],
            )
            .map_err(|e| format!("写入会话失败: {e}"))?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Rewrites a session in place. The secret is untouched — that lives in the
    /// Keychain and is only replaced when the user types a new one.
    pub fn update(&self, id: i64, session: &NewSession) -> Result<(), String> {
        let group_id = self.ensure_group(&session.group)?;
        let changed = self
            .conn
            .execute(
                r#"
                UPDATE sessions
                   SET group_id = ?1, label = ?2, host = ?3, port = ?4,
                       user = ?5, auth_kind = ?6, key_path = ?7
                 WHERE id = ?8
                "#,
                params![
                    group_id,
                    session.label,
                    session.host,
                    session.port as i64,
                    session.user,
                    match session.auth {
                        AuthKind::Key { .. } => "key",
                        AuthKind::Password => "password",
                    },
                    match &session.auth {
                        AuthKind::Key { path } => Some(path.clone()),
                        AuthKind::Password => None,
                    },
                    id,
                ],
            )
            .map_err(|e| format!("更新会话失败: {e}"))?;
        if changed == 0 {
            return Err(format!("会话 {id} 不存在"));
        }
        Ok(())
    }

    pub fn delete(&self, id: i64) -> Result<(), String> {
        self.conn
            .execute("DELETE FROM sessions WHERE id = ?1", params![id])
            .map_err(|e| format!("删除会话失败: {e}"))?;
        Ok(())
    }

    pub fn touch(&self, id: i64) -> Result<(), String> {
        self.conn
            .execute(
                "UPDATE sessions
                    SET connect_count = connect_count + 1,
                        last_used_at  = datetime('now')
                  WHERE id = ?1",
                params![id],
            )
            .map_err(|e| format!("更新会话统计失败: {e}"))?;
        Ok(())
    }

    fn ensure_group(&self, name: &str) -> Result<i64, String> {
        if let Some(id) = self
            .conn
            .query_row("SELECT id FROM groups WHERE name = ?1", params![name], |row| {
                row.get::<_, i64>(0)
            })
            .optional()
            .map_err(|e| format!("查询分组失败: {e}"))?
        {
            return Ok(id);
        }
        let order: i64 = self
            .conn
            .query_row(
                "SELECT COALESCE(MAX(sort_order), -1) + 1 FROM groups",
                [],
                |row| row.get(0),
            )
            .unwrap_or(0);
        self.conn
            .execute(
                "INSERT INTO groups (name, sort_order) VALUES (?1, ?2)",
                params![name, order],
            )
            .map_err(|e| format!("创建分组失败: {e}"))?;
        Ok(self.conn.last_insert_rowid())
    }

    fn next_sort_order(&self, group_id: i64) -> Result<i64, String> {
        self.conn
            .query_row(
                "SELECT COALESCE(MAX(sort_order), -1) + 1 FROM sessions WHERE group_id = ?1",
                params![group_id],
                |row| row.get(0),
            )
            .map_err(|e| format!("计算排序失败: {e}"))
    }

    /// Grouped view for the sidebar, preserving the stored order.
    pub fn groups(&self) -> Result<Vec<(String, Vec<Session>)>, String> {
        let mut stmt = self
            .conn
            .prepare(
                r#"
                SELECT s.id, g.name, s.label, s.host, s.port, s.user, s.auth_kind, s.key_path
                  FROM sessions s
                  JOIN groups g ON g.id = s.group_id
                 ORDER BY g.sort_order, g.name, s.sort_order, s.id
                "#,
            )
            .map_err(|e| format!("读取会话失败: {e}"))?;

        let rows = stmt
            .query_map([], |row| {
                let auth_kind: String = row.get(6)?;
                let key_path: Option<String> = row.get(7)?;
                Ok(Session {
                    id: row.get(0)?,
                    group: row.get(1)?,
                    label: row.get(2)?,
                    host: row.get(3)?,
                    port: row.get::<_, i64>(4)? as u16,
                    user: row.get(5)?,
                    auth: match (auth_kind.as_str(), key_path) {
                        ("key", Some(path)) => AuthKind::Key { path },
                        ("key", None) => AuthKind::Key {
                            path: "~/.ssh/id_rsa".to_string(),
                        },
                        _ => AuthKind::Password,
                    },
                })
            })
            .map_err(|e| format!("读取会话失败: {e}"))?;

        let mut grouped: Vec<(String, Vec<Session>)> = Vec::new();
        for row in rows {
            let session = row.map_err(|e| format!("读取会话失败: {e}"))?;
            match grouped.last_mut() {
                Some((name, list)) if *name == session.group => list.push(session),
                _ => grouped.push((session.group.clone(), vec![session])),
            }
        }
        Ok(grouped)
    }

    #[cfg(test)]
    pub fn get(&self, id: i64) -> Result<Option<Session>, String> {
        Ok(self
            .groups()?
            .into_iter()
            .flat_map(|(_, list)| list)
            .find(|s| s.id == id))
    }
}

/// A session as it is created in the UI, before it has a row id.
#[derive(Debug, Clone)]
pub struct NewSession {
    pub group: String,
    pub label: String,
    pub host: String,
    pub port: u16,
    pub user: String,
    pub auth: AuthKind,
}

fn db_path() -> Result<PathBuf, String> {
    // `SSH_CLIENT_DB` keeps smoke tests off the real profile.
    if let Some(path) = std::env::var_os("SSH_CLIENT_DB") {
        return Ok(PathBuf::from(path));
    }
    let home = std::env::var_os("HOME").ok_or("拿不到 HOME 目录")?;
    Ok(PathBuf::from(home)
        .join("Library")
        .join("Application Support")
        .join("ssh-client")
        .join("sessions.db"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    struct TempDir(PathBuf);

    /// `SystemTime` on macOS only has microsecond resolution, so two tests
    /// starting at once could pick the same directory and then delete each
    /// other's database. A process-local counter keeps them apart.
    static SEQ: AtomicU64 = AtomicU64::new(0);

    impl TempDir {
        fn new() -> Self {
            let seq = SEQ.fetch_add(1, Ordering::SeqCst);
            let path = std::env::temp_dir()
                .join(format!("ssh-client-store-{}-{seq}", std::process::id()));
            std::fs::create_dir_all(&path).unwrap();
            TempDir(path)
        }

        fn path(&self) -> &std::path::Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn temp_store() -> (TempDir, Store) {
        let dir = TempDir::new();
        let path = dir.path().join("s.db");
        let store = Store::open(&path).unwrap();
        (dir, store)
    }

    #[test]
    fn update_moves_group_and_switches_auth() {
        let (_dir, store) = temp_store();
        let id = store
            .insert(&NewSession {
                group: "生产".into(),
                label: "old".into(),
                host: "a".into(),
                port: 22,
                user: "root".into(),
                auth: AuthKind::Key {
                    path: "~/.ssh/id_rsa".into(),
                },
            })
            .unwrap();

        store
            .update(
                id,
                &NewSession {
                    group: "测试".into(),
                    label: "new".into(),
                    host: "b".into(),
                    port: 2200,
                    user: "deploy".into(),
                    auth: AuthKind::Password,
                },
            )
            .unwrap();

        let updated = store.get(id).unwrap().unwrap();
        assert_eq!(updated.group, "测试");
        assert_eq!(updated.label, "new");
        assert_eq!(updated.host, "b");
        assert_eq!(updated.port, 2200);
        assert_eq!(updated.auth, AuthKind::Password);

        // The old group is left behind only because it is now empty; it must
        // not be deleted out from under other sessions.
        assert_eq!(store.groups().unwrap().len(), 1);
    }

    #[test]
    fn groups_sessions_by_name() {
        let (_dir, store) = temp_store();
        let mk = |group: &str, label: &str| NewSession {
            group: group.into(),
            label: label.into(),
            host: "h".into(),
            port: 22,
            user: "root".into(),
            auth: AuthKind::Password,
        };
        store.insert(&mk("生产", "a")).unwrap();
        store.insert(&mk("测试", "b")).unwrap();
        store.insert(&mk("生产", "c")).unwrap();

        let groups = store.groups().unwrap();
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].0, "生产");
        assert_eq!(groups[0].1.len(), 2);
        assert_eq!(groups[1].0, "测试");
    }

    #[test]
    fn key_auth_roundtrips() {
        let (_dir, store) = temp_store();
        let id = store
            .insert(&NewSession {
                group: "g".into(),
                label: "l".into(),
                host: "h".into(),
                port: 2222,
                user: "u".into(),
                auth: AuthKind::Key {
                    path: "~/.ssh/id_ed25519".into(),
                },
            })
            .unwrap();
        let session = store.get(id).unwrap().unwrap();
        assert_eq!(session.port, 2222);
        assert_eq!(
            session.auth,
            AuthKind::Key {
                path: "~/.ssh/id_ed25519".into()
            }
        );
    }
}
