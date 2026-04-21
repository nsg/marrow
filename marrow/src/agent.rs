use std::collections::HashMap;
use std::error::Error;
use std::sync::Arc;

use reqwest::Client;
use tokio::sync::mpsc;

use crate::events::{Event, EventLog};
use crate::memory::Memory;
use crate::model::ModelBackend;
use crate::secrets::Secrets;
use crate::session::Message;
use crate::tool::{ToolContext, ToolRegistry};

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
NOTE: For call_tool actions, pass secrets as param values with "secret:" prefix (e.g. "secret:my_api_key"). They are resolved automatically — the tool receives the actual value.
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

To save the last successful inline Lua as a reusable tool (saves the code from your most recent successful ```lua block):
{{"action": "save_tool", "name": "generic_tool_name", "description": "one line description"}}
IMPORTANT: save_tool must be its own action — do NOT combine it with a ```lua block in the same response.

To remove a broken tool from the toolbox:
{{"action": "remove_tool", "name": "tool_name"}}

To give your final answer:
{{"action": "answer", "text": "your complete answer to the user"}}

Rules:
- After inline Lua succeeds: if the code is generally useful (API calls, data fetching, etc.), save it with save_tool before answering. The flow is: (1) write and run inline Lua, (2) if it works, send save_tool as the next action, (3) then answer. Skip saving only if a tool with the same purpose already exists.
- CRITICAL: When a step already returned the data you need, do NOT rewrite the code. Use save_tool immediately on the last successful code. Rewriting working code risks regression — save first, then answer with the data you already have.
- If a tool or code fails, read the error carefully. Do NOT repeat the same approach — fix the specific issue.
- NEVER retry something that already failed with the same error. If "require" failed, it will always fail. If a secret name was not found, try a different name.
- If something worked in a previous step, reuse that exact approach. Do not regress to a pattern that already failed.
- Do NOT answer prematurely. If data collection failed, try a different approach before giving up. Only answer when you have actual data or have exhausted all reasonable approaches.
- If a follow-up question asks about different data (different dates, different items, etc.), you MUST fetch new data — previous conversation results do not cover it.
- If a saved tool fails repeatedly, use remove_tool to delete it — you can always recreate it or use inline Lua instead.
- Match tool to purpose: read each tool's description and output fields carefully. Consider ALL data a tool returns — check "returns" fields for secondary data before writing new code.
- When saving tools, prefer generic names and use PARAMS for inputs (e.g. PARAMS["LOCATION"] instead of hardcoded "Stockholm"). This makes tools reusable for different inputs.
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
    pub success: bool,
    /// One-line summary of what this step discovered or accomplished (for working context).
    pub finding: Option<String>,
}

pub fn build_agent_prompt(
    task: &str,
    tools_section: &str,
    memories: &[Memory],
    history: &[StepResult],
    secret_descriptions: &[(&str, &str)],
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
        const RECENT_STEPS: usize = 3;
        let total = history.len();
        let split = total.saturating_sub(RECENT_STEPS);

        let mut parts = Vec::new();

        // Working context: pinned findings from ALL successful steps
        let findings: Vec<String> = history
            .iter()
            .filter_map(|s| {
                s.finding
                    .as_ref()
                    .map(|f| format!("- Step {}: {f}", s.step))
            })
            .collect();
        if !findings.is_empty() {
            parts.push("Working context (confirmed discoveries):".to_string());
            parts.extend(findings);
            parts.push(String::new());
        }

        // Older steps: compressed differently based on success/failure
        if split > 0 {
            parts.push("Earlier actions:".to_string());
            for s in &history[..split] {
                let desc = format_action_short(&s.action);
                if s.success {
                    // Successful older steps: action + finding (or brief output)
                    let detail = s.finding.as_deref().unwrap_or("OK");
                    parts.push(format!("  Step {}: {} → {detail}", s.step, desc));
                } else {
                    // Failed older steps: action + brief error reason
                    let reason = extract_error_reason(&s.output);
                    parts.push(format!("  Step {}: {} → FAILED: {reason}", s.step, desc));
                }
            }
            parts.push(String::new());
        }

        // Recent steps: full detail for successes, compressed for failures
        parts.push("Recent actions:".to_string());
        for s in &history[split..] {
            let desc = format_action_short(&s.action);
            if s.success {
                let output_display = if s.output.len() > 1000 {
                    format!("{}... (truncated)", &s.output[..1000])
                } else {
                    s.output.clone()
                };
                parts.push(format!(
                    "[Step {}] {}\nResult: {}\n",
                    s.step, desc, output_display
                ));
            } else {
                // Failed recent steps: show error but not the full output
                let reason = extract_error_reason(&s.output);
                parts.push(format!("[Step {}] {} → FAILED: {reason}\n", s.step, desc));
            }
        }

        format!("{}\n", parts.join("\n"))
    };

    let secrets_section = if secret_descriptions.is_empty() {
        String::new()
    } else {
        let list = secret_descriptions
            .iter()
            .map(|(name, desc)| {
                if desc.is_empty() {
                    format!("- {name}")
                } else {
                    format!("- {name}: {desc}")
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        format!(
            "Available secrets (pass as \"secret:NAME\" in tool params, or use secret(\"NAME\") in Lua):\n{list}\n\n"
        )
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

    let datetime = chrono::Local::now()
        .format("%Y-%m-%d %H:%M (%A)")
        .to_string();

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
        Action::RunCode { .. } => "Ran inline Lua".to_string(),
        Action::SaveTool { name, .. } => format!("Saved tool \"{name}\""),
        Action::RemoveTool { name } => format!("Removed tool \"{name}\""),
        Action::Answer { .. } => "Answered".to_string(),
        Action::UserMessage { text } => format!("User: \"{text}\""),
    }
}

/// Extract a brief error reason from a failed step output.
fn extract_error_reason(output: &str) -> String {
    // Try to parse as JSON and extract "error" field
    if let Ok(val) = serde_json::from_str::<serde_json::Value>(output)
        && let Some(err) = val.get("error").and_then(|e| e.as_str())
    {
        let truncated = if err.len() > 120 {
            format!("{}...", &err[..120])
        } else {
            err.to_string()
        };
        return truncated;
    }
    // Fallback: first 120 chars of output
    if output.len() > 120 {
        format!("{}...", &output[..120])
    } else {
        output.to_string()
    }
}

/// Extract a one-line finding from a successful step output.
/// Uses template-based extraction from JSON — no model call needed.
fn extract_finding(output: &str) -> Option<String> {
    let val: serde_json::Value = serde_json::from_str(output).ok()?;

    // Handle array wrapping (inline Lua returns arrays)
    let obj = if let Some(arr) = val.as_array() {
        arr.first()?.as_object()?
    } else {
        val.as_object()?
    };

    // Skip if it contains an error
    if obj.contains_key("error") {
        return None;
    }

    // Build a summary from numeric/string fields (skip large nested data)
    let mut parts = Vec::new();
    for (key, value) in obj {
        match value {
            serde_json::Value::Number(n) => {
                parts.push(format!("{key}={n}"));
            }
            serde_json::Value::String(s) if s.len() <= 80 => {
                parts.push(format!("{key}=\"{}\"", s));
            }
            serde_json::Value::String(s) => {
                parts.push(format!("{key}=({} chars)", s.len()));
            }
            serde_json::Value::Bool(b) => {
                parts.push(format!("{key}={b}"));
            }
            serde_json::Value::Array(arr) => {
                parts.push(format!("{key}=[{} items]", arr.len()));
            }
            serde_json::Value::Object(map) => {
                parts.push(format!("{key}={{{} fields}}", map.len()));
            }
            serde_json::Value::Null => {}
        }
    }

    if parts.is_empty() {
        return None;
    }

    Some(parts.join(", "))
}

/// Use the fast model to summarize a complex/large output into a one-line finding.
/// Called only when extract_finding returns something too long or when the output
/// is large enough to warrant model-based summarization.
async fn summarize_finding(output: &str, fast_backend: &dyn ModelBackend) -> Option<String> {
    let truncated = if output.len() > 2000 {
        &output[..2000]
    } else {
        output
    };
    let prompt = format!(
        "Summarize what this tool output tells us in ONE short sentence (under 20 words). \
         Focus on what was discovered or confirmed — data structure, counts, key values. \
         Reply with ONLY the summary, no preamble.\n\nOutput:\n{truncated}"
    );
    match fast_backend.complete(prompt).await {
        Ok(summary) => {
            let s = summary.trim().to_string();
            if s.is_empty() || s.len() > 200 {
                None
            } else {
                Some(s)
            }
        }
        Err(_) => None,
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
    fast_backend: &dyn ModelBackend,
    registry: &ToolRegistry,
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

    let tool_ctx = ToolContext {
        client: client.clone(),
        secrets: Arc::new(secrets.cloned().unwrap_or_default()),
        task_description: task.to_string(),
    };

    let mut history: Vec<StepResult> = Vec::new();
    let mut tool_fail_counts: HashMap<String, u32> = HashMap::new();
    let mut last_successful_code: Option<String> = None; // tracked for save_tool action

    for step in 1..=MAX_AGENT_STEPS {
        // Drain any user messages that arrived since the last step
        if let Some(rx) = incoming.as_mut() {
            while let Ok(msg) = rx.try_recv() {
                history.push(StepResult {
                    step,
                    action: Action::UserMessage { text: msg.clone() },
                    output: msg,
                    success: true,
                    finding: None,
                });
            }
        }
        let available_tools = registry.list_all();
        let tools_section = available_tools
            .iter()
            .map(|t| t.usage_line())
            .collect::<Vec<_>>()
            .join("\n");

        if log.is_verbose() {
            eprintln!("[agent] step {step}: tools shown to model:\n{tools_section}");
        }

        let secret_descs = secrets.map(|s| s.descriptions()).unwrap_or_default();
        let prompt = build_agent_prompt(
            task,
            &tools_section,
            memories,
            &history,
            &secret_descs,
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
                    let expected = registry.extract_params(tool);
                    eprintln!(
                        "[agent] step {step}: parsed call_tool \"{tool}\"\n  params passed:\n{params_str}\n  tool expects: {:?}",
                        expected
                    );
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
                        success: false,
                        finding: None,
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

                let (output, step_success) =
                    match registry.execute_tool(tool, &upper_params, &tool_ctx).await {
                        Ok(value) => {
                            let has_error = value.get("error").is_some();
                            let output_str = value.to_string();
                            if log.is_verbose() {
                                let preview = if output_str.len() > 500 {
                                    format!("{}...", &output_str[..500])
                                } else {
                                    output_str.clone()
                                };
                                eprintln!("[agent] step {step}: tool \"{tool}\" output: {preview}");
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
                            (output_str, !has_error)
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
                            (output_str, false)
                        }
                    };

                let finding = if step_success {
                    let template = extract_finding(&output);
                    match template {
                        Some(ref s) if s.len() <= 150 => template,
                        _ if output.len() > 500 => summarize_finding(&output, fast_backend).await,
                        other => other,
                    }
                } else {
                    None
                };
                history.push(StepResult {
                    step,
                    action,
                    output,
                    success: step_success,
                    finding,
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
                let toolbox_dir = Some(registry.toolbox_path().to_path_buf());
                let (output, step_success) = match provider
                    .execute_with_params(
                        task,
                        client.clone(),
                        &HashMap::new(),
                        toolbox_dir,
                        secrets,
                        registry.builtins_arc(),
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
                        (output_str, true)
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
                        (output_str, false)
                    }
                };

                let finding = if step_success {
                    let template = extract_finding(&output);
                    match template {
                        Some(ref s) if s.len() <= 150 => template,
                        _ if output.len() > 500 => summarize_finding(&output, fast_backend).await,
                        other => other,
                    }
                } else {
                    None
                };
                history.push(StepResult {
                    step,
                    action,
                    output,
                    success: step_success,
                    finding,
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
                    match registry.toolbox().save_tool(&meta, code) {
                        Ok(()) => {
                            emit(format!("💾 Saved tool \"{name}\""));
                            format!("Tool \"{name}\" saved. You can now call it with call_tool.")
                        }
                        Err(e) => format!("Failed to save tool: {e}"),
                    }
                } else {
                    "No successful inline code to save. Run inline code first.".to_string()
                };

                let step_success = output.starts_with("Tool \"") && output.ends_with("call_tool.");
                history.push(StepResult {
                    step,
                    action,
                    output,
                    success: step_success,
                    finding: None,
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

                let output = match registry.toolbox().delete_tool(name) {
                    Ok(()) => {
                        emit(format!("🗑️ Removed tool \"{name}\""));
                        format!("Tool \"{name}\" has been removed.")
                    }
                    Err(e) => format!("Failed to remove tool: {e}"),
                };

                let step_success = !output.starts_with("Failed");
                history.push(StepResult {
                    step,
                    action,
                    output,
                    success: step_success,
                    finding: None,
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
                    registry,
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
        success: false,
        finding: None,
    });
    format_answer(
        task,
        memories,
        &history,
        answer_backend,
        conversation,
        registry,
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
    registry: &ToolRegistry,
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

    let tools_list = registry.list_all();
    let tools_section = if tools_list.is_empty() {
        "You have no tools installed yet. You can create tools on demand when tasks require external data.".to_string()
    } else {
        let list = tools_list
            .iter()
            .map(|t| {
                let marker = if t.builtin { " [built-in]" } else { "" };
                format!("- {}: {}{marker}", t.name, t.description)
            })
            .collect::<Vec<_>>()
            .join("\n");
        format!("Your installed tools:\n{list}")
    };

    let datetime = chrono::Local::now()
        .format("%Y-%m-%d %H:%M (%A)")
        .to_string();
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
