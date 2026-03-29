use std::error::Error;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::context::LuaProvider;

#[derive(Debug, Serialize, Deserialize)]
pub struct ToolMeta {
    pub name: String,
    pub description: String,
    pub provides: Vec<String>,
    #[serde(default)]
    pub validated: bool,
}

pub struct Toolbox {
    dir: PathBuf,
}

impl Toolbox {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    pub fn ensure_dir(&self) -> Result<(), Box<dyn Error + Send + Sync>> {
        std::fs::create_dir_all(&self.dir)?;
        Ok(())
    }

    pub fn save_tool(
        &self,
        meta: &ToolMeta,
        lua_source: &str,
    ) -> Result<(), Box<dyn Error + Send + Sync>> {
        self.ensure_dir()?;

        let meta_path = self.dir.join(format!("{}.toml", meta.name));
        let lua_path = self.dir.join(format!("{}.lua", meta.name));

        let meta_content = toml::to_string_pretty(meta)?;
        std::fs::write(meta_path, meta_content)?;
        std::fs::write(lua_path, lua_source)?;

        Ok(())
    }

    pub fn delete_tool(&self, name: &str) -> Result<(), Box<dyn Error + Send + Sync>> {
        let meta_path = self.dir.join(format!("{name}.toml"));
        let lua_path = self.dir.join(format!("{name}.lua"));
        if meta_path.exists() {
            std::fs::remove_file(meta_path)?;
        }
        if lua_path.exists() {
            std::fs::remove_file(lua_path)?;
        }
        Ok(())
    }

    pub fn load_provider(&self, name: &str) -> Result<LuaProvider, Box<dyn Error + Send + Sync>> {
        let lua_path = self.dir.join(format!("{name}.lua"));
        LuaProvider::from_file(name, lua_path)
    }

    pub fn load_meta(&self, name: &str) -> Result<ToolMeta, Box<dyn Error + Send + Sync>> {
        let meta_path = self.dir.join(format!("{name}.toml"));
        let content = std::fs::read_to_string(meta_path)?;
        let meta: ToolMeta = toml::from_str(&content)?;
        Ok(meta)
    }

    pub fn list_tools(&self) -> Result<Vec<ToolMeta>, Box<dyn Error + Send + Sync>> {
        let mut tools = Vec::new();
        if !self.dir.exists() {
            return Ok(tools);
        }

        for entry in std::fs::read_dir(&self.dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().is_some_and(|e| e == "toml") {
                let content = std::fs::read_to_string(&path)?;
                if let Ok(meta) = toml::from_str::<ToolMeta>(&content) {
                    tools.push(meta);
                }
            }
        }

        Ok(tools)
    }

    pub fn load_source(&self, name: &str) -> Result<String, Box<dyn Error + Send + Sync>> {
        let lua_path = self.dir.join(format!("{name}.lua"));
        Ok(std::fs::read_to_string(lua_path)?)
    }

    pub fn extract_params(&self, name: &str) -> Vec<String> {
        let source = match self.load_source(name) {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        let mut params = Vec::new();
        for cap in source.match_indices("PARAMS[\"") {
            let rest = &source[cap.0 + 8..];
            if let Some(end) = rest.find('"') {
                let key = &rest[..end];
                if !params.contains(&key.to_string()) {
                    params.push(key.to_string());
                }
            }
        }
        params
    }

    pub fn tool_usage(&self, meta: &ToolMeta) -> String {
        let params = self.extract_params(&meta.name);
        let returns = self.extract_return_fields(&meta.name);

        let mut line = format!("- {}: {}", meta.name, meta.description);
        if !params.is_empty() {
            line.push_str(&format!(" (params: {})", params.join(", ")));
        }
        if !returns.is_empty() {
            line.push_str(&format!(" (returns: {})", returns.join(", ")));
        }
        line
    }

    pub fn extract_return_fields(&self, name: &str) -> Vec<String> {
        let source = match self.load_source(name) {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };

        // Find the last "return {" block and extract top-level keys
        let mut fields = Vec::new();
        if let Some(ret_start) = source.rfind("return {") {
            let rest = &source[ret_start..];
            // Extract field names from "key =" patterns at the top level
            let mut depth = 0;
            for line in rest.lines() {
                for ch in line.chars() {
                    match ch {
                        '{' => depth += 1,
                        '}' => depth -= 1,
                        _ => {}
                    }
                }
                // Only extract from depth 1 (top-level of return table)
                if depth >= 1 {
                    let trimmed = line.trim();
                    if let Some(eq_pos) = trimmed.find('=') {
                        let key = trimmed[..eq_pos].trim().trim_matches(',');
                        if !key.is_empty()
                            && !key.contains(' ')
                            && !key.contains('{')
                            && !key.starts_with('-')
                            && key != "return"
                        {
                            let key = key.to_string();
                            if !fields.contains(&key) {
                                fields.push(key);
                            }
                        }
                    }
                }
                if depth <= 0 && !fields.is_empty() {
                    break;
                }
            }
        }
        fields
    }

    pub fn list_unvalidated(&self) -> Result<Vec<ToolMeta>, Box<dyn Error + Send + Sync>> {
        Ok(self
            .list_tools()?
            .into_iter()
            .filter(|t| !t.validated)
            .collect())
    }

    pub fn knowledge_path(&self) -> PathBuf {
        self.dir.join("CODEGEN_NOTES.md")
    }

    pub fn read_knowledge(&self) -> String {
        std::fs::read_to_string(self.knowledge_path()).unwrap_or_default()
    }

    pub fn append_knowledge(&self, note: &str) -> Result<(), Box<dyn Error + Send + Sync>> {
        self.ensure_dir()?;
        let path = self.knowledge_path();
        let mut content = std::fs::read_to_string(&path).unwrap_or_default();
        if !content.is_empty() && !content.ends_with('\n') {
            content.push('\n');
        }
        content.push_str("- ");
        content.push_str(note.trim());
        content.push('\n');
        std::fs::write(path, content)?;
        Ok(())
    }
}
