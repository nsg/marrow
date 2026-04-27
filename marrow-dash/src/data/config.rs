use std::path::Path;

use serde::Serialize;

#[derive(Serialize, Default)]
pub struct ConfigInfo {
    pub roles: Vec<RoleInfo>,
}

#[derive(Serialize)]
pub struct RoleInfo {
    pub name: String,
    pub provider: String,
    pub model: String,
}

impl ConfigInfo {
    pub fn load(path: &Path) -> Self {
        let Ok(config) = marrow::router::RouterConfig::from_file(path) else {
            return Self::default();
        };

        let mut roles: Vec<RoleInfo> = config
            .roles
            .into_iter()
            .map(|(name, rc)| RoleInfo {
                name,
                provider: rc.provider,
                model: rc.model,
            })
            .collect();
        roles.sort_by(|a, b| a.name.cmp(&b.name));

        Self { roles }
    }
}
