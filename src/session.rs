use std::error::Error;

use serde::{Deserialize, Serialize};

use crate::model::ModelBackend;

const SUMMARIZE_THRESHOLD: usize = 20;
const KEEP_RECENT: usize = 6;

const SUMMARIZE_PROMPT: &str = r#"Summarize the following conversation concisely. Capture key facts, decisions, and context that would be needed to continue the conversation naturally. Be brief.

Conversation:
{conversation}"#;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: String,
    pub content: String,
}

impl Message {
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: "user".to_string(),
            content: content.into(),
        }
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: "assistant".to_string(),
            content: content.into(),
        }
    }

    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: "system".to_string(),
            content: content.into(),
        }
    }
}

pub struct ChatSession {
    messages: Vec<Message>,
    summary: Option<String>,
}

impl ChatSession {
    pub fn new() -> Self {
        Self {
            messages: Vec::new(),
            summary: None,
        }
    }

    pub fn append(&mut self, message: Message) {
        self.messages.push(message);
    }

    pub fn needs_summarization(&self) -> bool {
        self.messages.len() > SUMMARIZE_THRESHOLD
    }

    pub fn build_messages(&self, system_context: Option<&str>) -> Vec<Message> {
        let mut result = Vec::new();

        // System message with context and/or summary
        let mut system_parts = Vec::new();
        if let Some(ctx) = system_context {
            system_parts.push(ctx.to_string());
        }
        if let Some(ref summary) = self.summary {
            system_parts.push(format!("Previous conversation summary: {summary}"));
        }
        if !system_parts.is_empty() {
            result.push(Message::system(system_parts.join("\n\n")));
        }

        // All messages (or recent if summarized)
        result.extend(self.messages.iter().cloned());

        result
    }

    pub async fn summarize(
        &mut self,
        backend: &dyn ModelBackend,
    ) -> Result<(), Box<dyn Error + Send + Sync>> {
        if self.messages.len() <= KEEP_RECENT {
            return Ok(());
        }

        let to_summarize = self.messages.len() - KEEP_RECENT;
        let old_messages = &self.messages[..to_summarize];

        let conversation = old_messages
            .iter()
            .map(|m| format!("{}: {}", m.role, m.content))
            .collect::<Vec<_>>()
            .join("\n");

        let prompt = SUMMARIZE_PROMPT.replace("{conversation}", &conversation);
        let new_summary = backend.complete(prompt).await?;

        // Merge with existing summary if present
        self.summary = Some(if let Some(ref existing) = self.summary {
            format!("{existing}\n\n{new_summary}")
        } else {
            new_summary
        });

        // Keep only recent messages
        self.messages = self.messages.split_off(to_summarize);

        Ok(())
    }

    pub fn message_count(&self) -> usize {
        self.messages.len()
    }
}
