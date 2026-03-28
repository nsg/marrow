use std::collections::HashMap;
use std::error::Error;
use std::path::Path;
use std::sync::Arc;

use serde::Deserialize;

use crate::backends::ollama::OllamaBackend;
use crate::backends::openai::OpenAIBackend;
use crate::executor::{Context, Executor};
use crate::metrics::Metrics;
use crate::model::ModelBackend;
use crate::session::Message;
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
        self.build_backend_with_metrics(role, None)
    }

    pub fn build_backend_with_metrics(
        &self,
        role: &str,
        metrics: Option<Arc<Metrics>>,
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
                let mut backend =
                    OllamaBackend::from_env(base_url, &role_config.model).with_role(role);
                if let Some(key) = &role_config.api_key {
                    backend = backend.with_api_key(key);
                }
                if let Some(m) = metrics {
                    backend = backend.with_metrics(m);
                }
                Ok(Box::new(backend))
            }
            "openai" => {
                let base_url = role_config
                    .api_base
                    .as_deref()
                    .unwrap_or("https://api.openai.com/v1");
                let api_key = role_config
                    .api_key
                    .as_deref()
                    .ok_or_else(|| format!("openai provider for role '{role}' requires api_key"))?;
                let mut backend =
                    OpenAIBackend::new(base_url, &role_config.model, api_key).with_role(role);
                if let Some(m) = metrics {
                    backend = backend.with_metrics(m);
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
        Self::from_config_with_metrics(config, None)
    }

    pub fn from_config_with_metrics(
        config: &RouterConfig,
        metrics: Option<Arc<Metrics>>,
    ) -> Result<Self, Box<dyn Error + Send + Sync>> {
        let mut backends: HashMap<String, Box<dyn ModelBackend>> = HashMap::new();

        for (role, role_config) in &config.roles {
            let backend: Box<dyn ModelBackend> = match role_config.provider.as_str() {
                "ollama" => {
                    let base_url = role_config
                        .api_base
                        .as_deref()
                        .unwrap_or("http://localhost:11434");

                    let mut backend =
                        OllamaBackend::from_env(base_url, &role_config.model).with_role(role);

                    if let Some(key) = &role_config.api_key {
                        backend = backend.with_api_key(key);
                    }
                    if let Some(ref m) = metrics {
                        backend = backend.with_metrics(m.clone());
                    }

                    Box::new(backend)
                }
                "openai" => {
                    let base_url = role_config
                        .api_base
                        .as_deref()
                        .unwrap_or("https://api.openai.com/v1");
                    let api_key = role_config.api_key.as_deref().ok_or_else(|| {
                        format!("openai provider for role '{role}' requires api_key")
                    })?;
                    let mut backend =
                        OpenAIBackend::new(base_url, &role_config.model, api_key).with_role(role);
                    if let Some(ref m) = metrics {
                        backend = backend.with_metrics(m.clone());
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
        history: Option<&[Message]>,
    ) -> Result<serde_json::Value, Box<dyn Error + Send + Sync>> {
        let backend = self.backend(&task.model_role)?;

        let system_context = format!(
            "You are a helpful assistant. Tools have already been executed and their results are provided below. Use this data to answer the user's question directly. Do NOT suggest running more tools — all available data is here.\n\nTool results:\n{}",
            context.data
        );

        let response = if let Some(msgs) = history {
            // Build full message list: system context + history + current user message
            let mut messages = vec![Message::system(system_context)];
            messages.extend(msgs.iter().cloned());
            messages.push(Message::user(&task.description));
            backend.complete_chat(messages).await?
        } else {
            let prompt = format!("{system_context}\n\nTask: {}", task.description);
            backend.complete(prompt).await?
        };

        Ok(serde_json::Value::String(response))
    }
}
