use std::sync::Arc;
use std::time::{Duration, Instant};

use reqwest::Client;
use serde::{Deserialize, Serialize};

use crate::metrics::Metrics;
use crate::model::{CompletionResult, EmbedBackend, EmbedResult, ModelBackend, PROMPT_CACHE_KEY};
use crate::retry::{BackendError, RetryConfig, parse_retry_after, retry_with_backoff};
use crate::session::Message;

#[derive(Debug, Serialize)]
struct ChatRequest {
    model: String,
    messages: Vec<ApiMessage>,
    /// OpenAI routing hint — requests with the same key and prefix are more
    /// likely to land on the same cache server, improving cache hit rates.
    #[serde(skip_serializing_if = "Option::is_none")]
    prompt_cache_key: Option<String>,
    /// Extended cache retention — keeps KV tensors on GPU-local storage for up
    /// to 24 hours instead of the default 5-10 minute in-memory window.
    /// Only sent for models known to support it (GPT-4.1+, GPT-5+).
    #[serde(skip_serializing_if = "Option::is_none")]
    prompt_cache_retention: Option<String>,
}

/// Check whether a model name is known to support `prompt_cache_retention: "24h"`.
/// Conservative: only matches models explicitly documented as compatible.
/// Returns false for unknown models, OpenRouter-prefixed names, etc.
fn supports_extended_cache(model: &str) -> bool {
    let m = model.to_ascii_lowercase();
    m.starts_with("gpt-4.1") || m.starts_with("gpt-5")
}

#[derive(Debug, Serialize)]
struct ApiMessage {
    role: String,
    content: String,
}

impl From<&Message> for ApiMessage {
    fn from(m: &Message) -> Self {
        Self {
            role: m.role.clone(),
            content: m.content.clone(),
        }
    }
}

#[derive(Debug, Deserialize)]
struct ChatResponse {
    choices: Vec<Choice>,
    #[serde(default)]
    usage: Option<Usage>,
}

#[derive(Debug, Deserialize)]
struct Choice {
    message: ResponseMessage,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ResponseMessage {
    content: String,
}

#[derive(Debug, Deserialize)]
struct PromptTokensDetails {
    #[serde(default)]
    cached_tokens: u64,
}

#[derive(Debug, Deserialize)]
struct Usage {
    #[serde(default)]
    prompt_tokens: u64,
    #[serde(default)]
    completion_tokens: u64,
    #[serde(default)]
    prompt_tokens_details: Option<PromptTokensDetails>,
}

pub struct OpenAIBackend {
    client: Client,
    base_url: String,
    api_key: String,
    model: String,
    role: String,
    metrics: Option<Arc<Metrics>>,
}

impl OpenAIBackend {
    pub fn new(
        base_url: impl Into<String>,
        model: impl Into<String>,
        api_key: impl Into<String>,
    ) -> Self {
        Self {
            client: Client::builder()
                .timeout(Duration::from_secs(300))
                .build()
                .expect("failed to build HTTP client"),
            base_url: base_url.into(),
            api_key: api_key.into(),
            model: model.into(),
            role: String::new(),
            metrics: None,
        }
    }

    pub fn with_role(mut self, role: impl Into<String>) -> Self {
        self.role = role.into();
        self
    }

    pub fn with_metrics(mut self, metrics: Arc<Metrics>) -> Self {
        self.metrics = Some(metrics);
        self
    }

    async fn send_chat(
        &self,
        messages: Vec<Message>,
    ) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
        let url = format!("{}/chat/completions", self.base_url.trim_end_matches('/'));

        let api_messages: Vec<ApiMessage> = messages.iter().map(ApiMessage::from).collect();

        let cache_key = PROMPT_CACHE_KEY.try_with(|k| (**k).clone()).ok();

        let cache_retention = if supports_extended_cache(&self.model) {
            Some("24h".to_string())
        } else {
            None
        };

        let body = ChatRequest {
            model: self.model.clone(),
            messages: api_messages,
            prompt_cache_key: cache_key,
            prompt_cache_retention: cache_retention,
        };

        let config = RetryConfig::default();

        let start = Instant::now();
        let resp_text = retry_with_backoff(&config, BackendError::should_retry, || {
            let req = self
                .client
                .post(&url)
                .bearer_auth(&self.api_key)
                .json(&body);
            async move {
                let resp = req
                    .send()
                    .await
                    .map_err(|e| BackendError::Network(e.into()))?;
                if !resp.status().is_success() {
                    let status = resp.status().as_u16();
                    let retry_after = resp
                        .headers()
                        .get(reqwest::header::RETRY_AFTER)
                        .and_then(|v| v.to_str().ok())
                        .and_then(parse_retry_after);
                    let body = resp.text().await.unwrap_or_default();
                    return Err(BackendError::Http {
                        status,
                        body,
                        retry_after,
                    });
                }
                resp.text()
                    .await
                    .map_err(|e| BackendError::Network(e.into()))
            }
        })
        .await?;
        let duration = start.elapsed();

        let chat_resp: ChatResponse = serde_json::from_str(&resp_text)?;

        if let Some(ref metrics) = self.metrics {
            let (prompt_tokens, completion_tokens, cached_tokens) = chat_resp
                .usage
                .as_ref()
                .map(|u| {
                    let cached = u
                        .prompt_tokens_details
                        .as_ref()
                        .map(|d| d.cached_tokens)
                        .unwrap_or(0);
                    (u.prompt_tokens, u.completion_tokens, cached)
                })
                .unwrap_or((0, 0, 0));
            metrics.record(
                &self.role,
                duration,
                prompt_tokens,
                completion_tokens,
                cached_tokens,
            );
        }

        let choice = chat_resp
            .choices
            .into_iter()
            .next()
            .ok_or("no choices in response")?;

        let mut content = choice.message.content;

        // If the API truncated the response due to token limits, append a
        // marker so the agent loop's incomplete-answer detection can catch it.
        if choice.finish_reason.as_deref() == Some("length") {
            content.push_str("\n\n[response truncated by token limit]");
        }

        Ok(content)
    }
}

impl ModelBackend for OpenAIBackend {
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

// -- Embedding backend --

#[derive(Debug, Serialize)]
struct EmbedApiRequest {
    model: String,
    input: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct EmbedApiResponse {
    data: Vec<EmbedApiData>,
}

#[derive(Debug, Deserialize)]
struct EmbedApiData {
    embedding: Vec<f32>,
}

pub struct OpenAIEmbedBackend {
    client: Client,
    base_url: String,
    api_key: String,
    model: String,
}

impl OpenAIEmbedBackend {
    pub fn new(
        base_url: impl Into<String>,
        model: impl Into<String>,
        api_key: impl Into<String>,
    ) -> Self {
        Self {
            client: Client::builder()
                .timeout(Duration::from_secs(120))
                .build()
                .expect("failed to build HTTP client"),
            base_url: base_url.into(),
            api_key: api_key.into(),
            model: model.into(),
        }
    }

    async fn send_embed(
        &self,
        texts: Vec<String>,
    ) -> Result<Vec<Vec<f32>>, Box<dyn std::error::Error + Send + Sync>> {
        let url = format!("{}/embeddings", self.base_url.trim_end_matches('/'));
        let body = EmbedApiRequest {
            model: self.model.clone(),
            input: texts,
        };
        let config = RetryConfig::default();
        let resp_text = retry_with_backoff(&config, BackendError::should_retry, || {
            let req = self
                .client
                .post(&url)
                .bearer_auth(&self.api_key)
                .json(&body);
            async move {
                let resp = req
                    .send()
                    .await
                    .map_err(|e| BackendError::Network(e.into()))?;
                if !resp.status().is_success() {
                    let status = resp.status().as_u16();
                    let retry_after = resp
                        .headers()
                        .get(reqwest::header::RETRY_AFTER)
                        .and_then(|v| v.to_str().ok())
                        .and_then(parse_retry_after);
                    let body = resp.text().await.unwrap_or_default();
                    return Err(BackendError::Http {
                        status,
                        body,
                        retry_after,
                    });
                }
                resp.text()
                    .await
                    .map_err(|e| BackendError::Network(e.into()))
            }
        })
        .await?;
        let embed_resp: EmbedApiResponse = serde_json::from_str(&resp_text)?;
        Ok(embed_resp.data.into_iter().map(|d| d.embedding).collect())
    }
}

impl EmbedBackend for OpenAIEmbedBackend {
    fn embed(&self, texts: Vec<String>) -> EmbedResult<'_> {
        Box::pin(async move { self.send_embed(texts).await })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extended_cache_supported_models() {
        assert!(supports_extended_cache("gpt-4.1"));
        assert!(supports_extended_cache("gpt-4.1-mini"));
        assert!(supports_extended_cache("gpt-4.1-nano"));
        assert!(supports_extended_cache("gpt-5"));
        assert!(supports_extended_cache("gpt-5-codex"));
        assert!(supports_extended_cache("gpt-5.1"));
        assert!(supports_extended_cache("gpt-5.4"));
        assert!(supports_extended_cache("gpt-5.5"));
        assert!(supports_extended_cache("GPT-5.5")); // case insensitive
    }

    #[test]
    fn extended_cache_unsupported_models() {
        assert!(!supports_extended_cache("gpt-4o"));
        assert!(!supports_extended_cache("gpt-4o-mini"));
        assert!(!supports_extended_cache("gpt-4"));
        assert!(!supports_extended_cache("o3-mini"));
        assert!(!supports_extended_cache("claude-3-sonnet"));
        assert!(!supports_extended_cache("openai/gpt-5")); // OpenRouter-prefixed
        assert!(!supports_extended_cache("llama-3"));
    }
}
