use std::sync::Arc;
use std::time::Instant;

use reqwest::Client;
use serde::{Deserialize, Serialize};

use crate::metrics::Metrics;
use crate::model::{CompletionResult, ModelBackend};
use crate::session::Message;

#[derive(Debug, Serialize)]
struct ChatRequest {
    model: String,
    messages: Vec<Message>,
    stream: bool,
}

#[derive(Debug, Deserialize)]
struct ChatResponse {
    message: ChatMessage,
    #[serde(default)]
    prompt_eval_count: Option<u64>,
    #[serde(default)]
    eval_count: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct ChatMessage {
    content: String,
}

pub struct OllamaBackend {
    client: Client,
    base_url: String,
    api_key: Option<String>,
    model: String,
    role: String,
    metrics: Option<Arc<Metrics>>,
}

impl OllamaBackend {
    pub fn new(base_url: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            client: Client::new(),
            base_url: base_url.into(),
            api_key: None,
            model: model.into(),
            role: String::new(),
            metrics: None,
        }
    }

    pub fn with_api_key(mut self, key: impl Into<String>) -> Self {
        self.api_key = Some(key.into());
        self
    }

    pub fn with_role(mut self, role: impl Into<String>) -> Self {
        self.role = role.into();
        self
    }

    pub fn with_metrics(mut self, metrics: Arc<Metrics>) -> Self {
        self.metrics = Some(metrics);
        self
    }

    pub fn from_env(base_url: impl Into<String>, model: impl Into<String>) -> Self {
        let mut backend = Self::new(base_url, model);
        if let Ok(key) = std::env::var("OLLAMA_API_KEY") {
            backend.api_key = Some(key);
        }
        backend
    }

    async fn send_chat(
        &self,
        messages: Vec<Message>,
    ) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
        let url = format!("{}/api/chat", self.base_url.trim_end_matches('/'));

        let body = ChatRequest {
            model: self.model.clone(),
            messages,
            stream: false,
        };

        let mut req = self.client.post(&url).json(&body);

        if let Some(key) = &self.api_key {
            req = req.bearer_auth(key);
        }

        let start = Instant::now();
        let resp = req.send().await?;
        let duration = start.elapsed();

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(format!("ollama returned {status}: {text}").into());
        }

        let chat_resp: ChatResponse = resp.json().await?;

        if let Some(ref metrics) = self.metrics {
            metrics.record(
                &self.role,
                duration,
                chat_resp.prompt_eval_count.unwrap_or(0),
                chat_resp.eval_count.unwrap_or(0),
            );
        }

        Ok(chat_resp.message.content)
    }
}

impl ModelBackend for OllamaBackend {
    fn complete(&self, prompt: String) -> CompletionResult<'_> {
        Box::pin(async move {
            let messages = vec![Message::user(prompt)];
            self.send_chat(messages).await
        })
    }

    fn complete_chat(&self, messages: Vec<Message>) -> CompletionResult<'_> {
        Box::pin(async move { self.send_chat(messages).await })
    }
}
