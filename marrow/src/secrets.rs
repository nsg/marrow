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

    /// Resolve a parameter value: if the entire value starts with "secret:",
    /// look up the referenced secret and return its value. Otherwise, scan for
    /// any embedded `secret:NAME` references and replace them inline (e.g. a
    /// URL containing `secret:my_key` as a path segment).
    pub fn resolve_param(&self, value: &str) -> String {
        // Fast path: the entire value is a secret reference
        if let Some(name) = value.strip_prefix("secret:")
            && let Some(resolved) = self.get(name)
        {
            return resolved.to_string();
        }

        // Slow path: scan for embedded secret:NAME references.
        // A secret name is one or more word characters ([a-zA-Z0-9_]).
        if !value.contains("secret:") {
            return value.to_string();
        }

        let mut result = String::with_capacity(value.len());
        let mut rest = value;
        while let Some(pos) = rest.find("secret:") {
            result.push_str(&rest[..pos]);
            let after = &rest[pos + 7..]; // skip "secret:"
            // Collect the secret name: word chars until a non-word char or end
            let name_end = after
                .find(|c: char| !c.is_ascii_alphanumeric() && c != '_')
                .unwrap_or(after.len());
            let name = &after[..name_end];
            if let Some(resolved) = self.get(name) {
                result.push_str(resolved);
            } else {
                // Unknown secret — keep the original text
                result.push_str(&rest[pos..pos + 7 + name_end]);
            }
            rest = &after[name_end..];
        }
        result.push_str(rest);
        result
    }

    /// Resolve all `secret:` prefixed values in a params map.
    pub fn resolve_params(&self, params: &HashMap<String, String>) -> HashMap<String, String> {
        params
            .iter()
            .map(|(k, v)| (k.clone(), self.resolve_param(v)))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_secrets() -> Secrets {
        let mut m = HashMap::new();
        m.insert("api_key".to_string(), "sk-12345".to_string());
        m.insert("user".to_string(), "clawdy".to_string());
        m.insert("pass".to_string(), "hunter2".to_string());
        Secrets::from_map(m)
    }

    #[test]
    fn resolve_whole_value() {
        let s = test_secrets();
        assert_eq!(s.resolve_param("secret:api_key"), "sk-12345");
    }

    #[test]
    fn resolve_passthrough_no_prefix() {
        let s = test_secrets();
        assert_eq!(s.resolve_param("just a value"), "just a value");
    }

    #[test]
    fn resolve_unknown_secret_kept() {
        let s = test_secrets();
        assert_eq!(s.resolve_param("secret:nonexistent"), "secret:nonexistent");
    }

    #[test]
    fn resolve_embedded_in_url() {
        let s = test_secrets();
        let url = "https://example.com/calendars/secret:user/events";
        assert_eq!(
            s.resolve_param(url),
            "https://example.com/calendars/clawdy/events"
        );
    }

    #[test]
    fn resolve_multiple_embedded() {
        let s = test_secrets();
        let url = "https://secret:user:secret:pass@example.com/";
        assert_eq!(s.resolve_param(url), "https://clawdy:hunter2@example.com/");
    }

    #[test]
    fn resolve_embedded_at_end() {
        let s = test_secrets();
        assert_eq!(s.resolve_param("token=secret:api_key"), "token=sk-12345");
    }

    #[test]
    fn resolve_embedded_unknown_kept() {
        let s = test_secrets();
        let val = "https://example.com/secret:missing/path";
        assert_eq!(
            s.resolve_param(val),
            "https://example.com/secret:missing/path"
        );
    }

    #[test]
    fn resolve_no_secret_prefix_fast_path() {
        let s = test_secrets();
        let val = "https://example.com/normal/path";
        assert_eq!(s.resolve_param(val), "https://example.com/normal/path");
    }

    #[test]
    fn resolve_params_map() {
        let s = test_secrets();
        let mut params = HashMap::new();
        params.insert("URL".to_string(), "https://host/secret:user/".to_string());
        params.insert("TOKEN".to_string(), "secret:api_key".to_string());
        params.insert("PLAIN".to_string(), "hello".to_string());

        let resolved = s.resolve_params(&params);
        assert_eq!(resolved["URL"], "https://host/clawdy/");
        assert_eq!(resolved["TOKEN"], "sk-12345");
        assert_eq!(resolved["PLAIN"], "hello");
    }
}
