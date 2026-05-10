use std::path::{Path, PathBuf};

use marrow::model::EmbedBackend;

pub struct AppState {
    pub log_path: PathBuf,
    pub toolbox_path: PathBuf,
    pub memory_path: PathBuf,
    pub schedules_path: PathBuf,
    pub skills_path: PathBuf,
    pub error_log_path: PathBuf,
    pub config: crate::data::config::ConfigInfo,
    pub embed_backend: Option<Box<dyn EmbedBackend>>,
}

impl AppState {
    pub fn new(
        log: &Path,
        toolbox: &Path,
        memory: &Path,
        schedules: &Path,
        skills: &Path,
        config: &Path,
        error_log: &Path,
    ) -> Self {
        let config_info = crate::data::config::ConfigInfo::load(config);

        let embed_backend = marrow::router::RouterConfig::from_file(config)
            .ok()
            .and_then(|rc| rc.build_embed_backend("embedding").ok());

        if embed_backend.is_some() {
            eprintln!("embedding backend available — search will use vector similarity");
        }

        Self {
            log_path: log.to_path_buf(),
            toolbox_path: toolbox.to_path_buf(),
            memory_path: memory.to_path_buf(),
            schedules_path: schedules.to_path_buf(),
            skills_path: skills.to_path_buf(),
            error_log_path: error_log.to_path_buf(),
            config: config_info,
            embed_backend,
        }
    }
}
