use std::collections::HashMap;
use std::error::Error;
use std::path::Path;

use serde::Deserialize;

#[derive(Debug, Deserialize, Default)]
struct SecretsFile {
    #[serde(flatten)]
    values: HashMap<String, String>,
}

#[derive(Debug, Default)]
pub struct Secrets {
    values: HashMap<String, String>,
}

impl Secrets {
    pub fn from_file(path: impl AsRef<Path>) -> Result<Self, Box<dyn Error + Send + Sync>> {
        let content = std::fs::read_to_string(path)?;
        let file: SecretsFile = toml::from_str(&content)?;
        Ok(Self {
            values: file.values,
        })
    }

    pub fn load_or_empty(path: impl AsRef<Path>) -> Self {
        Self::from_file(path).unwrap_or_default()
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.values.get(key).map(String::as_str)
    }

    /// Returns secret key names (not values) for model prompts.
    pub fn keys(&self) -> Vec<&str> {
        self.values.keys().map(String::as_str).collect()
    }

    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }
}
