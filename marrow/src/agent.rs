use std::collections::HashMap;
use std::error::Error;
use std::path::PathBuf;
use std::sync::Arc;

use reqwest::Client;
use tokio::sync::mpsc;

use crate::codegen;
use crate::events::{Event, EventLog};
use crate::memory::Memory;
use crate::model::ModelBackend;
use crate::secrets::Secrets;
use crate::session::Message;
use crate::toolbox::Toolbox;

/// Sender for progress updates from the agent loop.
/// Each message is a human-readable status string.
pub type ProgressTx = mpsc::UnboundedSender<String>;

const MAX_AGENT_STEPS: u32 = 10;

const AGENT_PROMPT_TEMPLATE: &str = r#"You are an agent that completes tasks step by step. Each turn you perform ONE action.

Available tools:
{tools}

{memories}{conversation}Task: {task}

CRITICAL: You have NO shell access. No curl, no bash, no command line. You can ONLY interact through the JSON actions below. Any other output format will be rejected.

You MUST respond with exactly one JSON object (no other text). Choose one:

To call an existing tool:
{{"action": "call_tool", "tool": "TOOL_NAME", "params": {{"KEY": "value"}}}}

To create a new tool (ONLY if no existing tool can do the job):
{{"action": "create_tool", "name": "new_tool_name", "description": "one line description of what it does"}}

To keep a tool you created (only if it worked well and is reusable):
{{"action": "keep_tool", "name": "tool_name"}}

To remove a broken persistent tool (ONLY if you are certain it is fundamentally broken, not just called wrong):
{{"action": "remove_tool", "name": "tool_name"}}

To give your final answer (ONLY when you have gathered enough data):
{{"action": "answer", "text": "your complete answer to the user"}}

Rules:
- Prefer existing persistent tools when they fit. But don't hesitate to create a new tool if none match — created tools are ephemeral (deleted after this task) so there's no cost to experimenting.
- After creating a tool, you MUST call it in your next turn — creation alone does nothing.
- If a tool fails, you may retry ONCE with different params. After two failures, create a new tool or try a different approach.
- If a created tool works well and is generic enough to be reusable, use keep_tool to save it permanently.
- Match tool to purpose: read each tool's description and output fields carefully. Consider ALL data a tool returns — check "returns" fields for secondary data before creating a new tool.
- When creating tools, prefer generic names (e.g. "rss_reader" not "nsg_blog_reader").
- Use known facts to fill in real parameter values (actual URLs, locations, etc.)
- The answer action text should be a natural language response, NOT a JSON action.

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
    KeepTool {
        name: String,
    },
    RemoveTool {
        name: String,
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
    secret_keys: &[&str],
    conversation: &[Message],
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
                    Action::KeepTool { name } => format!("Kept tool \"{name}\""),
                    Action::RemoveTool { name } => format!("Removed tool \"{name}\""),
                    Action::Answer { .. } => "Answered".to_string(),
                };
                let output_display = if s.output.len() > 1000 {
                    format!("{}... (truncated)", &s.output[..1000])
                } else {
                    s.output.clone()
                };
                format!(
                    "[Step {}] {}\nResult: {}\n",
                    s.step, action_desc, output_display
                )
            })
            .collect();
        format!("Previous actions:\n{}\n", entries.join("\n"))
    };

    let secrets_section = if secret_keys.is_empty() {
        String::new()
    } else {
        let list = secret_keys
            .iter()
            .map(|k| format!("- {k}"))
            .collect::<Vec<_>>()
            .join("\n");
        format!("Available secrets (use secret(\"name\") in Lua tools):\n{list}\n\n")
    };

    let conversation_section = if conversation.is_empty() {
        String::new()
    } else {
        let lines = conversation
            .iter()
            .map(|m| format!("{}: {}", m.role, m.content))
            .collect::<Vec<_>>()
            .join("\n");
        format!("Conversation so far:\n{lines}\n\n")
    };

    AGENT_PROMPT_TEMPLATE
        .replace("{tools}", tools_section)
        .replace("{memories}", &format!("{memories_section}{secrets_section}"))
        .replace("{conversation}", &conversation_section)
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
                "keep_tool" => {
                    if let Some(name) = raw.name.or(raw.tool) {
                        return Action::KeepTool { name };
                    }
                }
                "remove_tool" => {
                    if let Some(name) = raw.name.or(raw.tool) {
                        return Action::RemoveTool { name };
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
    secrets: Option<&Secrets>,
    progress: Option<&ProgressTx>,
    conversation: &[Message],
) -> Result<String, Box<dyn Error + Send + Sync>> {
    let emit = |msg: String| {
        if let Some(tx) = progress {
            let _ = tx.send(msg);
        }
    };

    let mut history: Vec<StepResult> = Vec::new();
    let mut tool_fail_counts: HashMap<String, u32> = HashMap::new();

    for step in 1..=MAX_AGENT_STEPS {
        let available_tools = toolbox.list_tools().unwrap_or_default();
        let tools_section = available_tools
            .iter()
            .map(|t| toolbox.tool_usage(t))
            .collect::<Vec<_>>()
            .join("\n");

        if log.is_verbose() {
            eprintln!("[agent] step {step}: tools shown to model:\n{tools_section}");
        }

        let secret_keys = secrets.map(|s| s.keys()).unwrap_or_default();
        let secret_key_refs: Vec<&str> = secret_keys.to_vec();
        let prompt = build_agent_prompt(
            task,
            &tools_section,
            memories,
            &history,
            &secret_key_refs,
            conversation,
        );
        let response = backend.complete(prompt).await?;

        if log.is_verbose() {
            eprintln!("[agent] step {step}: raw model response:\n{response}");
        }

        let action = parse_action(&response);

        if log.is_verbose() {
            match &action {
                Action::CallTool { tool, params } => {
                    let params_str = params
                        .iter()
                        .map(|(k, v)| format!("  {k} = \"{v}\""))
                        .collect::<Vec<_>>()
                        .join("\n");
                    let expected = toolbox.extract_params(tool);
                    eprintln!(
                        "[agent] step {step}: parsed call_tool \"{tool}\"\n  params passed:\n{params_str}\n  tool expects: {:?}",
                        expected
                    );
                }
                Action::CreateTool { name, description } => {
                    eprintln!("[agent] step {step}: parsed create_tool \"{name}\" — {description}");
                }
                Action::KeepTool { name } => {
                    eprintln!("[agent] step {step}: parsed keep_tool \"{name}\"");
                }
                Action::RemoveTool { name } => {
                    eprintln!("[agent] step {step}: parsed remove_tool \"{name}\"");
                }
                Action::Answer { text } => {
                    let preview = if text.len() > 200 {
                        format!("{}...", &text[..200])
                    } else {
                        text.clone()
                    };
                    eprintln!("[agent] step {step}: parsed answer — {preview}");
                }
            }
        }

        match &action {
            Action::CallTool { tool, params } => {
                // Enforce retry limit: skip tools that have failed too many times
                let fail_count = tool_fail_counts.get(tool.as_str()).copied().unwrap_or(0);
                if fail_count >= 2 {
                    if log.is_verbose() {
                        eprintln!(
                            "[agent] step {step}: BLOCKED tool \"{tool}\" — failed {fail_count} times already, forcing move on"
                        );
                    }
                    history.push(StepResult {
                        step,
                        action: action.clone(),
                        output: format!("{{\"error\": \"tool '{tool}' has failed {fail_count} times already. You MUST use a different tool or create a new one.\"}}"),
                    });
                    continue;
                }

                log.emit(Event::AgentAction {
                    task_id: task_id.to_string(),
                    step,
                    action_type: "call_tool".to_string(),
                    detail: tool.clone(),
                })
                .await;

                let params_preview = params
                    .iter()
                    .map(|(k, v)| format!("{k}={v}"))
                    .collect::<Vec<_>>()
                    .join(", ");
                if params_preview.is_empty() {
                    emit(format!("🔧 Calling tool \"{tool}\""));
                } else {
                    emit(format!("🔧 Calling tool \"{tool}\" ({params_preview})"));
                }

                // Normalize param keys to uppercase
                let upper_params: HashMap<String, String> = params
                    .iter()
                    .map(|(k, v)| (k.to_uppercase(), v.clone()))
                    .collect();

                if log.is_verbose() {
                    let upper_str = upper_params
                        .iter()
                        .map(|(k, v)| format!("  {k} = \"{v}\""))
                        .collect::<Vec<_>>()
                        .join("\n");
                    eprintln!(
                        "[agent] step {step}: executing tool \"{tool}\" with uppercase params:\n{upper_str}"
                    );
                }

                let output = match toolbox.load_provider(tool) {
                    Ok(provider) => {
                        let toolbox_dir = Some(PathBuf::from(toolbox_path));
                        match provider
                            .execute_with_params(
                                task,
                                client.clone(),
                                &upper_params,
                                toolbox_dir,
                                secrets,
                            )
                            .await
                        {
                            Ok(value) => {
                                let has_error = value.get("error").is_some();
                                if log.is_verbose() {
                                    let preview = {
                                        let s = value.to_string();
                                        if s.len() > 500 {
                                            format!("{}...", &s[..500])
                                        } else {
                                            s
                                        }
                                    };
                                    eprintln!(
                                        "[agent] step {step}: tool \"{tool}\" output: {preview}"
                                    );
                                }
                                if has_error {
                                    *tool_fail_counts.entry(tool.clone()).or_insert(0) += 1;
                                }
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
                                *tool_fail_counts.entry(tool.clone()).or_insert(0) += 1;
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

                emit(format!("⚙️ Creating tool \"{name}\""));

                let output = match codegen::generate_provider_for_agent(
                    name,
                    description,
                    task,
                    code_backend,
                    toolbox,
                    client.clone(),
                    &available_tools,
                    &secret_key_refs,
                )
                .await
                {
                    Ok(generated_name) => {
                        log.emit(Event::ToolGenerated {
                            name: generated_name.clone(),
                            description: description.clone(),
                        })
                        .await;
                        format!(
                            "Tool \"{generated_name}\" created successfully. You can now call it."
                        )
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

            Action::KeepTool { name } => {
                log.emit(Event::AgentAction {
                    task_id: task_id.to_string(),
                    step,
                    action_type: "keep_tool".to_string(),
                    detail: name.clone(),
                })
                .await;

                let output = match toolbox.keep_tool(name) {
                    Ok(()) => {
                        emit(format!("📌 Keeping tool \"{name}\""));
                        format!("Tool \"{name}\" marked as persistent.")
                    }
                    Err(e) => format!("Failed to keep tool: {e}"),
                };

                history.push(StepResult {
                    step,
                    action,
                    output,
                });
            }

            Action::RemoveTool { name } => {
                log.emit(Event::AgentAction {
                    task_id: task_id.to_string(),
                    step,
                    action_type: "remove_tool".to_string(),
                    detail: name.clone(),
                })
                .await;

                let output = match toolbox.delete_tool(name) {
                    Ok(()) => {
                        emit(format!("🗑️ Removed tool \"{name}\""));
                        format!("Tool \"{name}\" has been removed.")
                    }
                    Err(e) => format!("Failed to remove tool: {e}"),
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
                if !history.is_empty() {
                    emit("💭 Thinking...".to_string());
                }
                let answer =
                    format_answer(task, memories, &history, answer_backend, conversation).await;
                cleanup_ephemeral(toolbox, &emit);
                return answer;
            }
        }
    }

    // Max steps reached — force an answer with what we have
    cleanup_ephemeral(toolbox, &emit);
    format_answer(task, memories, &history, answer_backend, conversation).await
}

fn cleanup_ephemeral(toolbox: &Toolbox, emit: &impl Fn(String)) {
    match toolbox.cleanup_ephemeral() {
        Ok(removed) => {
            for name in &removed {
                emit(format!("🗑️ Removed ephemeral tool \"{name}\""));
            }
        }
        Err(e) => eprintln!("[agent] ephemeral cleanup error: {e}"),
    }
}

async fn format_answer(
    task: &str,
    memories: &[Memory],
    history: &[StepResult],
    answer_backend: &dyn ModelBackend,
    conversation: &[Message],
) -> Result<String, Box<dyn Error + Send + Sync>> {
    let conversation_section = if conversation.is_empty() {
        String::new()
    } else {
        let lines = conversation
            .iter()
            .map(|m| format!("{}: {}", m.role, m.content))
            .collect::<Vec<_>>()
            .join("\n");
        format!("Conversation so far:\n{lines}\n\n")
    };

    if history.is_empty() {
        // No tools were called — just answer directly
        let mut context = format!("{conversation_section}Task: {task}\n");
        if !memories.is_empty() {
            let facts = memories
                .iter()
                .map(|m| format!("- {}", m.fact))
                .collect::<Vec<_>>()
                .join("\n");
            context.push_str(&format!("\nKnown facts:\n{facts}\n"));
        }
        context.push_str("\nAnswer the user's question directly. If the conversation has prior context, use it.");
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

    let mut context = format!("{conversation_section}Task: {task}\n\nCollected data:\n{data}\n");
    if !memories.is_empty() {
        let facts = memories
            .iter()
            .map(|m| format!("- {}", m.fact))
            .collect::<Vec<_>>()
            .join("\n");
        context.push_str(&format!("\nKnown facts:\n{facts}\n"));
    }
    context.push_str("\nUsing the collected data above, answer the user's question. Be specific and include relevant details. If the conversation has prior context, use it.");

    answer_backend.complete(context).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_call_tool_action() {
        let input =
            r#"{"action": "call_tool", "tool": "weather", "params": {"LOCATION": "Tokyo"}}"#;
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
