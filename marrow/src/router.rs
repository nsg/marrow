use std::collections::HashMap;
use std::error::Error;
use std::path::Path;
use std::sync::Arc;

use serde::Deserialize;

use crate::backends::ollama::{OllamaBackend, OllamaEmbedBackend};
use crate::backends::openai::{OpenAIBackend, OpenAIEmbedBackend};
use crate::metrics::Metrics;
use crate::model::{EmbedBackend, ModelBackend};
use crate::raw_log::RawLog;

#[derive(Debug, Deserialize, Default)]
pub struct DiscordConfig {
    #[serde(default)]
    pub token: Option<String>,
    #[serde(default)]
    pub channels: Vec<u64>,
    #[serde(default)]
    pub toolbox: Option<String>,
    #[serde(default)]
    pub memory: Option<String>,
    #[serde(default)]
    pub log: Option<String>,
    #[serde(default)]
    pub verbose: bool,
}

#[derive(Debug, Deserialize, Default)]
pub struct SchedulerConfig {
    #[serde(default)]
    pub schedules: Option<String>,
    #[serde(default = "default_scheduler_enabled")]
    pub enabled: bool,
    #[serde(default)]
    pub tick_seconds: Option<u64>,
}

fn default_scheduler_enabled() -> bool {
    true
}

impl SchedulerConfig {
    pub fn schedules_path(&self) -> &str {
        self.schedules.as_deref().unwrap_or("schedules")
    }

    pub fn tick(&self) -> u64 {
        self.tick_seconds.unwrap_or(60)
    }
}

#[derive(Debug, Deserialize, Default)]
pub struct DashConfig {
    #[serde(default)]
    pub bind: Option<String>,
    #[serde(default)]
    pub port: Option<u16>,
    #[serde(default)]
    pub debug_token: Option<String>,
}

impl DashConfig {
    pub fn bind_addr(&self) -> &str {
        self.bind.as_deref().unwrap_or("127.0.0.1")
    }

    pub fn port_number(&self) -> u16 {
        self.port.unwrap_or(3000)
    }
}

#[derive(Debug, Deserialize)]
pub struct RouterConfig {
    pub roles: HashMap<String, RoleConfig>,
    #[serde(default)]
    pub discord: Option<DiscordConfig>,
    #[serde(default)]
    pub scheduler: Option<SchedulerConfig>,
    #[serde(default)]
    pub dash: Option<DashConfig>,
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
        self.build_backend_with_metrics(role, None, None)
    }

    pub fn build_embed_backend(
        &self,
        role: &str,
    ) -> Result<Box<dyn EmbedBackend>, Box<dyn Error + Send + Sync>> {
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
                let mut eb = OllamaEmbedBackend::from_env(base_url, &role_config.model);
                if let Some(key) = &role_config.api_key {
                    eb = eb.with_api_key(key);
                }
                Ok(Box::new(eb))
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
                Ok(Box::new(OpenAIEmbedBackend::new(
                    base_url,
                    &role_config.model,
                    api_key,
                )))
            }
            other => Err(format!("unknown provider: {other}").into()),
        }
    }

    pub fn build_backend_with_metrics(
        &self,
        role: &str,
        metrics: Option<Arc<Metrics>>,
        raw_log: Option<Arc<RawLog>>,
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
                if let Some(rl) = raw_log {
                    backend = backend.with_raw_log(rl);
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
                if let Some(rl) = raw_log {
                    backend = backend.with_raw_log(rl);
                }
                Ok(Box::new(backend))
            }
            other => Err(format!("unknown provider: {other}").into()),
        }
    }
}

pub struct ModelRouter {
    backends: HashMap<String, Box<dyn ModelBackend>>,
    embed_backends: HashMap<String, Box<dyn EmbedBackend>>,
}

impl ModelRouter {
    pub fn from_config(config: &RouterConfig) -> Result<Self, Box<dyn Error + Send + Sync>> {
        Self::from_config_with_metrics(config, None, None)
    }

    pub fn from_config_with_metrics(
        config: &RouterConfig,
        metrics: Option<Arc<Metrics>>,
        raw_log: Option<Arc<RawLog>>,
    ) -> Result<Self, Box<dyn Error + Send + Sync>> {
        let mut backends: HashMap<String, Box<dyn ModelBackend>> = HashMap::new();
        let mut embed_backends: HashMap<String, Box<dyn EmbedBackend>> = HashMap::new();

        for (role, role_config) in &config.roles {
            let base_url_ollama = role_config
                .api_base
                .as_deref()
                .unwrap_or("http://localhost:11434");
            let base_url_openai = role_config
                .api_base
                .as_deref()
                .unwrap_or("https://api.openai.com/v1");

            let backend: Box<dyn ModelBackend> = match role_config.provider.as_str() {
                "ollama" => {
                    let mut backend = OllamaBackend::from_env(base_url_ollama, &role_config.model)
                        .with_role(role);
                    if let Some(key) = &role_config.api_key {
                        backend = backend.with_api_key(key);
                    }
                    if let Some(ref m) = metrics {
                        backend = backend.with_metrics(m.clone());
                    }
                    if let Some(ref rl) = raw_log {
                        backend = backend.with_raw_log(rl.clone());
                    }
                    Box::new(backend)
                }
                "openai" => {
                    let api_key = role_config.api_key.as_deref().ok_or_else(|| {
                        format!("openai provider for role '{role}' requires api_key")
                    })?;
                    let mut backend =
                        OpenAIBackend::new(base_url_openai, &role_config.model, api_key)
                            .with_role(role);
                    if let Some(ref m) = metrics {
                        backend = backend.with_metrics(m.clone());
                    }
                    if let Some(ref rl) = raw_log {
                        backend = backend.with_raw_log(rl.clone());
                    }
                    Box::new(backend)
                }
                other => return Err(format!("unknown provider: {other}").into()),
            };

            // Build embed backend for every role
            let embed: Box<dyn EmbedBackend> = match role_config.provider.as_str() {
                "ollama" => {
                    let mut eb = OllamaEmbedBackend::from_env(base_url_ollama, &role_config.model);
                    if let Some(key) = &role_config.api_key {
                        eb = eb.with_api_key(key);
                    }
                    Box::new(eb)
                }
                "openai" => {
                    let api_key = role_config.api_key.as_deref().ok_or_else(|| {
                        format!("openai provider for role '{role}' requires api_key")
                    })?;
                    Box::new(OpenAIEmbedBackend::new(
                        base_url_openai,
                        &role_config.model,
                        api_key,
                    ))
                }
                _ => continue, // skip unknown for embed — model backend already errored above
            };

            backends.insert(role.clone(), backend);
            embed_backends.insert(role.clone(), embed);
        }

        Ok(Self {
            backends,
            embed_backends,
        })
    }

    pub fn backend(&self, role: &str) -> Result<&dyn ModelBackend, Box<dyn Error + Send + Sync>> {
        let backend = self
            .backends
            .get(role)
            .ok_or_else(|| format!("no backend configured for role: {role}"))?;

        Ok(backend.as_ref())
    }

    pub fn embed_backend(
        &self,
        role: &str,
    ) -> Result<&dyn EmbedBackend, Box<dyn Error + Send + Sync>> {
        let backend = self
            .embed_backends
            .get(role)
            .ok_or_else(|| format!("no embed backend configured for role: {role}"))?;

        Ok(backend.as_ref())
    }
}
