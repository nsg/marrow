use std::path::{Path, PathBuf};
use std::sync::RwLock;

use marrow::model::EmbedBackend;

use crate::data;

pub struct AppState {
    pub events: RwLock<data::events::EventData>,
    pub memory: RwLock<data::memory::MemoryStats>,
    pub toolbox: RwLock<Vec<data::toolbox::ToolInfo>>,
    pub schedules: RwLock<Vec<data::schedules::ScheduleInfo>>,
    pub skills: RwLock<Vec<data::skills::SkillInfo>>,
    pub config: data::config::ConfigInfo,
    pub memory_dir: PathBuf,
    pub embed_backend: Option<Box<dyn EmbedBackend>>,
}

impl AppState {
    pub fn load(
        log: &Path,
        toolbox: &Path,
        memory: &Path,
        schedules: &Path,
        skills: &Path,
        config: &Path,
    ) -> Self {
        let events = data::events::EventData::load(log);
        let memory_stats = data::memory::MemoryStats::load(memory);
        let toolbox_items = data::toolbox::load(toolbox);
        let schedule_items = data::schedules::load(schedules);
        let skill_items = data::skills::load(skills);
        let config_info = data::config::ConfigInfo::load(config);

        let embed_backend = marrow::router::RouterConfig::from_file(config)
            .ok()
            .and_then(|rc| rc.build_embed_backend("embedding").ok());

        if embed_backend.is_some() {
            eprintln!("embedding backend available — search will use vector similarity");
        }

        Self {
            events: RwLock::new(events),
            memory: RwLock::new(memory_stats),
            toolbox: RwLock::new(toolbox_items),
            schedules: RwLock::new(schedule_items),
            skills: RwLock::new(skill_items),
            config: config_info,
            memory_dir: memory.to_path_buf(),
            embed_backend,
        }
    }

    pub fn refresh(
        &self,
        log: &Path,
        toolbox: &Path,
        memory: &Path,
        schedules: &Path,
        skills: &Path,
    ) {
        {
            let mut ev = self.events.write().unwrap_or_else(|e| e.into_inner());
            ev.refresh(log);
        }
        {
            let mut mem = self.memory.write().unwrap_or_else(|e| e.into_inner());
            *mem = data::memory::MemoryStats::load(memory);
        }
        {
            let mut tb = self.toolbox.write().unwrap_or_else(|e| e.into_inner());
            *tb = data::toolbox::load(toolbox);
        }
        {
            let mut sc = self.schedules.write().unwrap_or_else(|e| e.into_inner());
            *sc = data::schedules::load(schedules);
        }
        {
            let mut sk = self.skills.write().unwrap_or_else(|e| e.into_inner());
            *sk = data::skills::load(skills);
        }
    }
}
