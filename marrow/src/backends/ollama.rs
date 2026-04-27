use std::sync::Arc;
use std::time::{Duration, Instant};

use reqwest::Client;
use serde::{Deserialize, Serialize};

use crate::metrics::Metrics;
use crate::model::{CompletionResult, EmbedBackend, EmbedResult, ModelBackend};
use crate::retry::{RetryConfig, is_retryable_error, retry_with_backoff};
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
            client: Client::builder()
                .timeout(Duration::from_secs(300))
                .build()
                .expect("failed to build HTTP client"),
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

        let config = RetryConfig::default();

        let start = Instant::now();
        let resp_text = retry_with_backoff(&config, is_retryable_error, || {
            let mut req = self.client.post(&url).json(&body);
            if let Some(key) = &self.api_key {
                req = req.bearer_auth(key);
            }
            async move {
                let resp = req.send().await?;
                if !resp.status().is_success() {
                    let status = resp.status();
                    let text = resp.text().await.unwrap_or_default();
                    return Err(format!("ollama returned {status}: {text}").into());
                }
                resp.text().await.map_err(|e| e.into())
            }
        })
        .await?;
        let duration = start.elapsed();

        let chat_resp: ChatResponse = serde_json::from_str(&resp_text)?;

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

// -- Embedding backend --

#[derive(Debug, Serialize)]
struct EmbedRequest {
    model: String,
    input: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct EmbedResponse {
    embeddings: Vec<Vec<f32>>,
}

pub struct OllamaEmbedBackend {
    client: Client,
    base_url: String,
    api_key: Option<String>,
    model: String,
}

impl OllamaEmbedBackend {
    pub fn new(base_url: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            client: Client::builder()
                .timeout(Duration::from_secs(120))
                .build()
                .expect("failed to build HTTP client"),
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

    async fn send_embed(
        &self,
        texts: Vec<String>,
    ) -> Result<Vec<Vec<f32>>, Box<dyn std::error::Error + Send + Sync>> {
        let url = format!("{}/api/embed", self.base_url.trim_end_matches('/'));
        let body = EmbedRequest {
            model: self.model.clone(),
            input: texts,
        };
        let config = RetryConfig::default();
        let resp_text = retry_with_backoff(&config, is_retryable_error, || {
            let mut req = self.client.post(&url).json(&body);
            if let Some(key) = &self.api_key {
                req = req.bearer_auth(key);
            }
            async move {
                let resp = req.send().await?;
                if !resp.status().is_success() {
                    let status = resp.status();
                    let text = resp.text().await.unwrap_or_default();
                    return Err(format!("ollama embed returned {status}: {text}").into());
                }
                resp.text().await.map_err(|e| e.into())
            }
        })
        .await?;
        let embed_resp: EmbedResponse = serde_json::from_str(&resp_text)?;
        Ok(embed_resp.embeddings)
    }
}

impl EmbedBackend for OllamaEmbedBackend {
    fn embed(&self, texts: Vec<String>) -> EmbedResult<'_> {
        Box::pin(async move { self.send_embed(texts).await })
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
