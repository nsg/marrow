use std::path::Path;

use rusqlite::Connection;
use serde::Serialize;

#[derive(Clone, Serialize)]
pub struct KvItem {
    pub key: String,
    pub value: String,
    pub updated: String,
}

#[derive(Clone, Serialize, Default)]
pub struct KvData {
    pub entries: Vec<KvItem>,
}

impl KvData {
    pub fn load(memory_dir: &Path) -> Self {
        let db_path = memory_dir.join("memory.db");
        let Ok(conn) =
            Connection::open_with_flags(&db_path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
        else {
            return Self::default();
        };

        let mut entries = Vec::new();
        if let Ok(mut stmt) = conn.prepare("SELECT key, value, updated FROM kv_state ORDER BY key")
            && let Ok(rows) = stmt.query_map([], |row| {
                Ok(KvItem {
                    key: row.get(0)?,
                    value: row.get(1)?,
                    updated: row.get(2)?,
                })
            })
        {
            for row in rows.flatten() {
                entries.push(row);
            }
        }

        Self { entries }
    }
}
