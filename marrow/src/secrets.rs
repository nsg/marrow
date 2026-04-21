use std::collections::HashMap;
use std::error::Error;
use std::path::Path;

use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct SecretEntry {
    value: String,
    #[serde(default)]
    description: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
struct SecretsFile {
    #[serde(flatten)]
    entries: HashMap<String, SecretEntry>,
}

#[derive(Debug, Clone)]
struct SecretValue {
    value: String,
    description: Option<String>,
}

#[derive(Debug, Default, Clone)]
pub struct Secrets {
    entries: HashMap<String, SecretValue>,
}

impl Secrets {
    /// Build from a plain key-value map (no descriptions). Used by sandbox_host
    /// when Lua tools call built-in tools via run_tool().
    pub fn from_map(values: HashMap<String, String>) -> Self {
        let entries = values
            .into_iter()
            .map(|(k, v)| {
                (
                    k,
                    SecretValue {
                        value: v,
                        description: None,
                    },
                )
            })
            .collect();
        Self { entries }
    }

    pub fn from_file(path: impl AsRef<Path>) -> Result<Self, Box<dyn Error + Send + Sync>> {
        let content = std::fs::read_to_string(path)?;
        let file: SecretsFile = toml::from_str(&content)?;
        let entries = file
            .entries
            .into_iter()
            .map(|(k, e)| {
                (
                    k,
                    SecretValue {
                        value: e.value,
                        description: e.description,
                    },
                )
            })
            .collect();
        Ok(Self { entries })
    }

    pub fn load_or_empty(path: impl AsRef<Path>) -> Self {
        Self::from_file(path).unwrap_or_default()
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.entries.get(key).map(|e| e.value.as_str())
    }

    /// Returns secret key names (not values) for model prompts.
    pub fn keys(&self) -> Vec<&str> {
        self.entries.keys().map(String::as_str).collect()
    }

    /// Returns (name, description) pairs for the agent prompt.
    /// Secrets without a description are included with an empty string.
    pub fn descriptions(&self) -> Vec<(&str, &str)> {
        self.entries
            .iter()
            .map(|(k, v)| (k.as_str(), v.description.as_deref().unwrap_or("")))
            .collect()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Resolve a parameter value: if it starts with "secret:", look up the
    /// referenced secret and return its value. Otherwise return the original.
    pub fn resolve_param<'a>(&'a self, value: &'a str) -> &'a str {
        value
            .strip_prefix("secret:")
            .and_then(|name| self.get(name))
            .unwrap_or(value)
    }

    /// Resolve all `secret:` prefixed values in a params map.
    pub fn resolve_params(&self, params: &HashMap<String, String>) -> HashMap<String, String> {
        params
            .iter()
            .map(|(k, v)| (k.clone(), self.resolve_param(v).to_string()))
            .collect()
    }
}
