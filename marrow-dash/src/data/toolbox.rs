use std::path::Path;

use serde::Serialize;

#[derive(Clone, Serialize)]
pub struct ToolInfo {
    pub name: String,
    pub description: String,
    pub provides: Vec<String>,
    pub validated: bool,
}

pub fn load(dir: &Path) -> Vec<ToolInfo> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };

    let mut tools = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_some_and(|e| e == "toml")
            && let Ok(content) = std::fs::read_to_string(&path)
            && let Ok(meta) = toml::from_str::<marrow::toolbox::ToolMeta>(&content)
        {
            tools.push(ToolInfo {
                name: meta.name,
                description: meta.description,
                provides: meta.provides,
                validated: meta.validated,
            });
        }
    }
    tools.sort_by(|a, b| a.name.cmp(&b.name));
    tools
}
