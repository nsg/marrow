use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Once;

use rusqlite::Connection;
use serde::Serialize;

static INIT_VEC: Once = Once::new();

fn init_sqlite_vec() {
    INIT_VEC.call_once(|| unsafe {
        #[allow(clippy::missing_transmute_annotations)]
        rusqlite::ffi::sqlite3_auto_extension(Some(std::mem::transmute(
            sqlite_vec::sqlite3_vec_init as *const (),
        )));
    });
}

fn open_readonly(memory_dir: &Path) -> Option<Connection> {
    init_sqlite_vec();
    let db_path = memory_dir.join("memory.db");
    Connection::open_with_flags(&db_path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).ok()
}

#[derive(Serialize, Default)]
pub struct MemoryStats {
    pub total: usize,
    pub auto_count: usize,
    pub user_count: usize,
    pub embedded_count: usize,
    pub memories: Vec<MemoryRow>,
    pub cluster_summaries: HashMap<usize, String>,
    #[serde(skip)]
    pub row_count: usize,
}

#[derive(Serialize, Clone)]
pub struct MemoryRow {
    pub id: String,
    pub fact: String,
    pub source: String,
    pub created: String,
    pub has_embedding: bool,
    pub cluster: usize,
}

impl MemoryStats {
    pub fn needs_reload(&self, memory_dir: &Path) -> bool {
        let Some(conn) = open_readonly(memory_dir) else {
            return self.row_count > 0;
        };
        let current = count(&conn, "SELECT COUNT(*) FROM memories");
        current != self.row_count
    }

    pub fn load(memory_dir: &Path) -> Self {
        let Some(conn) = open_readonly(memory_dir) else {
            return Self::default();
        };

        let total = count(&conn, "SELECT COUNT(*) FROM memories");
        let auto_count = count(&conn, "SELECT COUNT(*) FROM memories WHERE source = 'auto'");
        let user_count = count(&conn, "SELECT COUNT(*) FROM memories WHERE source = 'user'");
        let embedded_count = count(&conn, "SELECT COUNT(*) FROM vec_memories");

        let embedded_ids = embedded_id_set(&conn);

        let mut memories = Vec::new();
        if let Ok(mut stmt) =
            conn.prepare("SELECT id, fact, source, created FROM memories ORDER BY created DESC")
            && let Ok(rows) = stmt.query_map([], |row| {
                let id: String = row.get(0)?;
                let fact: String = row.get(1)?;
                let source: String = row.get(2)?;
                let created: String = row.get(3)?;
                Ok((id, fact, source, created))
            })
        {
            for row in rows.flatten() {
                let has_embedding = embedded_ids.contains(&row.0);
                memories.push(MemoryRow {
                    id: row.0,
                    fact: row.1,
                    source: row.2,
                    created: row.3,
                    has_embedding,
                    cluster: 0,
                });
            }
        }

        // Load cluster assignments from DB
        let mut cluster_map: HashMap<String, (usize, String)> = HashMap::new();
        if let Ok(mut stmt) =
            conn.prepare("SELECT cluster_id, memory_id, summary FROM memory_clusters")
            && let Ok(rows) = stmt.query_map([], |row| {
                Ok((
                    row.get::<_, i64>(0)? as usize,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })
        {
            for row in rows.flatten() {
                cluster_map.insert(row.1, (row.0, row.2));
            }
        }
        let mut next_cluster = cluster_map.values().map(|(id, _)| *id).max().unwrap_or(0) + 1;
        for row in &mut memories {
            if let Some((cid, _)) = cluster_map.get(&row.id) {
                row.cluster = *cid;
            } else {
                row.cluster = next_cluster;
                next_cluster += 1;
            }
        }

        let mut cluster_summaries: HashMap<usize, String> = HashMap::new();
        for (cid, summary) in cluster_map.values() {
            cluster_summaries
                .entry(*cid)
                .or_insert_with(|| summary.clone());
        }

        Self {
            total,
            auto_count,
            user_count,
            embedded_count,
            row_count: total,
            memories,
            cluster_summaries,
        }
    }

    pub fn search(&self, query: &str) -> Vec<&MemoryRow> {
        let q = query.to_lowercase();
        self.memories
            .iter()
            .filter(|m| m.fact.to_lowercase().contains(&q))
            .collect()
    }

    pub fn search_by_embedding(
        memory_dir: &Path,
        query_embedding: &[f32],
        limit: usize,
    ) -> Vec<MemoryRow> {
        let Some(conn) = open_readonly(memory_dir) else {
            return Vec::new();
        };

        let blob: Vec<u8> = query_embedding
            .iter()
            .flat_map(|f| f.to_le_bytes())
            .collect();

        let Ok(mut stmt) = conn.prepare(
            "SELECT m.id, m.fact, m.source, m.created, v.distance
             FROM vec_memories v
             JOIN memories m ON m.id = v.id
             WHERE v.embedding MATCH ?1
             AND k = ?2
             ORDER BY v.distance",
        ) else {
            return Vec::new();
        };

        let Ok(rows) = stmt.query_map(rusqlite::params![blob, limit as i64], |row| {
            let id: String = row.get(0)?;
            let fact: String = row.get(1)?;
            let source: String = row.get(2)?;
            let created: String = row.get(3)?;
            Ok(MemoryRow {
                id,
                fact,
                source,
                created,
                has_embedding: true,
                cluster: 0,
            })
        }) else {
            return Vec::new();
        };

        rows.flatten().collect()
    }
}

fn count(conn: &Connection, sql: &str) -> usize {
    conn.query_row(sql, [], |r| r.get::<_, i64>(0).map(|v| v as usize))
        .unwrap_or(0)
}

fn embedded_id_set(conn: &Connection) -> HashSet<String> {
    let mut set = HashSet::new();
    if let Ok(mut stmt) = conn.prepare("SELECT id FROM vec_memories")
        && let Ok(rows) = stmt.query_map([], |row| row.get::<_, String>(0))
    {
        for id in rows.flatten() {
            set.insert(id);
        }
    }
    set
}
