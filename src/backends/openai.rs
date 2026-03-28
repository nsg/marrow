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
    messages: Vec<ApiMessage>,
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
}

#[derive(Debug, Deserialize)]
struct ResponseMessage {
    content: String,
}

#[derive(Debug, Deserialize)]
struct Usage {
    #[serde(default)]
    prompt_tokens: u64,
    #[serde(default)]
    completion_tokens: u64,
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
            client: Client::new(),
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
        let url = format!(
            "{}/chat/completions",
            self.base_url.trim_end_matches('/')
        );

        let api_messages: Vec<ApiMessage> = messages.iter().map(ApiMessage::from).collect();

        let body = ChatRequest {
            model: self.model.clone(),
            messages: api_messages,
        };

        let start = Instant::now();
        let resp = self
            .client
            .post(&url)
            .bearer_auth(&self.api_key)
            .json(&body)
            .send()
            .await?;
        let duration = start.elapsed();

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(format!("openai returned {status}: {text}").into());
        }

        let chat_resp: ChatResponse = resp.json().await?;

        if let Some(ref metrics) = self.metrics {
            let (prompt_tokens, completion_tokens) = chat_resp
                .usage
                .as_ref()
                .map(|u| (u.prompt_tokens, u.completion_tokens))
                .unwrap_or((0, 0));
            metrics.record(&self.role, duration, prompt_tokens, completion_tokens);
        }

        let content = chat_resp
            .choices
            .into_iter()
            .next()
            .map(|c| c.message.content)
            .ok_or("no choices in response")?;

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
