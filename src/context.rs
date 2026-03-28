use std::collections::HashMap;
use std::error::Error;
use std::path::Path;
use std::sync::Arc;

use mlua::LuaSerdeExt;
use reqwest::Client;

use crate::executor::Context;
use crate::sandbox::create_sandbox;
use crate::sandbox_host::register_host_functions;

pub struct LuaProvider {
    name: String,
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
        path: impl AsRef<Path>,
    ) -> Result<Self, Box<dyn Error + Send + Sync>> {
        let source = std::fs::read_to_string(path)?;
        Ok(Self::new(name, source))
    }

    pub async fn execute(
        &self,
        task_description: &str,
        client: Arc<Client>,
    ) -> Result<serde_json::Value, Box<dyn Error + Send + Sync>> {
        self.execute_with_params(task_description, client, &HashMap::new(), &HashMap::new())
            .await
    }

    pub async fn execute_with_params(
        &self,
        task_description: &str,
        client: Arc<Client>,
        params: &HashMap<String, String>,
        results: &HashMap<String, String>,
    ) -> Result<serde_json::Value, Box<dyn Error + Send + Sync>> {
        let lua = create_sandbox()?;
        register_host_functions(&lua, client)?;

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

        // RESULTS table (outputs from prior stages)
        let results_table = lua.create_table()?;
        for (key, value) in results {
            results_table.set(key.as_str(), value.as_str())?;
        }
        lua.globals().set("RESULTS", results_table)?;

        let result: mlua::Value = lua.load(&self.source).eval_async().await?;

        let json: serde_json::Value = lua.from_value(result)?;
        Ok(json)
    }
}

/// A stage of tool execution: tools with per-tool params, run in parallel.
#[derive(Debug, Clone)]
pub struct Stage {
    /// tool_name -> per-tool params
    pub tools: HashMap<String, HashMap<String, String>>,
}

pub struct ContextAssembler {
    providers: HashMap<String, LuaProvider>,
    client: Arc<Client>,
}

impl ContextAssembler {
    pub fn new(client: Arc<Client>) -> Self {
        Self {
            providers: HashMap::new(),
            client,
        }
    }

    pub fn add_provider(&mut self, provider: LuaProvider) {
        self.providers.insert(provider.name.clone(), provider);
    }

    /// Execute stages sequentially. Within each stage, tools run in parallel.
    /// Outputs from earlier stages are passed as RESULTS to later stages.
    pub async fn assemble(
        &self,
        task_description: &str,
        stages: &[Stage],
    ) -> Result<Context, Box<dyn Error + Send + Sync>> {
        let mut data = serde_json::Map::new();
        let mut results: HashMap<String, String> = HashMap::new();

        for stage in stages {
            for (tool_name, tool_params) in &stage.tools {
                let Some(provider) = self.providers.get(tool_name) else {
                    eprintln!("provider '{tool_name}' not loaded, skipping");
                    continue;
                };

                match provider
                    .execute_with_params(
                        task_description,
                        self.client.clone(),
                        tool_params,
                        &results,
                    )
                    .await
                {
                    Ok(value) => {
                        // Store as JSON string for RESULTS in later stages
                        if let Ok(json_str) = serde_json::to_string(&value) {
                            results.insert(tool_name.clone(), json_str);
                        }
                        data.insert(tool_name.clone(), value);
                    }
                    Err(e) => {
                        eprintln!("provider '{tool_name}' failed: {e}");
                        let error_val = serde_json::json!({ "error": e.to_string() });
                        if let Ok(json_str) = serde_json::to_string(&error_val) {
                            results.insert(tool_name.clone(), json_str);
                        }
                        data.insert(tool_name.clone(), error_val);
                    }
                }
            }
        }

        Ok(Context::new(serde_json::Value::Object(data)))
    }
}
