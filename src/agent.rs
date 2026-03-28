use std::collections::HashMap;
use std::error::Error;
use std::path::PathBuf;
use std::sync::Arc;

use reqwest::Client;

use crate::codegen;
use crate::events::{Event, EventLog};
use crate::memory::Memory;
use crate::model::ModelBackend;
use crate::toolbox::Toolbox;

const MAX_AGENT_STEPS: u32 = 10;

const AGENT_PROMPT_TEMPLATE: &str = r#"You are an agent that completes tasks step by step. Each turn you perform ONE action.

Available tools:
{tools}

{memories}Task: {task}

You MUST respond with exactly one JSON object (no other text). Choose one:

To call an existing tool:
{{"action": "call_tool", "tool": "TOOL_NAME", "params": {{"KEY": "value"}}}}

To create a new tool that doesn't exist yet:
{{"action": "create_tool", "name": "new_tool_name", "description": "one line description of what it does"}}

To give your final answer (ONLY when you have gathered enough data):
{{"action": "answer", "text": "your complete answer to the user"}}

Important:
- After creating a tool, you MUST call it in your next turn — creation alone does nothing
- Use known facts to fill in real parameter values (actual URLs, locations, etc.)
- If a tool returns an error, try a different approach or create a better tool
- The answer action text should be a natural language response to the user, NOT a JSON action

{history}Your action:"#;

#[derive(Debug, Clone)]
pub enum Action {
    CallTool {
        tool: String,
        params: HashMap<String, String>,
    },
    CreateTool {
        name: String,
        description: String,
    },
    Answer {
        text: String,
    },
}

#[derive(Debug, Clone)]
pub struct StepResult {
    pub step: u32,
    pub action: Action,
    pub output: String,
}

pub fn build_agent_prompt(
    task: &str,
    tools_section: &str,
    memories: &[Memory],
    history: &[StepResult],
) -> String {
    let tools_section = if tools_section.is_empty() {
        "(none available — create one if needed)"
    } else {
        tools_section
    };

    let memories_section = if memories.is_empty() {
        String::new()
    } else {
        let facts = memories
            .iter()
            .map(|m| format!("- {}", m.fact))
            .collect::<Vec<_>>()
            .join("\n");
        format!("Known facts:\n{facts}\n\n")
    };

    let history_section = if history.is_empty() {
        String::new()
    } else {
        let entries: Vec<String> = history
            .iter()
            .map(|s| {
                let action_desc = match &s.action {
                    Action::CallTool { tool, params } => {
                        let params_str = params
                            .iter()
                            .map(|(k, v)| format!("{k}: {v}"))
                            .collect::<Vec<_>>()
                            .join(", ");
                        format!("Called tool \"{tool}\" with params {{{params_str}}}")
                    }
                    Action::CreateTool { name, .. } => {
                        format!("Created tool \"{name}\"")
                    }
                    Action::Answer { .. } => "Answered".to_string(),
                };
                let output_display = if s.output.len() > 1000 {
                    format!("{}... (truncated)", &s.output[..1000])
                } else {
                    s.output.clone()
                };
                format!("[Step {}] {}\nResult: {}\n", s.step, action_desc, output_display)
            })
            .collect();
        format!("Previous actions:\n{}\n", entries.join("\n"))
    };

    AGENT_PROMPT_TEMPLATE
        .replace("{tools}", tools_section)
        .replace("{memories}", &memories_section)
        .replace("{task}", task)
        .replace("{history}", &history_section)
}

pub fn parse_action(response: &str) -> Action {
    let trimmed = response.trim();

    let start = trimmed.find('{');
    let end = trimmed.rfind('}');

    if let (Some(s), Some(e)) = (start, end)
        && s < e
    {
        let json_str = &trimmed[s..=e];

        // If parsing fails, try fixing double-brace escaping from the model
        let json_str = if serde_json::from_str::<serde_json::Value>(json_str).is_err() {
            std::borrow::Cow::Owned(json_str.replace("{{", "{").replace("}}", "}"))
        } else {
            std::borrow::Cow::Borrowed(json_str)
        };
        let json_str = json_str.as_ref();

        #[derive(serde::Deserialize)]
        struct RawAction {
            action: String,
            #[serde(default)]
            tool: Option<String>,
            #[serde(default)]
            name: Option<String>,
            #[serde(default)]
            description: Option<String>,
            #[serde(default)]
            text: Option<String>,
            #[serde(default)]
            params: HashMap<String, serde_json::Value>,
        }

        if let Ok(raw) = serde_json::from_str::<RawAction>(json_str) {
            match raw.action.as_str() {
                "call_tool" => {
                    if let Some(tool) = raw.tool {
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
                        return Action::CallTool { tool, params };
                    }
                }
                "create_tool" => {
                    if let (Some(name), Some(description)) = (raw.name, raw.description) {
                        return Action::CreateTool { name, description };
                    }
                }
                "answer" => {
                    if let Some(text) = raw.text {
                        return Action::Answer { text };
                    }
                }
                _ => {}
            }
        }
    }

    // If we can't parse an action, treat the whole response as an answer
    Action::Answer {
        text: trimmed.to_string(),
    }
}

#[allow(clippy::too_many_arguments)]
pub async fn run_loop(
    task: &str,
    task_id: &str,
    backend: &dyn ModelBackend,
    answer_backend: &dyn ModelBackend,
    code_backend: &dyn ModelBackend,
    toolbox: &Toolbox,
    toolbox_path: &str,
    client: Arc<Client>,
    memories: &[Memory],
    log: &EventLog,
) -> Result<String, Box<dyn Error + Send + Sync>> {
    let mut history: Vec<StepResult> = Vec::new();

    for step in 1..=MAX_AGENT_STEPS {
        let available_tools = toolbox.list_tools().unwrap_or_default();
        let tools_section = available_tools
            .iter()
            .map(|t| toolbox.tool_usage(t))
            .collect::<Vec<_>>()
            .join("\n");
        let prompt = build_agent_prompt(task, &tools_section, memories, &history);
        let response = backend.complete(prompt).await?;
        let action = parse_action(&response);

        match &action {
            Action::CallTool { tool, params } => {
                log.emit(Event::AgentAction {
                    task_id: task_id.to_string(),
                    step,
                    action_type: "call_tool".to_string(),
                    detail: tool.clone(),
                })
                .await;

                // Normalize param keys to uppercase
                let upper_params: HashMap<String, String> = params
                    .iter()
                    .map(|(k, v)| (k.to_uppercase(), v.clone()))
                    .collect();

                let output = match toolbox.load_provider(tool) {
                    Ok(provider) => {
                        let toolbox_dir = Some(PathBuf::from(toolbox_path));
                        match provider
                            .execute_with_params(task, client.clone(), &upper_params, toolbox_dir)
                            .await
                        {
                            Ok(value) => {
                                let has_error = value.get("error").is_some();
                                log.emit(Event::AgentToolResult {
                                    task_id: task_id.to_string(),
                                    step,
                                    tool: tool.clone(),
                                    success: !has_error,
                                })
                                .await;
                                value.to_string()
                            }
                            Err(e) => {
                                log.emit(Event::AgentToolResult {
                                    task_id: task_id.to_string(),
                                    step,
                                    tool: tool.clone(),
                                    success: false,
                                })
                                .await;
                                format!("{{\"error\": \"tool execution failed: {e}\"}}")
                            }
                        }
                    }
                    Err(e) => {
                        format!("{{\"error\": \"tool not found: {e}\"}}")
                    }
                };

                history.push(StepResult {
                    step,
                    action,
                    output,
                });
            }

            Action::CreateTool { name, description } => {
                log.emit(Event::AgentAction {
                    task_id: task_id.to_string(),
                    step,
                    action_type: "create_tool".to_string(),
                    detail: name.clone(),
                })
                .await;

                let output =
                    match codegen::generate_provider_for_agent(name, description, task, code_backend, toolbox, client.clone(), &available_tools)
                        .await
                    {
                        Ok(generated_name) => {
                            log.emit(Event::ToolGenerated {
                                name: generated_name.clone(),
                                description: description.clone(),
                            })
                            .await;
                            format!("Tool \"{generated_name}\" created successfully. You can now call it.")
                        }
                        Err(e) => {
                            format!("Failed to create tool: {e}")
                        }
                    };

                history.push(StepResult {
                    step,
                    action,
                    output,
                });
            }

            Action::Answer { .. } => {
                log.emit(Event::AgentAction {
                    task_id: task_id.to_string(),
                    step,
                    action_type: "answer".to_string(),
                    detail: String::new(),
                })
                .await;
                return format_answer(task, memories, &history, answer_backend).await;
            }
        }
    }

    // Max steps reached — force an answer with what we have
    format_answer(task, memories, &history, answer_backend).await
}

async fn format_answer(
    task: &str,
    memories: &[Memory],
    history: &[StepResult],
    answer_backend: &dyn ModelBackend,
) -> Result<String, Box<dyn Error + Send + Sync>> {
    if history.is_empty() {
        // No tools were called — just answer directly
        let mut context = format!("Task: {task}\n");
        if !memories.is_empty() {
            let facts = memories.iter().map(|m| format!("- {}", m.fact)).collect::<Vec<_>>().join("\n");
            context.push_str(&format!("\nKnown facts:\n{facts}\n"));
        }
        context.push_str("\nAnswer the user's question directly.");
        return answer_backend.complete(context).await;
    }

    let data = history
        .iter()
        .filter_map(|s| match &s.action {
            Action::CallTool { tool, .. } => Some(format!("[{tool}]: {}", s.output)),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n\n");

    let mut context = format!("Task: {task}\n\nCollected data:\n{data}\n");
    if !memories.is_empty() {
        let facts = memories.iter().map(|m| format!("- {}", m.fact)).collect::<Vec<_>>().join("\n");
        context.push_str(&format!("\nKnown facts:\n{facts}\n"));
    }
    context.push_str("\nUsing the collected data above, answer the user's question. Be specific and include relevant details.");

    answer_backend.complete(context).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_call_tool_action() {
        let input = r#"{"action": "call_tool", "tool": "weather", "params": {"LOCATION": "Tokyo"}}"#;
        match parse_action(input) {
            Action::CallTool { tool, params } => {
                assert_eq!(tool, "weather");
                assert_eq!(params.get("LOCATION").unwrap(), "Tokyo");
            }
            other => panic!("expected CallTool, got {other:?}"),
        }
    }

    #[test]
    fn parse_create_tool_action() {
        let input = r#"{"action": "create_tool", "name": "blog_reader", "description": "Reads blog posts"}"#;
        match parse_action(input) {
            Action::CreateTool { name, description } => {
                assert_eq!(name, "blog_reader");
                assert_eq!(description, "Reads blog posts");
            }
            other => panic!("expected CreateTool, got {other:?}"),
        }
    }

    #[test]
    fn parse_answer_action() {
        let input = r#"{"action": "answer", "text": "The answer is 42."}"#;
        match parse_action(input) {
            Action::Answer { text } => {
                assert_eq!(text, "The answer is 42.");
            }
            other => panic!("expected Answer, got {other:?}"),
        }
    }

    #[test]
    fn parse_with_surrounding_text() {
        let input = r#"Here is my action: {"action": "answer", "text": "done"} hope that helps"#;
        match parse_action(input) {
            Action::Answer { text } => assert_eq!(text, "done"),
            other => panic!("expected Answer, got {other:?}"),
        }
    }

    #[test]
    fn parse_malformed_defaults_to_answer() {
        let input = "I don't know what to do";
        match parse_action(input) {
            Action::Answer { text } => assert_eq!(text, "I don't know what to do"),
            other => panic!("expected Answer, got {other:?}"),
        }
    }

    #[test]
    fn parse_numeric_params() {
        let input = r#"{"action": "call_tool", "tool": "test", "params": {"COUNT": 5}}"#;
        match parse_action(input) {
            Action::CallTool { params, .. } => {
                assert_eq!(params.get("COUNT").unwrap(), "5");
            }
            other => panic!("expected CallTool, got {other:?}"),
        }
    }
}
