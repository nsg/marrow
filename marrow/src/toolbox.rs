use std::collections::HashMap;
use std::error::Error;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::context::LuaProvider;

type BoxError = Box<dyn Error + Send + Sync>;

static TOOLBOX_LOCKS: OnceLock<Mutex<HashMap<PathBuf, Arc<Mutex<()>>>>> = OnceLock::new();

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

    fn lock_handle(&self) -> Arc<Mutex<()>> {
        let key = if self.dir.is_absolute() {
            self.dir.clone()
        } else {
            std::env::current_dir()
                .unwrap_or_else(|_| PathBuf::from("."))
                .join(&self.dir)
        };

        let locks = TOOLBOX_LOCKS.get_or_init(|| Mutex::new(HashMap::new()));
        let mut guard = locks.lock().unwrap_or_else(|e| e.into_inner());
        guard
            .entry(key)
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }

    fn with_lock<T>(&self, f: impl FnOnce() -> Result<T, BoxError>) -> Result<T, BoxError> {
        let lock = self.lock_handle();
        let _guard = lock.lock().unwrap_or_else(|e| e.into_inner());
        f()
    }

    fn ensure_dir_inner(&self) -> Result<(), BoxError> {
        std::fs::create_dir_all(&self.dir)?;
        Ok(())
    }

    pub fn ensure_dir(&self) -> Result<(), BoxError> {
        self.with_lock(|| self.ensure_dir_inner())
    }

    fn meta_path(&self, name: &str) -> PathBuf {
        self.dir.join(format!("{name}.toml"))
    }

    fn lua_path(&self, name: &str) -> PathBuf {
        self.dir.join(format!("{name}.lua"))
    }

    fn validate_name(name: &str) -> Result<(), BoxError> {
        let mut chars = name.chars();
        match chars.next() {
            Some(c) if c.is_ascii_alphabetic() => {}
            Some(_) => {
                return Err(
                    "invalid tool name: must start with an ASCII letter and contain only ASCII letters, digits, and underscores"
                        .into(),
                );
            }
            None => return Err("invalid tool name: name cannot be empty".into()),
        }

        if chars.all(|c| c.is_ascii_alphanumeric() || c == '_') {
            Ok(())
        } else {
            Err(
                "invalid tool name: must start with an ASCII letter and contain only ASCII letters, digits, and underscores"
                    .into(),
            )
        }
    }

    fn write_atomic(path: &Path, contents: &str) -> Result<(), BoxError> {
        let file_name = path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or("tool path missing file name")?;
        let temp_path = path.with_file_name(format!(".{file_name}.tmp-{}", Uuid::new_v4()));

        std::fs::write(&temp_path, contents)?;
        std::fs::rename(&temp_path, path).inspect_err(|_| {
            let _ = std::fs::remove_file(&temp_path);
        })?;
        Ok(())
    }

    fn save_tool_inner(&self, meta: &ToolMeta, lua_source: &str) -> Result<(), BoxError> {
        self.ensure_dir_inner()?;

        let meta_path = self.meta_path(&meta.name);
        let lua_path = self.lua_path(&meta.name);

        let meta_content = toml::to_string_pretty(meta)?;
        Self::write_atomic(&lua_path, lua_source)?;
        Self::write_atomic(&meta_path, &meta_content)?;

        Ok(())
    }

    pub fn save_tool(&self, meta: &ToolMeta, lua_source: &str) -> Result<(), BoxError> {
        Self::validate_name(&meta.name)?;
        self.with_lock(|| self.save_tool_inner(meta, lua_source))
    }

    fn delete_tool_inner(&self, name: &str) -> Result<(), BoxError> {
        let meta_path = self.meta_path(name);
        let lua_path = self.lua_path(name);
        if meta_path.exists() {
            std::fs::remove_file(meta_path)?;
        }
        if lua_path.exists() {
            std::fs::remove_file(lua_path)?;
        }
        Ok(())
    }

    pub fn delete_tool(&self, name: &str) -> Result<(), BoxError> {
        Self::validate_name(name)?;
        self.with_lock(|| self.delete_tool_inner(name))
    }

    pub fn replace_tool(
        &self,
        old_name: Option<&str>,
        new_meta: &ToolMeta,
        lua_source: &str,
    ) -> Result<(), BoxError> {
        Self::validate_name(&new_meta.name)?;
        if let Some(old_name) = old_name {
            Self::validate_name(old_name)?;
        }
        self.with_lock(|| {
            self.save_tool_inner(new_meta, lua_source)?;
            if let Some(old_name) = old_name.filter(|name| *name != new_meta.name) {
                self.delete_tool_inner(old_name)?;
            }
            Ok(())
        })
    }

    pub fn load_provider(&self, name: &str) -> Result<LuaProvider, BoxError> {
        Self::validate_name(name)?;
        self.with_lock(|| {
            let source = std::fs::read_to_string(self.lua_path(name))?;
            Ok(LuaProvider::new(name, source))
        })
    }

    pub fn load_meta(&self, name: &str) -> Result<ToolMeta, BoxError> {
        Self::validate_name(name)?;
        self.with_lock(|| {
            let content = std::fs::read_to_string(self.meta_path(name))?;
            let meta: ToolMeta = toml::from_str(&content)?;
            Self::validate_name(&meta.name)?;
            if meta.name != name {
                return Err(format!(
                    "tool metadata name mismatch: requested '{name}', found '{}'",
                    meta.name
                )
                .into());
            }
            Ok(meta)
        })
    }

    pub fn list_tools(&self) -> Result<Vec<ToolMeta>, BoxError> {
        self.with_lock(|| {
            let mut tools = Vec::new();
            if !self.dir.exists() {
                return Ok(tools);
            }

            for entry in std::fs::read_dir(&self.dir)? {
                let entry = entry?;
                let path = entry.path();
                if path.extension().is_some_and(|e| e == "toml") {
                    let content = std::fs::read_to_string(&path)?;
                    if let Ok(meta) = toml::from_str::<ToolMeta>(&content)
                        && Self::validate_name(&meta.name).is_ok()
                    {
                        tools.push(meta);
                    }
                }
            }

            Ok(tools)
        })
    }

    pub fn load_source(&self, name: &str) -> Result<String, BoxError> {
        Self::validate_name(name)?;
        self.with_lock(|| Ok(std::fs::read_to_string(self.lua_path(name))?))
    }

    pub fn extract_params(&self, name: &str) -> Vec<String> {
        if Self::validate_name(name).is_err() {
            return Vec::new();
        }
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
        if Self::validate_name(name).is_err() {
            return Vec::new();
        }
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
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> tempfile::TempDir {
        tempfile::Builder::new().prefix(name).tempdir().unwrap()
    }

    #[test]
    fn separate_instances_see_saved_tool_consistently() {
        let dir = temp_dir("marrow_toolbox");
        let writer = Toolbox::new(dir.path());
        let reader = Toolbox::new(dir.path());

        let meta = ToolMeta {
            name: "weather".to_string(),
            description: "Weather lookup".to_string(),
            provides: vec!["weather".to_string()],
            validated: false,
        };

        writer
            .save_tool(&meta, r#"return { city = PARAMS["CITY"] }"#)
            .unwrap();

        assert_eq!(
            reader.load_meta("weather").unwrap().description,
            "Weather lookup"
        );
        assert!(reader.load_source("weather").unwrap().contains("PARAMS"));
        assert_eq!(reader.list_tools().unwrap().len(), 1);
    }

    #[test]
    fn replace_tool_removes_old_name() {
        let dir = temp_dir("marrow_toolbox");
        let toolbox = Toolbox::new(dir.path());

        let old_meta = ToolMeta {
            name: "old_tool".to_string(),
            description: "Old tool".to_string(),
            provides: vec!["old_tool".to_string()],
            validated: false,
        };
        toolbox
            .save_tool(&old_meta, "return { ok = true }")
            .unwrap();

        let new_meta = ToolMeta {
            name: "new_tool".to_string(),
            description: "New tool".to_string(),
            provides: vec!["new_tool".to_string()],
            validated: false,
        };
        toolbox
            .replace_tool(Some("old_tool"), &new_meta, "return { ok = true }")
            .unwrap();

        assert!(toolbox.load_meta("old_tool").is_err());
        assert!(toolbox.load_source("old_tool").is_err());
        assert_eq!(
            toolbox.load_meta("new_tool").unwrap().description,
            "New tool"
        );
    }

    #[test]
    fn rejects_path_traversal_tool_names() {
        let dir = temp_dir("marrow_toolbox");
        let toolbox = Toolbox::new(dir.path());
        let meta = ToolMeta {
            name: "../outside".to_string(),
            description: "Bad tool".to_string(),
            provides: vec![],
            validated: false,
        };

        assert!(toolbox.save_tool(&meta, "return {}").is_err());
        assert!(toolbox.delete_tool("../outside").is_err());
        assert!(toolbox.load_source("../outside").is_err());
        assert!(!dir.path().join("../outside.lua").exists());
    }

    #[test]
    fn rejects_separator_empty_and_hidden_tool_names() {
        let dir = temp_dir("marrow_toolbox");
        let toolbox = Toolbox::new(dir.path());

        for name in ["", ".hidden", "nested/tool", "1tool", "tool-name"] {
            let meta = ToolMeta {
                name: name.to_string(),
                description: "Bad tool".to_string(),
                provides: vec![],
                validated: false,
            };
            assert!(toolbox.save_tool(&meta, "return {}").is_err(), "{name}");
        }
    }

    #[test]
    fn allows_ascii_identifier_tool_names() {
        let dir = temp_dir("marrow_toolbox");
        let toolbox = Toolbox::new(dir.path());
        let meta = ToolMeta {
            name: "Weather_tool_2".to_string(),
            description: "Valid tool".to_string(),
            provides: vec!["Weather_tool_2".to_string()],
            validated: false,
        };

        toolbox.save_tool(&meta, "return {}").unwrap();

        assert!(toolbox.load_source("Weather_tool_2").is_ok());
    }
}
