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

    pub fn list_unvalidated(&self) -> Result<Vec<ToolMeta>, Box<dyn Error + Send + Sync>> {
        Ok(self
            .list_tools()?
            .into_iter()
            .filter(|t| !t.validated)
            .collect())
    }

    pub fn load_all_providers(&self) -> Result<Vec<LuaProvider>, Box<dyn Error + Send + Sync>> {
        let mut providers = Vec::new();
        for meta in self.list_tools()? {
            match self.load_provider(&meta.name) {
                Ok(p) => providers.push(p),
                Err(e) => eprintln!("failed to load tool '{}': {e}", meta.name),
            }
        }
        Ok(providers)
    }
}
