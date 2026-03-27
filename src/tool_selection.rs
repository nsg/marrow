use std::error::Error;

use crate::model::ModelBackend;
use crate::toolbox::ToolMeta;

const SELECTION_PROMPT_TEMPLATE: &str = r#"You are a tool selection system. Given a task description and a list of available tools, decide which tools are needed to provide context for the task.

Available tools:
{tools}

Task: {task}

Respond with ONLY a JSON array of tool names to use. If no existing tools are relevant, respond with an empty array [].
Examples:
- ["time", "calendar"]
- ["weather"]
- []

Your response (JSON array only):"#;

pub fn build_selection_prompt(task_description: &str, tools: &[ToolMeta]) -> String {
    let tools_list = if tools.is_empty() {
        "(none available)".to_string()
    } else {
        tools
            .iter()
            .map(|t| format!("- {}: {}", t.name, t.description))
            .collect::<Vec<_>>()
            .join("\n")
    };

    SELECTION_PROMPT_TEMPLATE
        .replace("{tools}", &tools_list)
        .replace("{task}", task_description)
}

pub async fn select_tools(
    task_description: &str,
    tools: &[ToolMeta],
    backend: &dyn ModelBackend,
) -> Result<Vec<String>, Box<dyn Error + Send + Sync>> {
    if tools.is_empty() {
        return Ok(Vec::new());
    }

    let prompt = build_selection_prompt(task_description, tools);
    let response = backend.complete(prompt).await?;

    parse_tool_names(&response)
}

fn parse_tool_names(response: &str) -> Result<Vec<String>, Box<dyn Error + Send + Sync>> {
    // Find the JSON array in the response — model might include extra text
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
