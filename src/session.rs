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

impl Default for ChatSession {
    fn default() -> Self {
        Self::new()
    }
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_session_is_empty() {
        let s = ChatSession::new();
        assert!(!s.needs_summarization());
        let msgs = s.build_messages(None);
        assert!(msgs.is_empty());
    }

    #[test]
    fn append_and_build() {
        let mut s = ChatSession::new();
        s.append(Message::user("hello"));
        s.append(Message::assistant("hi"));

        let msgs = s.build_messages(None);
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].role, "user");
        assert_eq!(msgs[1].role, "assistant");
    }

    #[test]
    fn build_with_system_context() {
        let mut s = ChatSession::new();
        s.append(Message::user("test"));

        let msgs = s.build_messages(Some("You are helpful"));
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].role, "system");
        assert!(msgs[0].content.contains("You are helpful"));
    }

    #[test]
    fn build_with_summary() {
        let mut s = ChatSession::new();
        s.summary = Some("User asked about weather".to_string());
        s.append(Message::user("and now?"));

        let msgs = s.build_messages(None);
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].role, "system");
        assert!(msgs[0].content.contains("Previous conversation summary"));
        assert!(msgs[0].content.contains("weather"));
    }

    #[test]
    fn build_with_context_and_summary() {
        let mut s = ChatSession::new();
        s.summary = Some("talked about code".to_string());
        s.append(Message::user("next"));

        let msgs = s.build_messages(Some("Be concise"));
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].role, "system");
        assert!(msgs[0].content.contains("Be concise"));
        assert!(msgs[0].content.contains("talked about code"));
    }

    #[test]
    fn needs_summarization_threshold() {
        let mut s = ChatSession::new();
        for i in 0..SUMMARIZE_THRESHOLD {
            s.append(Message::user(format!("msg {i}")));
        }
        assert!(!s.needs_summarization());

        s.append(Message::user("one more"));
        assert!(s.needs_summarization());
    }
}
