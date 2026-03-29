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

/// Receiver for user messages injected mid-loop (e.g. Discord follow-ups).
pub type IncomingRx = mpsc::UnboundedReceiver<String>;

const MAX_AGENT_STEPS: u32 = 25;

const AGENT_PROMPT_TEMPLATE: &str = r#"You are an agent that completes tasks step by step. Each turn you perform ONE action.

Available tools:
{tools}

{memories}{conversation}Current date/time: {datetime}
Task: {task}

CRITICAL: You have NO shell access. No curl, no bash, no command line. You can ONLY interact through the actions below.

You can respond with EITHER a JSON action OR a Lua code block.

## Inline Lua code (preferred for one-off data fetching)

Write a ```lua code block and it will be executed in a sandbox with ONLY these functions:
- http_request({{ method, url, body?, headers? }}) / http_get(url) / http_post(url, body)
- json_parse(string) / json_encode(table)
- xml_parse(string) / xml_encode(table)
- secret(name) — retrieve API keys/passwords (ONLY names listed under "Available secrets" above)
- run_tool(name, params) — call an existing tool
- log(message)
- Standard Lua: string.*, table.*, math.*, tonumber, tostring, type, pairs, ipairs, pcall

UNAVAILABLE (sandboxed out): require, os, io, debug, dofile, loadfile, package, base64.
There is no base64 library — for HTTP Basic auth, embed credentials in the URL (https://user:pass@host).

Return a table with the results. Example:
```lua
local resp = http_get("https://api.example.com/data")
local data = json_parse(resp.body)
return {{ result = data }}
```

## JSON actions

To call an existing tool:
{{"action": "call_tool", "tool": "TOOL_NAME", "params": {{"KEY": "value"}}}}

To save the last successful inline code as a reusable tool:
{{"action": "save_tool", "name": "generic_tool_name", "description": "one line description"}}

To create a new tool via code generation (when you need something more complex):
{{"action": "create_tool", "name": "generic_tool_name", "description": "one line description"}}

To remove a broken tool from the toolbox:
{{"action": "remove_tool", "name": "tool_name"}}

To give your final answer:
{{"action": "answer", "text": "your complete answer to the user"}}

Rules:
- Use inline Lua for one-off tasks. If inline code works well and you'll need it again, use save_tool to keep it.
- After creating a tool, you MUST call it in your next turn — creation alone does nothing.
- If a tool or code fails, read the error carefully. Do NOT repeat the same approach — fix the specific issue.
- NEVER retry something that already failed with the same error. If "require" failed, it will always fail. If a secret name was not found, try a different name.
- If something worked in a previous step, reuse that exact approach. Do not regress to a pattern that already failed.
- Do NOT answer prematurely. If data collection failed, try a different approach before giving up. Only answer when you have actual data or have exhausted all reasonable approaches.
- If a saved tool fails repeatedly, use remove_tool to delete it — you can always recreate it or use inline Lua instead.
- Match tool to purpose: read each tool's description and output fields carefully. Consider ALL data a tool returns — check "returns" fields for secondary data before writing new code.
- When creating tools, prefer generic names (e.g. "rss_reader" not "nsg_blog_reader").
- Use known facts to fill in real parameter values (actual URLs, locations, etc.)
- The answer action text should be a natural language response, NOT a JSON action.
- If the user sends a follow-up message during your work, you'll see it in the history. Adjust your plan accordingly — they may be correcting, clarifying, or cancelling.

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
    RunCode {
        code: String,
    },
    SaveTool {
        name: String,
        description: String,
    },
    RemoveTool {
        name: String,
    },
    Answer {
        text: String,
    },
    UserMessage {
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
        const RECENT_STEPS: usize = 5;
        let total = history.len();
        let split = total.saturating_sub(RECENT_STEPS);

        let mut parts = Vec::new();

        // Older steps: one-line summaries
        if split > 0 {
            parts.push("Earlier actions (summary):".to_string());
            for s in &history[..split] {
                let desc = format_action_short(&s.action);
                let status = if s.output.contains("\"error\"") || s.output.contains("error") {
                    "FAILED"
                } else {
                    "OK"
                };
                parts.push(format!("  Step {}: {} → {status}", s.step, desc));
            }
            parts.push(String::new());
        }

        // Recent steps: full detail
        parts.push("Recent actions:".to_string());
        for s in &history[split..] {
            let desc = format_action_short(&s.action);
            let output_display = if s.output.len() > 1000 {
                format!("{}... (truncated)", &s.output[..1000])
            } else {
                s.output.clone()
            };
            parts.push(format!(
                "[Step {}] {}\nResult: {}\n",
                s.step, desc, output_display
            ));
        }

        format!("{}\n", parts.join("\n"))
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

    let datetime = chrono::Local::now().format("%Y-%m-%d %H:%M (%A)").to_string();

    AGENT_PROMPT_TEMPLATE
        .replace("{tools}", tools_section)
        .replace(
            "{memories}",
            &format!("{memories_section}{secrets_section}"),
        )
        .replace("{conversation}", &conversation_section)
        .replace("{datetime}", &datetime)
        .replace("{task}", task)
        .replace("{history}", &history_section)
}

pub fn parse_action(response: &str) -> Action {
    let trimmed = response.trim();

    // Check for inline Lua code block first
    if let Some(code) = extract_lua_block(trimmed) {
        return Action::RunCode { code };
    }

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
                "save_tool" => {
                    if let (Some(name), Some(description)) =
                        (raw.name.clone(), raw.description.clone())
                    {
                        return Action::SaveTool { name, description };
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

fn format_action_short(action: &Action) -> String {
    match action {
        Action::CallTool { tool, params } => {
            let params_str = params
                .iter()
                .map(|(k, v)| format!("{k}: {v}"))
                .collect::<Vec<_>>()
                .join(", ");
            format!("Called tool \"{tool}\" ({params_str})")
        }
        Action::CreateTool { name, .. } => format!("Created tool \"{name}\""),
        Action::RunCode { .. } => "Ran inline Lua".to_string(),
        Action::SaveTool { name, .. } => format!("Saved tool \"{name}\""),
        Action::RemoveTool { name } => format!("Removed tool \"{name}\""),
        Action::Answer { .. } => "Answered".to_string(),
        Action::UserMessage { text } => format!("User: \"{text}\""),
    }
}

fn extract_lua_block(text: &str) -> Option<String> {
    let start = text.find("```lua")?;
    let rest = &text[start + 6..];
    let newline = rest.find('\n')?;
    let rest = &rest[newline + 1..];
    let end = rest.find("```")?;
    let code = rest[..end].trim();
    if code.is_empty() {
        None
    } else {
        Some(code.to_string())
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
    mut incoming: Option<&mut IncomingRx>,
    formatting_hint: Option<&str>,
) -> Result<String, Box<dyn Error + Send + Sync>> {
    let emit = |msg: String| {
        if let Some(tx) = progress {
            let _ = tx.send(msg);
        }
    };

    let mut history: Vec<StepResult> = Vec::new();
    let mut tool_fail_counts: HashMap<String, u32> = HashMap::new();
    let mut last_successful_code: Option<String> = None;

    for step in 1..=MAX_AGENT_STEPS {
        // Drain any user messages that arrived since the last step
        if let Some(rx) = incoming.as_mut() {
            while let Ok(msg) = rx.try_recv() {
                history.push(StepResult {
                    step,
                    action: Action::UserMessage { text: msg.clone() },
                    output: msg,
                });
            }
        }
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
                Action::RunCode { code } => {
                    let preview = if code.len() > 200 {
                        format!("{}...", &code[..200])
                    } else {
                        code.clone()
                    };
                    eprintln!("[agent] step {step}: parsed run_code:\n{preview}");
                }
                Action::SaveTool { name, description } => {
                    eprintln!("[agent] step {step}: parsed save_tool \"{name}\" — {description}");
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
                Action::UserMessage { .. } => {}
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
                                let output_str = value.to_string();
                                if log.is_verbose() {
                                    let preview = if output_str.len() > 500 {
                                        format!("{}...", &output_str[..500])
                                    } else {
                                        output_str.clone()
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
                                    output: output_str.clone(),
                                })
                                .await;
                                output_str
                            }
                            Err(e) => {
                                let output_str =
                                    format!("{{\"error\": \"tool execution failed: {e}\"}}");
                                *tool_fail_counts.entry(tool.clone()).or_insert(0) += 1;
                                log.emit(Event::AgentToolResult {
                                    task_id: task_id.to_string(),
                                    step,
                                    tool: tool.clone(),
                                    success: false,
                                    output: output_str.clone(),
                                })
                                .await;
                                output_str
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

            Action::RunCode { code } => {
                log.emit(Event::AgentAction {
                    task_id: task_id.to_string(),
                    step,
                    action_type: "run_code".to_string(),
                    detail: String::new(),
                })
                .await;

                emit("⚡ Running inline Lua...".to_string());

                let provider = crate::context::LuaProvider::new("inline", code);
                let toolbox_dir = Some(PathBuf::from(toolbox_path));
                let output = match provider
                    .execute_with_params(
                        task,
                        client.clone(),
                        &HashMap::new(),
                        toolbox_dir,
                        secrets,
                    )
                    .await
                {
                    Ok(value) => {
                        last_successful_code = Some(code.clone());
                        let output_str = value.to_string();
                        log.emit(Event::AgentToolResult {
                            task_id: task_id.to_string(),
                            step,
                            tool: "inline".to_string(),
                            success: true,
                            output: output_str.clone(),
                        })
                        .await;
                        output_str
                    }
                    Err(e) => {
                        let output_str = format!("{{\"error\": \"inline code failed: {e}\"}}");
                        log.emit(Event::AgentToolResult {
                            task_id: task_id.to_string(),
                            step,
                            tool: "inline".to_string(),
                            success: false,
                            output: output_str.clone(),
                        })
                        .await;
                        output_str
                    }
                };

                history.push(StepResult {
                    step,
                    action,
                    output,
                });
            }

            Action::SaveTool { name, description } => {
                log.emit(Event::AgentAction {
                    task_id: task_id.to_string(),
                    step,
                    action_type: "save_tool".to_string(),
                    detail: name.clone(),
                })
                .await;

                let output = if let Some(ref code) = last_successful_code {
                    let meta = crate::toolbox::ToolMeta {
                        name: name.clone(),
                        description: description.clone(),
                        provides: vec![name.clone()],
                        validated: false,
                    };
                    match toolbox.save_tool(&meta, code) {
                        Ok(()) => {
                            emit(format!("💾 Saved tool \"{name}\""));
                            format!("Tool \"{name}\" saved. You can now call it with call_tool.")
                        }
                        Err(e) => format!("Failed to save tool: {e}"),
                    }
                } else {
                    "No successful inline code to save. Run inline code first.".to_string()
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

            Action::UserMessage { .. } => unreachable!(),

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
                let answer = format_answer(
                    task,
                    memories,
                    &history,
                    answer_backend,
                    conversation,
                    toolbox,
                    formatting_hint,
                )
                .await;
                return answer;
            }
        }
    }

    // Max steps reached — force an answer with what we have
    history.push(StepResult {
        step: MAX_AGENT_STEPS + 1,
        action: Action::UserMessage {
            text: String::new(),
        },
        output: "SYSTEM: You have run out of steps. Summarize what you accomplished and what remains unfinished so the user knows where things stand.".to_string(),
    });
    format_answer(
        task,
        memories,
        &history,
        answer_backend,
        conversation,
        toolbox,
        formatting_hint,
    )
    .await
}

async fn format_answer(
    task: &str,
    memories: &[Memory],
    history: &[StepResult],
    answer_backend: &dyn ModelBackend,
    conversation: &[Message],
    toolbox: &Toolbox,
    formatting_hint: Option<&str>,
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

    let tools_list = toolbox.list_tools().unwrap_or_default();
    let tools_section = if tools_list.is_empty() {
        "You have no tools installed yet. You can create tools on demand when tasks require external data.".to_string()
    } else {
        let list = tools_list
            .iter()
            .map(|t| format!("- {}: {}", t.name, t.description))
            .collect::<Vec<_>>()
            .join("\n");
        format!("Your installed tools:\n{list}")
    };

    let datetime = chrono::Local::now().format("%Y-%m-%d %H:%M (%A)").to_string();
    let system_context = format!(
        "You are Marrow, a workflow automation agent. You interact through Lua tools in a sandbox — you do NOT have shell access, curl, Python, or any command line tools. {tools_section}\n\nCurrent date/time: {datetime}\n\n"
    );

    if history.is_empty() {
        // No tools were called — just answer directly
        let mut context = format!("{system_context}{conversation_section}Task: {task}\n");
        if !memories.is_empty() {
            let facts = memories
                .iter()
                .map(|m| format!("- {}", m.fact))
                .collect::<Vec<_>>()
                .join("\n");
            context.push_str(&format!("\nKnown facts:\n{facts}\n"));
        }
        context.push_str("\nAnswer the user's question directly. If the conversation has prior context, use it. Only reference tools and capabilities you actually have.");
        if let Some(hint) = formatting_hint {
            context.push_str(&format!("\n\n{hint}"));
        }
        return answer_backend.complete(context).await;
    }

    let data = history
        .iter()
        .filter_map(|s| match &s.action {
            Action::CallTool { tool, .. } => Some(format!("[{tool}]: {}", s.output)),
            Action::RunCode { .. } => Some(format!("[inline]: {}", s.output)),
            Action::UserMessage { text } => Some(format!("[User follow-up]: {text}")),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n\n");

    let mut context =
        format!("{system_context}{conversation_section}Task: {task}\n\nCollected data:\n{data}\n");
    if !memories.is_empty() {
        let facts = memories
            .iter()
            .map(|m| format!("- {}", m.fact))
            .collect::<Vec<_>>()
            .join("\n");
        context.push_str(&format!("\nKnown facts:\n{facts}\n"));
    }
    context.push_str("\nUsing the collected data above, answer the user's question. Be specific and include relevant details. If the conversation has prior context, use it.");
    if let Some(hint) = formatting_hint {
        context.push_str(&format!("\n\n{hint}"));
    }

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

    #[test]
    fn parse_inline_lua_block() {
        let input = "Let me fetch that:\n```lua\nlocal r = http_get(\"https://example.com\")\nreturn r\n```";
        match parse_action(input) {
            Action::RunCode { code } => {
                assert!(code.contains("http_get"));
                assert!(code.contains("return r"));
            }
            other => panic!("expected RunCode, got {other:?}"),
        }
    }

    #[test]
    fn parse_lua_block_preferred_over_json() {
        // If response has both a lua block and JSON, lua wins (checked first)
        let input = "```lua\nreturn {}\n```\n{\"action\": \"answer\", \"text\": \"done\"}";
        assert!(matches!(parse_action(input), Action::RunCode { .. }));
    }

    #[test]
    fn parse_empty_lua_block_falls_through() {
        let input = "```lua\n\n```\n{\"action\": \"answer\", \"text\": \"done\"}";
        assert!(matches!(parse_action(input), Action::Answer { .. }));
    }
}
