use std::collections::HashMap;
use std::error::Error;

use crate::memory::Memory;
use crate::model::ModelBackend;
use crate::session::Message;
use crate::toolbox::ToolMeta;

const SELECTION_PROMPT_TEMPLATE: &str = r#"You are a tool selection system. Given a task description, conversation history, and a list of available tools, decide which single tool to use and what parameters to pass it.

IMPORTANT: If the task can be answered from conversation history alone (follow-up questions, chitchat, references to earlier messages), respond with no tool.

Available tools:
{tools}

{history}{memories}Task: {task}

Select ONE tool. If no existing tool fits, request a new one by name — it will be generated automatically.
If the task needs data from multiple sources, request a glue tool that composes them using run_tool() internally.

Use known facts about the user to fill in parameters (e.g., if you know their blog URL, pass it as a param). Always provide concrete values, not placeholders.

Respond with ONLY a JSON object:
{{"tool": "tool_name", "params": {{"PARAM_NAME": "value"}}}}

If no tools are needed:
{{"tool": null}}

Your response (JSON only):"#;

#[derive(Debug)]
pub struct SelectionResult {
    pub tool: Option<String>,
    pub params: HashMap<String, String>,
}

impl SelectionResult {
    pub fn is_empty(&self) -> bool {
        self.tool.is_none()
    }
}

pub fn build_selection_prompt(
    task_description: &str,
    tools: &[ToolMeta],
    history: Option<&[Message]>,
    memories: &[Memory],
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

    SELECTION_PROMPT_TEMPLATE
        .replace("{tools}", &tools_list)
        .replace("{history}", &history_section)
        .replace("{memories}", &memories_section)
        .replace("{task}", task_description)
}

pub async fn select_tools(
    task_description: &str,
    tools: &[ToolMeta],
    backend: &dyn ModelBackend,
    history: Option<&[Message]>,
    memories: &[Memory],
) -> Result<SelectionResult, Box<dyn Error + Send + Sync>> {
    let prompt = build_selection_prompt(task_description, tools, history, memories);
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
                tool: Option<String>,
                #[serde(default)]
                params: HashMap<String, serde_json::Value>,
            }

            let raw: RawSelection = serde_json::from_str(json_str).unwrap_or(RawSelection {
                tool: None,
                params: HashMap::new(),
            });

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
                tool: raw.tool,
                params,
            })
        }
        _ => Ok(SelectionResult {
            tool: None,
            params: HashMap::new(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_single_tool() {
        let r = parse_selection(r#"{"tool": "weather", "params": {"LOCATION": "Tokyo"}}"#).unwrap();
        assert_eq!(r.tool.as_deref(), Some("weather"));
        assert_eq!(r.params.get("LOCATION").unwrap(), "Tokyo");
    }

    #[test]
    fn parse_null_tool() {
        let r = parse_selection(r#"{"tool": null}"#).unwrap();
        assert!(r.is_empty());
    }

    #[test]
    fn parse_no_json() {
        let r = parse_selection("I don't know what tools to use").unwrap();
        assert!(r.is_empty());
    }

    #[test]
    fn parse_json_with_surrounding_text() {
        let r = parse_selection(
            r#"Here: {"tool": "time", "params": {"TIMEZONE": "UTC"}} done"#,
        )
        .unwrap();
        assert_eq!(r.tool.as_deref(), Some("time"));
        assert_eq!(r.params.get("TIMEZONE").unwrap(), "UTC");
    }

    #[test]
    fn parse_numeric_param_converted_to_string() {
        let r = parse_selection(r#"{"tool": "test", "params": {"COUNT": 5}}"#).unwrap();
        assert_eq!(r.params.get("COUNT").unwrap(), "5");
    }

    #[test]
    fn parse_empty_params() {
        let r = parse_selection(r#"{"tool": "greet", "params": {}}"#).unwrap();
        assert_eq!(r.tool.as_deref(), Some("greet"));
        assert!(r.params.is_empty());
    }

    #[test]
    fn parse_missing_params_defaults_empty() {
        let r = parse_selection(r#"{"tool": "greet"}"#).unwrap();
        assert_eq!(r.tool.as_deref(), Some("greet"));
        assert!(r.params.is_empty());
    }
}
