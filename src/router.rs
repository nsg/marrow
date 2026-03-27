use std::collections::HashMap;
use std::error::Error;
use std::path::Path;

use serde::Deserialize;

use crate::backends::ollama::OllamaBackend;
use crate::executor::{Context, Executor};
use crate::model::ModelBackend;
use crate::task::Task;

#[derive(Debug, Deserialize)]
pub struct RouterConfig {
    pub roles: HashMap<String, RoleConfig>,
}

#[derive(Debug, Deserialize)]
pub struct RoleConfig {
    pub provider: String,
    pub model: String,
    #[serde(default)]
    pub api_base: Option<String>,
    #[serde(default)]
    pub api_key: Option<String>,
}

impl RouterConfig {
    pub fn from_file(path: impl AsRef<Path>) -> Result<Self, Box<dyn Error + Send + Sync>> {
        let content = std::fs::read_to_string(path)?;
        let config: Self = toml::from_str(&content)?;
        Ok(config)
    }

    pub fn build_backend(
        &self,
        role: &str,
    ) -> Result<Box<dyn ModelBackend>, Box<dyn Error + Send + Sync>> {
        let role_config = self
            .roles
            .get(role)
            .ok_or_else(|| format!("no config for role: {role}"))?;

        match role_config.provider.as_str() {
            "ollama" => {
                let base_url = role_config
                    .api_base
                    .as_deref()
                    .unwrap_or("http://localhost:11434");
                let mut backend = OllamaBackend::from_env(base_url, &role_config.model);
                if let Some(key) = &role_config.api_key {
                    backend = backend.with_api_key(key);
                }
                Ok(Box::new(backend))
            }
            other => Err(format!("unknown provider: {other}").into()),
        }
    }
}

pub struct ModelRouter {
    backends: HashMap<String, Box<dyn ModelBackend>>,
}

impl ModelRouter {
    pub fn from_config(config: &RouterConfig) -> Result<Self, Box<dyn Error + Send + Sync>> {
        let mut backends: HashMap<String, Box<dyn ModelBackend>> = HashMap::new();

        for (role, role_config) in &config.roles {
            let backend: Box<dyn ModelBackend> = match role_config.provider.as_str() {
                "ollama" => {
                    let base_url = role_config
                        .api_base
                        .as_deref()
                        .unwrap_or("http://localhost:11434");

                    let mut backend = OllamaBackend::from_env(base_url, &role_config.model);

                    if let Some(key) = &role_config.api_key {
                        backend = backend.with_api_key(key);
                    }

                    Box::new(backend)
                }
                other => return Err(format!("unknown provider: {other}").into()),
            };

            backends.insert(role.clone(), backend);
        }

        Ok(Self { backends })
    }

    pub fn backend(&self, role: &str) -> Result<&dyn ModelBackend, Box<dyn Error + Send + Sync>> {
        let backend = self
            .backends
            .get(role)
            .ok_or_else(|| format!("no backend configured for role: {role}"))?;

        Ok(backend.as_ref())
    }
}

impl Executor for ModelRouter {
    async fn execute(
        &self,
        task: &Task,
        context: &Context,
    ) -> Result<serde_json::Value, Box<dyn Error + Send + Sync>> {
        let backend = self.backend(&task.model_role)?;

        let prompt = format!("Task: {}\nContext: {}", task.description, context.data);

        let response = backend.complete(prompt).await?;
        Ok(serde_json::Value::String(response))
    }
}
