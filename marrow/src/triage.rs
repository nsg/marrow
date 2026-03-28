use std::error::Error;

use crate::memory::Memory;
use crate::model::ModelBackend;
use crate::session::Message;

const TRIAGE_PROMPT_TEMPLATE: &str = r#"Does this task require fetching external data (weather, time, calendar, APIs, etc.) to answer correctly?

Consider:
- If the answer is in the conversation history, say NO
- If the answer is in the user's stored memories, say NO
- If it's a follow-up question, greeting, or chitchat, say NO
- If it requires real-time data, live information, or external services, say YES
- If you can answer it from general knowledge alone, say NO

{history}{memories}Task: {task}

Respond with only YES or NO."#;

pub async fn needs_external_data(
    task_description: &str,
    backend: &dyn ModelBackend,
    history: Option<&[Message]>,
    memories: &[Memory],
) -> Result<bool, Box<dyn Error + Send + Sync>> {
    let history_section = if let Some(msgs) = history {
        if msgs.is_empty() {
            String::new()
        } else {
            let conversation = msgs
                .iter()
                .map(|m| format!("{}: {}", m.role, m.content))
                .collect::<Vec<_>>()
                .join("\n");
            format!("Conversation history:\n{conversation}\n\n")
        }
    } else {
        String::new()
    };

    let memories_section = if memories.is_empty() {
        String::new()
    } else {
        let facts = memories
            .iter()
            .map(|m| format!("- {}", m.fact))
            .collect::<Vec<_>>()
            .join("\n");
        format!("Known facts about the user:\n{facts}\n\n")
    };

    let prompt = TRIAGE_PROMPT_TEMPLATE
        .replace("{history}", &history_section)
        .replace("{memories}", &memories_section)
        .replace("{task}", task_description);

    let response = backend.complete(prompt).await?;
    let answer = response.trim().to_uppercase();

    Ok(answer.contains("YES"))
}
