use reqwest::Client;
use serde::{Deserialize, Serialize};

use crate::model::{CompletionResult, ModelBackend};

#[derive(Debug, Serialize)]
struct ChatRequest {
    model: String,
    messages: Vec<Message>,
    stream: bool,
}

#[derive(Debug, Serialize, Deserialize)]
struct Message {
    role: String,
    content: String,
}

#[derive(Debug, Deserialize)]
struct ChatResponse {
    message: Message,
}

pub struct OllamaBackend {
    client: Client,
    base_url: String,
    api_key: Option<String>,
    model: String,
}

impl OllamaBackend {
    pub fn new(base_url: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            client: Client::new(),
            base_url: base_url.into(),
            api_key: None,
            model: model.into(),
        }
    }

    pub fn with_api_key(mut self, key: impl Into<String>) -> Self {
        self.api_key = Some(key.into());
        self
    }

    pub fn from_env(base_url: impl Into<String>, model: impl Into<String>) -> Self {
        let mut backend = Self::new(base_url, model);
        if let Ok(key) = std::env::var("OLLAMA_API_KEY") {
            backend.api_key = Some(key);
        }
        backend
    }
}

impl ModelBackend for OllamaBackend {
    fn complete(&self, prompt: String) -> CompletionResult<'_> {
        Box::pin(async move {
            let url = format!("{}/api/chat", self.base_url.trim_end_matches('/'));

            let body = ChatRequest {
                model: self.model.clone(),
                messages: vec![Message {
                    role: "user".to_string(),
                    content: prompt,
                }],
                stream: false,
            };

            let mut req = self.client.post(&url).json(&body);

            if let Some(key) = &self.api_key {
                req = req.bearer_auth(key);
            }

            let resp = req.send().await?;

            if !resp.status().is_success() {
                let status = resp.status();
                let text = resp.text().await.unwrap_or_default();
                return Err(format!("ollama returned {status}: {text}").into());
            }

            let chat_resp: ChatResponse = resp.json().await?;
            Ok(chat_resp.message.content)
        })
    }
}
