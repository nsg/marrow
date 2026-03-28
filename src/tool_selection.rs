use std::error::Error;

use crate::model::ModelBackend;
use crate::session::Message;
use crate::toolbox::ToolMeta;

const SELECTION_PROMPT_TEMPLATE: &str = r#"You are a tool selection system. Given a task description, conversation history, and a list of available tools, decide which tools are needed to provide external context for the task.

IMPORTANT: If the task can be answered from conversation history alone (follow-up questions, chitchat, references to earlier messages), respond with []. Only select tools when external data is genuinely needed.

Available tools:
{tools}

{history}Task: {task}

Respond with ONLY a JSON array of tool names to use. If no tools are needed, respond with [].
Examples:
- ["time", "calendar"]
- ["weather"]
- []

Your response (JSON array only):"#;

pub fn build_selection_prompt(
    task_description: &str,
    tools: &[ToolMeta],
    history: Option<&[Message]>,
) -> String {
    let tools_list = if tools.is_empty() {
        "(none available)".to_string()
    } else {
        tools
            .iter()
            .map(|t| format!("- {}: {}", t.name, t.description))
            .collect::<Vec<_>>()
            .join("\n")
    };

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

    SELECTION_PROMPT_TEMPLATE
        .replace("{tools}", &tools_list)
        .replace("{history}", &history_section)
        .replace("{task}", task_description)
}

pub async fn select_tools(
    task_description: &str,
    tools: &[ToolMeta],
    backend: &dyn ModelBackend,
    history: Option<&[Message]>,
) -> Result<Vec<String>, Box<dyn Error + Send + Sync>> {
    if tools.is_empty() {
        return Ok(Vec::new());
    }

    let prompt = build_selection_prompt(task_description, tools, history);
    let response = backend.complete(prompt).await?;

    parse_tool_names(&response)
}

fn parse_tool_names(response: &str) -> Result<Vec<String>, Box<dyn Error + Send + Sync>> {
    let trimmed = response.trim();

    let start = trimmed.find('[');
    let end = trimmed.rfind(']');

    match (start, end) {
        (Some(s), Some(e)) if s < e => {
            let json_str = &trimmed[s..=e];
            let names: Vec<String> = serde_json::from_str(json_str)?;
            Ok(names)
        }
        _ => Ok(Vec::new()),
    }
}
