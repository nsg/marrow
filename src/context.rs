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

    pub fn name(&self) -> &str {
        &self.name
    }

    pub async fn execute(
        &self,
        task_description: &str,
        client: Arc<Client>,
    ) -> Result<serde_json::Value, Box<dyn Error + Send + Sync>> {
        self.execute_with_params(task_description, client, &HashMap::new())
            .await
    }

    pub async fn execute_with_params(
        &self,
        task_description: &str,
        client: Arc<Client>,
        params: &HashMap<String, String>,
    ) -> Result<serde_json::Value, Box<dyn Error + Send + Sync>> {
        let lua = create_sandbox()?;
        register_host_functions(&lua, client)?;

        lua.globals()
            .set("TASK_DESCRIPTION", task_description.to_string())?;

        for (key, value) in params {
            lua.globals().set(key.as_str(), value.as_str())?;
        }

        let result: mlua::Value = lua.load(&self.source).eval_async().await?;

        let json: serde_json::Value = lua.from_value(result)?;
        Ok(json)
    }
}

pub struct ContextAssembler {
    providers: Vec<LuaProvider>,
    client: Arc<Client>,
    params: HashMap<String, String>,
}

impl ContextAssembler {
    pub fn new(client: Arc<Client>) -> Self {
        Self {
            providers: Vec::new(),
            client,
            params: HashMap::new(),
        }
    }

    pub fn set_params(&mut self, params: HashMap<String, String>) {
        self.params = params;
    }

    pub fn add_provider(&mut self, provider: LuaProvider) {
        self.providers.push(provider);
    }

    pub async fn assemble(
        &self,
        task_description: &str,
        provider_names: &[String],
    ) -> Result<Context, Box<dyn Error + Send + Sync>> {
        let mut data = serde_json::Map::new();

        for provider in &self.providers {
            if !provider_names.is_empty() && !provider_names.contains(&provider.name) {
                continue;
            }

            match provider
                .execute_with_params(task_description, self.client.clone(), &self.params)
                .await
            {
                Ok(value) => {
                    data.insert(provider.name.clone(), value);
                }
                Err(e) => {
                    eprintln!("provider '{}' failed: {e}", provider.name);
                    data.insert(
                        provider.name.clone(),
                        serde_json::json!({ "error": e.to_string() }),
                    );
                }
            }
        }

        Ok(Context::new(serde_json::Value::Object(data)))
    }
}
