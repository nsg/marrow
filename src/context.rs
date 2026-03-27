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
        let lua = create_sandbox()?;
        register_host_functions(&lua, client)?;

        lua.globals()
            .set("TASK_DESCRIPTION", task_description.to_string())?;

        let result: mlua::Value = lua.load(&self.source).eval_async().await?;

        let json: serde_json::Value = lua.from_value(result)?;
        Ok(json)
    }
}

pub struct ContextAssembler {
    providers: Vec<LuaProvider>,
    client: Arc<Client>,
}

impl ContextAssembler {
    pub fn new(client: Arc<Client>) -> Self {
        Self {
            providers: Vec::new(),
            client,
        }
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
                .execute(task_description, self.client.clone())
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
