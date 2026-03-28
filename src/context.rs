use std::collections::HashMap;
use std::error::Error;
use std::path::PathBuf;
use std::sync::atomic::AtomicU32;
use std::sync::Arc;

use mlua::LuaSerdeExt;
use reqwest::Client;

use crate::sandbox::create_sandbox;
use crate::sandbox_host::{register_host_functions, HostConfig};

pub struct LuaProvider {
    pub name: String,
    source: String,
}

impl LuaProvider {
    pub fn new(name: impl Into<String>, source: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            source: source.into(),
        }
    }

    pub fn from_file(
        name: impl Into<String>,
        path: impl AsRef<std::path::Path>,
    ) -> Result<Self, Box<dyn Error + Send + Sync>> {
        let source = std::fs::read_to_string(path)?;
        Ok(Self::new(name, source))
    }

    pub async fn execute(
        &self,
        task_description: &str,
        client: Arc<Client>,
    ) -> Result<serde_json::Value, Box<dyn Error + Send + Sync>> {
        self.execute_with_params(task_description, client, &HashMap::new(), None)
            .await
    }

    pub async fn execute_with_params(
        &self,
        task_description: &str,
        client: Arc<Client>,
        params: &HashMap<String, String>,
        toolbox_dir: Option<PathBuf>,
    ) -> Result<serde_json::Value, Box<dyn Error + Send + Sync>> {
        let lua = create_sandbox()?;

        let config = HostConfig {
            client,
            toolbox_dir,
            task_description: task_description.to_string(),
            recursion_depth: Arc::new(AtomicU32::new(0)),
        };
        register_host_functions(&lua, &config)?;

        // TASK table
        let task_table = lua.create_table()?;
        task_table.set("description", task_description.to_string())?;
        lua.globals().set("TASK", task_table)?;

        // PARAMS table
        let params_table = lua.create_table()?;
        for (key, value) in params {
            params_table.set(key.as_str(), value.as_str())?;
        }
        lua.globals().set("PARAMS", params_table)?;

        let result: mlua::Value = lua.load(&self.source).eval_async().await?;
        let json: serde_json::Value = lua.from_value(result)?;

        Ok(json)
    }
}
