use std::collections::HashMap;
use std::error::Error;

use crate::model::ModelBackend;
use crate::session::Message;
use crate::toolbox::ToolMeta;

const SELECTION_PROMPT_TEMPLATE: &str = r#"You are a tool selection system. Given a task description, conversation history, and a list of available tools, decide which tools are needed and what parameters to pass them.

IMPORTANT: If the task can be answered from conversation history alone (follow-up questions, chitchat, references to earlier messages), respond with empty tools.

Available tools:
{tools}

{history}Task: {task}

Respond with ONLY a JSON object in this exact format:
{{"tools": ["tool_name"], "params": {{"PARAM_NAME": "value"}}}}

The params object should contain uppercase global variables that tools will read.
Common params: LOCATION, TIMEZONE, QUERY, DATE, URL

If no tools are needed:
{{"tools": [], "params": {{}}}}

Your response (JSON only):"#;

#[derive(Debug)]
pub struct SelectionResult {
    pub tools: Vec<String>,
    pub params: HashMap<String, String>,
}

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
) -> Result<SelectionResult, Box<dyn Error + Send + Sync>> {
    if tools.is_empty() {
        return Ok(SelectionResult {
            tools: Vec::new(),
            params: HashMap::new(),
        });
    }

    let prompt = build_selection_prompt(task_description, tools, history);
    let response = backend.complete(prompt).await?;

    parse_selection(&response)
}

fn parse_selection(response: &str) -> Result<SelectionResult, Box<dyn Error + Send + Sync>> {
    let trimmed = response.trim();

    let start = trimmed.find('{');
    let end = trimmed.rfind('}');

    match (start, end) {
        (Some(s), Some(e)) if s < e => {
            let json_str = &trimmed[s..=e];

            #[derive(serde::Deserialize)]
            struct RawSelection {
                #[serde(default)]
                tools: Vec<String>,
                #[serde(default)]
                params: HashMap<String, serde_json::Value>,
            }

            let raw: RawSelection = serde_json::from_str(json_str).unwrap_or(RawSelection {
                tools: Vec::new(),
                params: HashMap::new(),
            });

            // Convert all param values to strings
            let params = raw
                .params
                .into_iter()
                .map(|(k, v)| {
                    let s = match v {
                        serde_json::Value::String(s) => s,
                        other => other.to_string(),
                    };
                    (k, s)
                })
                .collect();

            Ok(SelectionResult {
                tools: raw.tools,
                params,
            })
        }
        _ => Ok(SelectionResult {
            tools: Vec::new(),
            params: HashMap::new(),
        }),
    }
}
