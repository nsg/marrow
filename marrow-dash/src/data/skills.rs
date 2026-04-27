use std::path::Path;

use serde::Serialize;

#[derive(Clone, Serialize)]
pub struct SkillInfo {
    pub name: String,
    pub content: String,
}

pub fn load(dir: &Path) -> Vec<SkillInfo> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };

    let mut skills: Vec<SkillInfo> = entries
        .flatten()
        .filter_map(|e| {
            let path = e.path();
            if path.extension().is_some_and(|ext| ext == "md") {
                let name = path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .map(String::from)?;
                let content = std::fs::read_to_string(&path).unwrap_or_default();
                Some(SkillInfo { name, content })
            } else {
                None
            }
        })
        .collect();
    skills.sort_by(|a, b| a.name.cmp(&b.name));
    skills
}
