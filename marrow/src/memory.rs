use std::error::Error;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

const DEFAULT_EMBEDDING_DIMENSIONS: usize = 768;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Memory {
    pub id: Uuid,
    pub fact: String,
    pub source: MemorySource,
    pub created: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemorySource {
    Auto,
    User,
}

impl MemorySource {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::User => "user",
        }
    }

    pub fn from_db_str(s: &str) -> Result<Self, Box<dyn Error + Send + Sync>> {
        match s {
            "auto" => Ok(Self::Auto),
            "user" => Ok(Self::User),
            other => Err(format!("unknown memory source: {other}").into()),
        }
    }
}

impl Memory {
    pub fn new(fact: impl Into<String>, source: MemorySource) -> Self {
        Self {
            id: Uuid::new_v4(),
            fact: fact.into(),
            source,
            created: now_iso(),
        }
    }
}

pub fn now_iso() -> String {
    let dur = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap();
    let secs = dur.as_secs();
    let days = secs / 86400;
    let time = secs % 86400;
    let hours = time / 3600;
    let minutes = (time % 3600) / 60;
    let seconds = time % 60;

    // Approximate date calculation (good enough for timestamps)
    let mut y = 1970i64;
    let mut remaining = days as i64;
    loop {
        let days_in_year = if y % 4 == 0 && (y % 100 != 0 || y % 400 == 0) {
            366
        } else {
            365
        };
        if remaining < days_in_year {
            break;
        }
        remaining -= days_in_year;
        y += 1;
    }
    let leap = y % 4 == 0 && (y % 100 != 0 || y % 400 == 0);
    let month_days = [
        31,
        if leap { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    let mut m = 0;
    for md in &month_days {
        if remaining < *md {
            break;
        }
        remaining -= md;
        m += 1;
    }

    format!(
        "{y:04}-{:02}-{:02}T{hours:02}:{minutes:02}:{seconds:02}Z",
        m + 1,
        remaining + 1
    )
}

fn init_sqlite_vec() {
    use std::sync::Once;
    static INIT: Once = Once::new();
    INIT.call_once(|| unsafe {
        #[allow(clippy::missing_transmute_annotations)]
        rusqlite::ffi::sqlite3_auto_extension(Some(std::mem::transmute(
            sqlite_vec::sqlite3_vec_init as *const (),
        )));
    });
}

fn open_db(path: &Path) -> Result<Connection, Box<dyn Error + Send + Sync>> {
    init_sqlite_vec();
    let conn = Connection::open(path)?;
    conn.execute_batch(
        "PRAGMA journal_mode = WAL;
         PRAGMA busy_timeout = 5000;",
    )?;
    conn.execute(
        "CREATE TABLE IF NOT EXISTS memories (
            id      TEXT PRIMARY KEY NOT NULL,
            fact    TEXT NOT NULL,
            source  TEXT NOT NULL,
            created TEXT NOT NULL
        )",
        [],
    )?;
    conn.execute(
        "CREATE TABLE IF NOT EXISTS metadata (
            key   TEXT PRIMARY KEY NOT NULL,
            value TEXT NOT NULL
        )",
        [],
    )?;
    // Create vec_memories with stored dimension (or default if first run)
    let dim = get_embedding_dim(&conn);
    ensure_vec_table(&conn, dim)?;
    Ok(conn)
}

fn get_embedding_dim(conn: &Connection) -> usize {
    conn.query_row(
        "SELECT value FROM metadata WHERE key = 'embedding_dim'",
        [],
        |row| {
            let s: String = row.get(0)?;
            Ok(s.parse::<usize>().unwrap_or(DEFAULT_EMBEDDING_DIMENSIONS))
        },
    )
    .unwrap_or(DEFAULT_EMBEDDING_DIMENSIONS)
}

fn has_embedding_dim(conn: &Connection) -> bool {
    conn.query_row(
        "SELECT 1 FROM metadata WHERE key = 'embedding_dim'",
        [],
        |_| Ok(()),
    )
    .is_ok()
}

fn set_embedding_dim(conn: &Connection, dim: usize) -> Result<(), Box<dyn Error + Send + Sync>> {
    conn.execute(
        "INSERT OR REPLACE INTO metadata (key, value) VALUES ('embedding_dim', ?1)",
        rusqlite::params![dim.to_string()],
    )?;
    Ok(())
}

fn ensure_vec_table(conn: &Connection, dim: usize) -> Result<(), Box<dyn Error + Send + Sync>> {
    conn.execute(
        &format!(
            "CREATE VIRTUAL TABLE IF NOT EXISTS vec_memories USING vec0(
                id TEXT PRIMARY KEY,
                embedding float[{dim}]
            )"
        ),
        [],
    )?;
    Ok(())
}

fn row_to_memory(row: &rusqlite::Row) -> rusqlite::Result<Memory> {
    let id_str: String = row.get(0)?;
    let fact: String = row.get(1)?;
    let source_str: String = row.get(2)?;
    let created: String = row.get(3)?;
    let id = id_str.parse::<Uuid>().map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
    })?;
    let source = MemorySource::from_db_str(&source_str).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(2, rusqlite::types::Type::Text, e)
    })?;
    Ok(Memory {
        id,
        fact,
        source,
        created,
    })
}

pub struct MemoryStore {
    #[allow(dead_code)]
    dir: PathBuf,
    conn: Mutex<Connection>,
}

pub fn embedding_to_blob(embedding: &[f32]) -> Vec<u8> {
    embedding.iter().flat_map(|f| f.to_le_bytes()).collect()
}

impl MemoryStore {
    pub fn new(dir: impl Into<PathBuf>) -> Result<Self, Box<dyn Error + Send + Sync>> {
        let dir = dir.into();
        std::fs::create_dir_all(&dir)?;
        let db_path = dir.join("memory.db");
        let conn = open_db(&db_path)?;
        Ok(Self {
            dir,
            conn: Mutex::new(conn),
        })
    }

    pub fn save(&self, memory: &Memory) -> Result<(), Box<dyn Error + Send + Sync>> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO memories (id, fact, source, created) VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![
                memory.id.to_string(),
                memory.fact,
                memory.source.as_str(),
                memory.created,
            ],
        )?;
        Ok(())
    }

    pub fn load(&self, id: Uuid) -> Result<Memory, Box<dyn Error + Send + Sync>> {
        let conn = self.conn.lock().unwrap();
        let memory = conn.query_row(
            "SELECT id, fact, source, created FROM memories WHERE id = ?1",
            rusqlite::params![id.to_string()],
            row_to_memory,
        )?;
        Ok(memory)
    }

    pub fn delete(&self, id: Uuid) -> Result<(), Box<dyn Error + Send + Sync>> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "DELETE FROM memories WHERE id = ?1",
            rusqlite::params![id.to_string()],
        )?;
        conn.execute(
            "DELETE FROM vec_memories WHERE id = ?1",
            rusqlite::params![id.to_string()],
        )?;
        Ok(())
    }

    pub fn update(&self, id: Uuid, new_fact: String) -> Result<(), Box<dyn Error + Send + Sync>> {
        self.update_with_source(id, new_fact, None)
    }

    pub fn update_with_source(
        &self,
        id: Uuid,
        new_fact: String,
        source: Option<MemorySource>,
    ) -> Result<(), Box<dyn Error + Send + Sync>> {
        let conn = self.conn.lock().unwrap();
        let changed = if let Some(src) = source {
            conn.execute(
                "UPDATE memories SET fact = ?1, source = ?2 WHERE id = ?3",
                rusqlite::params![new_fact, src.as_str(), id.to_string()],
            )?
        } else {
            conn.execute(
                "UPDATE memories SET fact = ?1 WHERE id = ?2",
                rusqlite::params![new_fact, id.to_string()],
            )?
        };
        if changed == 0 {
            return Err(format!("memory not found: {id}").into());
        }
        // Invalidate embedding since the fact text changed
        conn.execute(
            "DELETE FROM vec_memories WHERE id = ?1",
            rusqlite::params![id.to_string()],
        )?;
        Ok(())
    }

    pub fn list(&self) -> Result<Vec<Memory>, Box<dyn Error + Send + Sync>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt =
            conn.prepare("SELECT id, fact, source, created FROM memories ORDER BY created")?;
        let memories = stmt
            .query_map([], row_to_memory)?
            .filter_map(|r| r.ok())
            .collect();
        Ok(memories)
    }

    pub fn search(
        &self,
        query: &str,
        offset: usize,
        limit: usize,
    ) -> Result<(Vec<Memory>, usize), Box<dyn Error + Send + Sync>> {
        let conn = self.conn.lock().unwrap();
        let pattern = format!("%{query}%");

        let total: i64 = conn.query_row(
            "SELECT COUNT(*) FROM memories WHERE fact LIKE ?1",
            rusqlite::params![pattern],
            |row| row.get(0),
        )?;

        let mut stmt = conn.prepare(
            "SELECT id, fact, source, created FROM memories WHERE fact LIKE ?1 ORDER BY created LIMIT ?2 OFFSET ?3",
        )?;
        let results = stmt
            .query_map(
                rusqlite::params![pattern, limit as i64, offset as i64],
                row_to_memory,
            )?
            .filter_map(|r| r.ok())
            .collect();

        Ok((results, total as usize))
    }

    pub fn set_embedding(
        &self,
        id: Uuid,
        embedding: &[f32],
    ) -> Result<(), Box<dyn Error + Send + Sync>> {
        let conn = self.conn.lock().unwrap();
        let new_dim = embedding.len();
        let stored_dim = get_embedding_dim(&conn);

        if new_dim != stored_dim {
            // Dimension mismatch — could be first real embedding (overriding
            // the default) or a model change. Either way, rebuild.
            if has_embedding_dim(&conn) {
                eprintln!(
                    "[marrow] embedding dimension changed ({stored_dim} → {new_dim}), rebuilding vector index"
                );
            }
            conn.execute("DROP TABLE IF EXISTS vec_memories", [])?;
            ensure_vec_table(&conn, new_dim)?;
            set_embedding_dim(&conn, new_dim)?;
        }

        let blob = embedding_to_blob(embedding);
        conn.execute(
            "INSERT OR REPLACE INTO vec_memories (id, embedding) VALUES (?1, ?2)",
            rusqlite::params![id.to_string(), blob],
        )?;
        Ok(())
    }

    pub fn nearest(
        &self,
        query_embedding: &[f32],
        limit: usize,
    ) -> Result<Vec<(Memory, f32)>, Box<dyn Error + Send + Sync>> {
        let conn = self.conn.lock().unwrap();
        let blob = embedding_to_blob(query_embedding);
        let mut stmt = conn.prepare(
            "SELECT m.id, m.fact, m.source, m.created, v.distance
             FROM vec_memories v
             JOIN memories m ON m.id = v.id
             WHERE v.embedding MATCH ?1
             AND k = ?2
             ORDER BY v.distance",
        )?;
        let results = stmt
            .query_map(rusqlite::params![blob, limit as i64], |row| {
                let memory = row_to_memory(row)?;
                let distance: f32 = row.get(4)?;
                Ok((memory, distance))
            })?
            .filter_map(|r| r.ok())
            .collect();
        Ok(results)
    }

    pub fn unembedded(&self) -> Result<Vec<Memory>, Box<dyn Error + Send + Sync>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT m.id, m.fact, m.source, m.created
             FROM memories m
             LEFT JOIN vec_memories v ON m.id = v.id
             WHERE v.id IS NULL",
        )?;
        let results = stmt
            .query_map([], row_to_memory)?
            .filter_map(|r| r.ok())
            .collect();
        Ok(results)
    }
}

// -- Migrations --

pub fn migrate_json_to_sqlite(
    json_dir: &Path,
    store: &MemoryStore,
) -> Result<u32, Box<dyn Error + Send + Sync>> {
    if !json_dir.exists() {
        return Ok(0);
    }

    let mut count = 0u32;
    let entries: Vec<_> = std::fs::read_dir(json_dir)?
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|ext| ext == "json"))
        .collect();

    if entries.is_empty() {
        return Ok(0);
    }

    for entry in &entries {
        let path = entry.path();
        let data = std::fs::read_to_string(&path)?;
        if let Ok(mem) = serde_json::from_str::<Memory>(&data) {
            store.save(&mem)?;
            std::fs::remove_file(&path)?;
            count += 1;
        }
    }

    if count > 0 {
        eprintln!("[marrow] migrated {count} memories from JSON to SQLite");
    }

    // Remove the directory if it's now empty (ignore errors �� may contain other files)
    let _ = std::fs::remove_dir(json_dir);

    Ok(count)
}

pub fn migrate_knowledge_to_memories(
    knowledge_dir: &Path,
    store: &MemoryStore,
) -> Result<u32, Box<dyn Error + Send + Sync>> {
    if !knowledge_dir.exists() {
        return Ok(0);
    }

    let entries: Vec<_> = std::fs::read_dir(knowledge_dir)?
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|ext| ext == "md"))
        .collect();

    if entries.is_empty() {
        return Ok(0);
    }

    let mut count = 0u32;
    for entry in &entries {
        let path = entry.path();
        let content = std::fs::read_to_string(&path)?;
        if content.is_empty() {
            continue;
        }
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("document");
        let fact = format!("[{stem}] {content}");
        let memory = Memory::new(fact, MemorySource::Auto);
        store.save(&memory)?;
        std::fs::remove_file(&path)?;
        count += 1;
    }

    if count > 0 {
        eprintln!("[marrow] migrated {count} knowledge document(s) back to memories");
    }

    let _ = std::fs::remove_dir(knowledge_dir);

    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> tempfile::TempDir {
        tempfile::Builder::new().prefix(name).tempdir().unwrap()
    }

    #[test]
    fn save_load_roundtrip() {
        let dir = temp_dir("mem_test");
        let store = MemoryStore::new(dir.path()).unwrap();
        let mem = Memory::new("user likes Rust", MemorySource::User);
        let id = mem.id;
        store.save(&mem).unwrap();

        let loaded = store.load(id).unwrap();
        assert_eq!(loaded.id, id);
        assert_eq!(loaded.fact, "user likes Rust");
        assert!(matches!(loaded.source, MemorySource::User));
    }

    #[test]
    fn list_returns_all() {
        let dir = temp_dir("mem_test");
        let store = MemoryStore::new(dir.path()).unwrap();
        store
            .save(&Memory::new("fact one", MemorySource::Auto))
            .unwrap();
        store
            .save(&Memory::new("fact two", MemorySource::User))
            .unwrap();
        store
            .save(&Memory::new("fact three", MemorySource::Auto))
            .unwrap();
        assert_eq!(store.list().unwrap().len(), 3);
    }

    #[test]
    fn update_changes_fact() {
        let dir = temp_dir("mem_test");
        let store = MemoryStore::new(dir.path()).unwrap();
        let mem = Memory::new("old fact", MemorySource::Auto);
        let id = mem.id;
        store.save(&mem).unwrap();
        store.update(id, "new fact".to_string()).unwrap();
        assert_eq!(store.load(id).unwrap().fact, "new fact");
    }

    #[test]
    fn delete_removes() {
        let dir = temp_dir("mem_test");
        let store = MemoryStore::new(dir.path()).unwrap();
        let mem = Memory::new("ephemeral", MemorySource::Auto);
        let id = mem.id;
        store.save(&mem).unwrap();
        store.delete(id).unwrap();
        assert!(store.list().unwrap().is_empty());
    }

    #[test]
    fn delete_nonexistent_ok() {
        let dir = temp_dir("mem_test");
        let store = MemoryStore::new(dir.path()).unwrap();
        store.delete(Uuid::new_v4()).unwrap();
    }

    #[test]
    fn search_filters() {
        let dir = temp_dir("mem_test");
        let store = MemoryStore::new(dir.path()).unwrap();
        store
            .save(&Memory::new("user prefers dark mode", MemorySource::User))
            .unwrap();
        store
            .save(&Memory::new("user timezone is UTC", MemorySource::User))
            .unwrap();
        store
            .save(&Memory::new("deploy target is prod", MemorySource::Auto))
            .unwrap();

        let (results, total) = store.search("user", 0, 20).unwrap();
        assert_eq!(total, 2);
        assert_eq!(results.len(), 2);
    }

    #[test]
    fn search_pagination() {
        let dir = temp_dir("mem_test");
        let store = MemoryStore::new(dir.path()).unwrap();
        for i in 0..5 {
            store
                .save(&Memory::new(format!("fact {i}"), MemorySource::Auto))
                .unwrap();
        }

        let (results, total) = store.search("fact", 0, 2).unwrap();
        assert_eq!(total, 5);
        assert_eq!(results.len(), 2);

        let (results, _) = store.search("fact", 3, 2).unwrap();
        assert_eq!(results.len(), 2);
    }

    #[test]
    fn search_case_insensitive() {
        let dir = temp_dir("mem_test");
        let store = MemoryStore::new(dir.path()).unwrap();
        store
            .save(&Memory::new("User Prefers DARK Mode", MemorySource::User))
            .unwrap();

        let (results, _) = store.search("dark mode", 0, 20).unwrap();
        assert_eq!(results.len(), 1);
    }

    #[test]
    fn migrate_json_to_sqlite_works() {
        let dir = temp_dir("mem_migrate");
        let json_dir = dir.path().join("json_memories");
        std::fs::create_dir_all(&json_dir).unwrap();

        let mem = Memory::new("migrated fact", MemorySource::User);
        let json = serde_json::to_string_pretty(&mem).unwrap();
        std::fs::write(json_dir.join(format!("{}.json", mem.id)), json).unwrap();

        let store_dir = dir.path().join("store");
        let store = MemoryStore::new(&store_dir).unwrap();
        let count = migrate_json_to_sqlite(&json_dir, &store).unwrap();

        assert_eq!(count, 1);
        assert_eq!(store.list().unwrap().len(), 1);
        assert_eq!(store.list().unwrap()[0].fact, "migrated fact");
        assert!(!json_dir.join(format!("{}.json", mem.id)).exists());
    }

    #[test]
    fn migrate_json_empty_dir() {
        let dir = temp_dir("mem_migrate");
        let json_dir = dir.path().join("empty");
        std::fs::create_dir_all(&json_dir).unwrap();

        let store_dir = dir.path().join("store");
        let store = MemoryStore::new(&store_dir).unwrap();
        assert_eq!(migrate_json_to_sqlite(&json_dir, &store).unwrap(), 0);
    }

    #[test]
    fn migrate_knowledge_to_memories_works() {
        let dir = temp_dir("mem_knowledge");
        let knowledge_dir = dir.path().join("knowledge");
        std::fs::create_dir_all(&knowledge_dir).unwrap();
        std::fs::write(knowledge_dir.join("profile.md"), "# Profile\n- Name: Alice").unwrap();
        std::fs::write(
            knowledge_dir.join("infra.md"),
            "# Infrastructure\n- Server: prod",
        )
        .unwrap();

        let store_dir = dir.path().join("store");
        let store = MemoryStore::new(&store_dir).unwrap();
        let count = migrate_knowledge_to_memories(&knowledge_dir, &store).unwrap();

        assert_eq!(count, 2);
        let memories = store.list().unwrap();
        assert_eq!(memories.len(), 2);
        let facts: Vec<&str> = memories.iter().map(|m| m.fact.as_str()).collect();
        assert!(facts.iter().any(|f| f.starts_with("[profile]")));
        assert!(facts.iter().any(|f| f.contains("Alice")));
    }

    #[test]
    fn migrate_knowledge_empty_dir() {
        let dir = temp_dir("mem_knowledge");
        let store = MemoryStore::new(dir.path()).unwrap();
        let missing = dir.path().join("no_such_dir");
        assert_eq!(migrate_knowledge_to_memories(&missing, &store).unwrap(), 0);
    }

    #[test]
    fn concurrent_access() {
        let dir = temp_dir("mem_concurrent");
        let store1 = MemoryStore::new(dir.path()).unwrap();
        let store2 = MemoryStore::new(dir.path()).unwrap();

        let mem = Memory::new("shared fact", MemorySource::Auto);
        let id = mem.id;
        store1.save(&mem).unwrap();

        let loaded = store2.load(id).unwrap();
        assert_eq!(loaded.fact, "shared fact");
    }

    #[test]
    fn set_and_query_embedding() {
        let dir = temp_dir("mem_vec");
        let store = MemoryStore::new(dir.path()).unwrap();
        let mem = Memory::new("test fact", MemorySource::Auto);
        let id = mem.id;
        store.save(&mem).unwrap();

        let mut embedding = vec![0.0f32; DEFAULT_EMBEDDING_DIMENSIONS];
        embedding[0] = 1.0;
        store.set_embedding(id, &embedding).unwrap();

        let results = store.nearest(&embedding, 10).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0.id, id);
    }

    #[test]
    fn nearest_excludes_unembedded() {
        let dir = temp_dir("mem_vec");
        let store = MemoryStore::new(dir.path()).unwrap();

        let embedded = Memory::new("embedded fact", MemorySource::Auto);
        let embedded_id = embedded.id;
        store.save(&embedded).unwrap();
        let mut emb = vec![0.0f32; DEFAULT_EMBEDDING_DIMENSIONS];
        emb[0] = 1.0;
        store.set_embedding(embedded_id, &emb).unwrap();

        let unembedded = Memory::new("no vector", MemorySource::Auto);
        store.save(&unembedded).unwrap();

        let results = store.nearest(&emb, 10).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0.id, embedded_id);
    }

    #[test]
    fn unembedded_returns_only_null() {
        let dir = temp_dir("mem_vec");
        let store = MemoryStore::new(dir.path()).unwrap();

        let with_vec = Memory::new("has embedding", MemorySource::Auto);
        store.save(&with_vec).unwrap();
        let emb = vec![0.0f32; DEFAULT_EMBEDDING_DIMENSIONS];
        store.set_embedding(with_vec.id, &emb).unwrap();

        let without_vec = Memory::new("no embedding", MemorySource::Auto);
        let without_id = without_vec.id;
        store.save(&without_vec).unwrap();

        let missing = store.unembedded().unwrap();
        assert_eq!(missing.len(), 1);
        assert_eq!(missing[0].id, without_id);
    }

    #[test]
    fn nearest_ordering() {
        let dir = temp_dir("mem_vec");
        let store = MemoryStore::new(dir.path()).unwrap();

        let close = Memory::new("close fact", MemorySource::Auto);
        let mid = Memory::new("mid fact", MemorySource::Auto);
        let far = Memory::new("far fact", MemorySource::Auto);
        store.save(&close).unwrap();
        store.save(&mid).unwrap();
        store.save(&far).unwrap();

        // Vectors pointing in different directions
        let mut close_emb = vec![0.0f32; DEFAULT_EMBEDDING_DIMENSIONS];
        close_emb[0] = 1.0;
        close_emb[1] = 0.0;

        let mut mid_emb = vec![0.0f32; DEFAULT_EMBEDDING_DIMENSIONS];
        mid_emb[0] = 0.7;
        mid_emb[1] = 0.7;

        let mut far_emb = vec![0.0f32; DEFAULT_EMBEDDING_DIMENSIONS];
        far_emb[0] = 0.0;
        far_emb[1] = 1.0;

        store.set_embedding(close.id, &close_emb).unwrap();
        store.set_embedding(mid.id, &mid_emb).unwrap();
        store.set_embedding(far.id, &far_emb).unwrap();

        // Query with a vector similar to close_emb
        let mut query = vec![0.0f32; DEFAULT_EMBEDDING_DIMENSIONS];
        query[0] = 1.0;

        let results = store.nearest(&query, 3).unwrap();
        assert_eq!(results.len(), 3);
        assert_eq!(results[0].0.id, close.id);
        assert_eq!(results[2].0.id, far.id);
    }

    #[test]
    fn delete_removes_embedding() {
        let dir = temp_dir("mem_vec");
        let store = MemoryStore::new(dir.path()).unwrap();
        let mem = Memory::new("will be deleted", MemorySource::Auto);
        let id = mem.id;
        store.save(&mem).unwrap();
        let emb = vec![0.1f32; DEFAULT_EMBEDDING_DIMENSIONS];
        store.set_embedding(id, &emb).unwrap();

        store.delete(id).unwrap();

        assert!(store.list().unwrap().is_empty());
        assert!(store.nearest(&emb, 10).unwrap().is_empty());
    }
}
