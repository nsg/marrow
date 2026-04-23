use std::error::Error;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

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

pub struct MemoryStore {
    dir: PathBuf,
}

impl MemoryStore {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn ensure_dir(&self) -> Result<(), Box<dyn Error + Send + Sync>> {
        std::fs::create_dir_all(&self.dir)?;
        Ok(())
    }

    pub fn save(&self, memory: &Memory) -> Result<(), Box<dyn Error + Send + Sync>> {
        self.ensure_dir()?;
        let path = self.dir.join(format!("{}.json", memory.id));
        let json = serde_json::to_string_pretty(memory)?;
        std::fs::write(path, json)?;
        Ok(())
    }

    pub fn load(&self, id: Uuid) -> Result<Memory, Box<dyn Error + Send + Sync>> {
        let path = self.dir.join(format!("{id}.json"));
        let data = std::fs::read_to_string(path)?;
        Ok(serde_json::from_str(&data)?)
    }

    pub fn delete(&self, id: Uuid) -> Result<(), Box<dyn Error + Send + Sync>> {
        let path = self.dir.join(format!("{id}.json"));
        if path.exists() {
            std::fs::remove_file(path)?;
        }
        Ok(())
    }

    pub fn update(&self, id: Uuid, new_fact: String) -> Result<(), Box<dyn Error + Send + Sync>> {
        let mut memory = self.load(id)?;
        memory.fact = new_fact;
        self.save(&memory)
    }

    pub fn list(&self) -> Result<Vec<Memory>, Box<dyn Error + Send + Sync>> {
        let mut memories = Vec::new();
        if !self.dir.exists() {
            return Ok(memories);
        }

        for entry in std::fs::read_dir(&self.dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().is_some_and(|e| e == "json") {
                let data = std::fs::read_to_string(&path)?;
                if let Ok(mem) = serde_json::from_str::<Memory>(&data) {
                    memories.push(mem);
                }
            }
        }

        Ok(memories)
    }
}
