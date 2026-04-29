use std::collections::HashMap;
use std::error::Error;
use std::sync::Arc;
use std::time::Instant;

use reqwest::Client;
use tokio::sync::mpsc;

use crate::events::{Event, EventLog};
use crate::memory::Memory;
use crate::metrics::StepTiming;
use crate::model::ModelBackend;
use crate::secrets::Secrets;
use crate::session::Message;
use crate::skills::SkillStore;
use crate::tool::{FrontendContext, ToolContext, ToolRegistry};

/// Result of a single agent loop run, returned to the runtime layer.
#[derive(Debug, Clone)]
pub struct LoopResult {
    pub answer: String,
    pub steps: u32,
    pub tool_calls: u32,
    pub code_runs: u32,
    /// True when the loop exhausted `MAX_AGENT_STEPS` and the answer was forced.
    pub hit_step_limit: bool,
    /// Per-step timing breakdown.
    pub step_timings: Vec<StepTiming>,
}

/// Structured progress updates from the agent loop.
/// Discord renders these as emoji reactions; CLI prints via `Display`.
#[derive(Debug, Clone)]
pub enum ProgressUpdate {
    // Temporary — paired start/end
    ToolCallStart, // 🔧
    ToolCallEnd,   // 🔧
    CodeRunStart,  // ⚡
    CodeRunEnd,    // ⚡
    Thinking,      // 💭

    // Persistent
    ToolCreated,   // ⚙️
    ToolRemoved,   // 🗑️
    MemoryNew,     // 🧠
    MemoryUpdated, // 📝
    MemoryCleared, // ♻️

    // Messages
    Notification(String), // 💬 Intermediate message to user
}

impl std::fmt::Display for ProgressUpdate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ToolCallStart => write!(f, "🔧 Calling tool"),
            Self::ToolCallEnd => write!(f, "🔧 Tool call done"),
            Self::CodeRunStart => write!(f, "⚡ Running inline Lua..."),
            Self::CodeRunEnd => write!(f, "⚡ Code execution done"),
            Self::Thinking => write!(f, "💭 Thinking..."),
            Self::ToolCreated => write!(f, "⚙️ New tool created"),
            Self::ToolRemoved => write!(f, "🗑️ Tool removed"),
            Self::MemoryNew => write!(f, "🧠 New memory"),
            Self::MemoryUpdated => write!(f, "📝 Memory updated"),
            Self::MemoryCleared => write!(f, "♻️ Memory cleaned"),
            Self::Notification(msg) => write!(f, "💬 {msg}"),
        }
    }
}

/// Sender for progress updates from the agent loop.
pub type ProgressTx = mpsc::UnboundedSender<ProgressUpdate>;

/// Receiver for user messages injected mid-loop (e.g. Discord follow-ups).
pub type IncomingRx = mpsc::UnboundedReceiver<String>;

const MAX_AGENT_STEPS: u32 = 25;

/// Character budget for the full prompt. Assumes ~1M-token context models with
/// ~2 chars/token for code-heavy content, minus headroom for the response.
const PROMPT_CHAR_BUDGET: usize = 1_800_000;

/// Number of most recent steps shown with full output detail in history.
const RECENT_STEPS: usize = 3;

/// Create a checkpoint after this many steps since the last checkpoint.
const CHECKPOINT_STEP_INTERVAL: u32 = 8;

/// A compacted summary of all history up to a certain step. When present,
/// the prompt shows the checkpoint text + only the steps that followed it.
#[derive(Debug, Clone)]
pub struct Checkpoint {
    /// Compacted summary produced by the fast model.
    pub text: String,
    /// This checkpoint covers all steps up to and including this step number.
    pub up_to_step: u32,
}

/// Static system prompt — identical across all steps and tasks.
/// Placed in the system message so API providers can cache it.
const AGENT_SYSTEM_PROMPT: &str = r#"You are an agent that completes tasks step by step.

CRITICAL: You have NO shell access. No curl, no bash, no command line. You can ONLY interact through the actions below.

Each response can contain any combination of ```lua code blocks and JSON actions. They all execute in parallel.

## Inline Lua code (preferred for one-off data fetching)

Write ```lua code blocks and they will be executed in a sandbox with ONLY these functions:
- http_request({ method, url, body?, headers? }) / http_get(url) / http_post(url, body)
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
return { result = data }
```

### Multiple Lua blocks & naming

You can write multiple ```lua blocks in one response. They run in parallel in separate sandboxes — they cannot access each other's variables or results. Name blocks with a `-- name: xyz` comment on the first line:

```lua
-- name: fetch_weather
local resp = http_get("https://api.example.com/weather")
return json_parse(resp.body)
```

```lua
-- name: fetch_news
local resp = http_get("https://api.example.com/news")
return json_parse(resp.body)
```

Results come back labeled by name (e.g. [fetch_weather]: ..., [fetch_news]: ...). If you omit the name, blocks are labeled by index ([inline 1], [inline 2]).

## JSON actions

Multiple JSON actions can appear in one response. Wrap them in a JSON array:
[{"action": "call_tool", "tool": "TOOL_A", "params": {}}, {"action": "call_tool", "tool": "TOOL_B", "params": {}}]

A single action can be a plain object (no array needed):
{"action": "call_tool", "tool": "TOOL_NAME", "params": {"KEY": "value"}}

Available actions:

**call_tool** — call an existing tool:
{"action": "call_tool", "tool": "TOOL_NAME", "params": {"KEY": "value"}}

**save_tool** — save a previously successful ```lua block as a reusable tool. Reference the block by name or index:
{"action": "save_tool", "name": "generic_tool_name", "description": "one line description", "block": "fetch_weather"}

**remove_tool** — remove a broken tool from the toolbox:
{"action": "remove_tool", "name": "tool_name"}

**progress** — send the user a progress update while you continue working (does NOT end the task):
{"action": "progress", "text": "status message"}

**load_skill** — load full procedural steps for a skill (see "Available skills" catalog):
{"action": "load_skill", "name": "skill-filename.md"}

**done** — give your final answer (this text is shown directly to the user — make it complete and well-formatted):
{"action": "done", "text": "your complete answer to the user"}
IMPORTANT: done MUST be the only action in a response. If you combine done with other actions (Lua blocks, tool calls, etc.), the done will be IGNORED and all other actions will execute. You will be asked to resubmit done on its own.

## Mixing actions

You can freely combine ```lua blocks with JSON actions (call_tool, save_tool, remove_tool, progress) in a single response. Everything executes in parallel. The only exception is done, which must appear alone.

Example — fetch data and save a previous block in the same response:
```lua
-- name: fetch_prices
local resp = http_get("https://api.example.com/prices")
return json_parse(resp.body)
```
{"action": "save_tool", "name": "weather_lookup", "description": "Fetches weather data", "block": "fetch_weather"}

## Rules

- After inline Lua succeeds: if the code is generally useful (API calls, data fetching, etc.), save it with save_tool. You can save a block in the same response as new work, or in a subsequent response.
- CRITICAL: When a step already returned the data you need, do NOT rewrite the code. Use save_tool referencing the successful block. Rewriting working code risks regression.
- If a tool or code fails, read the error carefully. Do NOT repeat the same approach — fix the specific issue.
- NEVER retry something that already failed with the same error. If "require" failed, it will always fail. If a secret name was not found, try a different name.
- If something worked in a previous step, reuse that exact approach. Do not regress to a pattern that already failed.
- Do NOT answer prematurely. If data collection failed, try a different approach before giving up. Only use done when you have actual data or have exhausted all reasonable approaches.
- If a follow-up question asks about different data (different dates, different items, etc.), you MUST fetch new data — previous conversation results do not cover it.
- If a saved tool fails repeatedly, use remove_tool to delete it — you can always recreate it or use inline Lua instead.
- Match tool to purpose: read each tool's description and output fields carefully. Consider ALL data a tool returns — check "returns" fields for secondary data before writing new code.
- When saving tools, prefer generic names and use PARAMS for inputs (e.g. PARAMS["LOCATION"] instead of hardcoded "Stockholm"). This makes tools reusable for different inputs.
- Use known facts to fill in real parameter values (actual URLs, locations, etc.)
- The done action text should be a natural language response, NOT a JSON action. It is sent directly to the user — include all relevant details from your findings.
- CRITICAL: Every response MUST contain at least one action (JSON or ```lua block). Plain text without an action will be treated as your final answer and sent to the user immediately. Do NOT output bare text as a thinking or planning step — if you are not ready to answer, use an action.
- Use progress sparingly — only when the user would genuinely benefit from an intermediate update during a long multi-step task.
- When you can do independent work in parallel (e.g. fetch from multiple APIs), use multiple ```lua blocks in one response instead of sequential steps.
- If the user sends a follow-up message during your work, you'll see it in the history. Adjust your plan accordingly — they may be correcting, clarifying, or cancelling."#;

/// Dynamic user prompt — ordered for prompt cache efficiency: stable sections
/// first (skills, memories, secrets, conversation, context, task), then
/// volatile sections (tools, history, datetime) so that cache-breaking changes
/// only invalidate the tail.
const AGENT_USER_PROMPT_TEMPLATE: &str = r#"{memories}{conversation}{execution_context}Task: {task}

Available tools:
{tools}

{history}Current date/time: {datetime}
Your action:"#;

#[derive(Debug, Clone)]
pub enum Action {
    CallTool {
        tool: String,
        params: HashMap<String, String>,
    },
    RunCode {
        name: String,
        code: String,
    },
    SaveTool {
        name: String,
        description: String,
        block: Option<String>,
    },
    RemoveTool {
        name: String,
    },
    Progress {
        text: String,
    },
    LoadSkill {
        name: String,
    },
    Done {
        text: String,
        /// True when the done signal was inferred from unparseable text (no explicit
        /// `{"action":"done"}` was found).  The main loop uses this to nudge
        /// the model back into the loop instead of accepting a half-finished
        /// response.
        fallback: bool,
    },
    UserMessage {
        text: String,
    },
}

/// Parsed response from the model: zero or more actions per turn.
#[derive(Debug, Clone)]
pub struct ParsedResponse {
    pub actions: Vec<Action>,
    /// Validation errors for malformed actions — fed back to the model so it can
    /// learn what it did wrong and self-correct.
    pub errors: Vec<String>,
}

impl Action {
    fn type_str(&self) -> &'static str {
        match self {
            Action::CallTool { .. } => "call_tool",
            Action::RunCode { .. } => "run_code",
            Action::SaveTool { .. } => "save_tool",
            Action::RemoveTool { .. } => "remove_tool",
            Action::LoadSkill { .. } => "load_skill",
            Action::Progress { .. } => "progress",
            Action::Done { .. } => "done",
            Action::UserMessage { .. } => "user_message",
        }
    }
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

#[allow(clippy::too_many_arguments)]
pub fn build_agent_prompt(
    task: &str,
    tools_section: &str,
    memories: &[Memory],
    skill_catalog: &[(String, String)],
    history: &[StepResult],
    checkpoint: Option<&Checkpoint>,
    secret_descriptions: &[(&str, &str)],
    conversation: &[Message],
    frontend: &str,
) -> Vec<Message> {
    let tools_section = if tools_section.is_empty() {
        "(none available — create one if needed)"
    } else {
        tools_section
    };

    let skills_section = if skill_catalog.is_empty() {
        String::new()
    } else {
        let lines: Vec<String> = skill_catalog
            .iter()
            .map(|(name, first_line)| format!("- {name}: {first_line}"))
            .collect();
        format!(
            "Available skills (use load_skill to get full steps):\n{}\n\n",
            lines.join("\n")
        )
    };

    let memories_section = if memories.is_empty() {
        String::new()
    } else {
        let facts = memories
            .iter()
            .map(|m| format!("- [{}] {}", m.id, m.fact))
            .collect::<Vec<_>>()
            .join("\n");
        format!(
            "Working memory (bracketed UUID is the ID param for memory_update / memory_delete / memory_search):\n{facts}\n\n"
        )
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

    let execution_context = match frontend {
        "scheduler" => concat!(
            "Execution context: This is a SCHEDULED run (no human in the loop).\n",
            "- There is no conversation history — you are starting fresh.\n",
            "- You cannot ask follow-up questions — no one will see them until the run is complete.\n",
            "- Your answer will be delivered to the frontend that created this schedule.\n",
            "- Save any important findings to working memory so future runs can build on them.\n\n",
        )
        .to_string(),
        "cli" => concat!(
            "Execution context: CLI run.\n",
            "- The user may be a human or an automated agent.\n",
            "- Your answer goes to stdout.\n\n",
        )
        .to_string(),
        "discord" => concat!(
            "Execution context: Discord conversation.\n",
            "- The user is a human chatting via Discord.\n",
            "- You can expect follow-up messages in the conversation history.\n\n",
        )
        .to_string(),
        other => format!("Execution context: {other}\n\n"),
    };

    let datetime = chrono::Local::now()
        .format("%Y-%m-%d %H:%M (%A)")
        .to_string();

    let history_section = build_history_section(history, checkpoint);

    let user_content = AGENT_USER_PROMPT_TEMPLATE
        .replace("{tools}", tools_section)
        .replace(
            "{memories}",
            &format!("{skills_section}{memories_section}{secrets_section}"),
        )
        .replace("{conversation}", &conversation_section)
        .replace("{execution_context}", &execution_context)
        .replace("{datetime}", &datetime)
        .replace("{task}", task)
        .replace("{history}", &history_section);

    vec![
        Message::system(AGENT_SYSTEM_PROMPT),
        Message::user(user_content),
    ]
}

// ---------------------------------------------------------------------------
// Step history — checkpoint-based compaction
// ---------------------------------------------------------------------------

/// Build the history section for the prompt. If a checkpoint exists, show the
/// checkpoint summary followed by only the steps that happened after it.
/// Otherwise show the full history with the existing compression scheme.
fn build_history_section(history: &[StepResult], checkpoint: Option<&Checkpoint>) -> String {
    if history.is_empty() && checkpoint.is_none() {
        return String::new();
    }

    let mut parts = Vec::new();

    // If we have a checkpoint, show it and only include steps after it
    let visible_history = if let Some(cp) = checkpoint {
        parts.push(format!(
            "Summary of steps 1–{}:\n{}\n",
            cp.up_to_step, cp.text
        ));
        history
            .iter()
            .filter(|s| s.step > cp.up_to_step)
            .collect::<Vec<_>>()
    } else {
        history.iter().collect()
    };

    if visible_history.is_empty() {
        if parts.is_empty() {
            return String::new();
        }
        return format!("{}\n", parts.join("\n"));
    }

    // Working context: findings from visible steps
    let findings: Vec<String> = visible_history
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

    let total = visible_history.len();
    let split = total.saturating_sub(RECENT_STEPS);

    // Earlier steps: one-line summaries
    if split > 0 {
        parts.push("Earlier actions:".to_string());
        for s in &visible_history[..split] {
            let desc = format_action_short(&s.action);
            if s.success {
                let detail = s.finding.as_deref().unwrap_or("OK");
                parts.push(format!("  Step {}: {} → {detail}", s.step, desc));
            } else {
                let reason = extract_error_reason(&s.output);
                parts.push(format!("  Step {}: {} → FAILED: {reason}", s.step, desc));
            }
        }
        parts.push(String::new());
    }

    // Recent steps: full detail
    parts.push("Recent actions:".to_string());
    for s in &visible_history[split..] {
        let desc = format_action_short(&s.action);
        if s.success {
            let output_display = truncate_str(&s.output, 1000);
            parts.push(format!(
                "[Step {}] {}\nResult: {}\n",
                s.step, desc, output_display
            ));
        } else {
            let reason = extract_error_reason(&s.output);
            parts.push(format!("[Step {}] {} → FAILED: {reason}\n", s.step, desc));
        }
    }

    format!("{}\n", parts.join("\n"))
}

/// Create a checkpoint by summarizing the current checkpoint (if any) plus
/// all steps since into a compact text using the fast model.
async fn create_checkpoint(
    history: &[StepResult],
    current_checkpoint: Option<&Checkpoint>,
    current_step: u32,
    fast_backend: &dyn ModelBackend,
) -> Option<Checkpoint> {
    // Gather steps to summarize: everything after the last checkpoint
    let steps_to_summarize: Vec<&StepResult> = if let Some(cp) = current_checkpoint {
        history.iter().filter(|s| s.step > cp.up_to_step).collect()
    } else {
        history.iter().collect()
    };

    if steps_to_summarize.is_empty() {
        return current_checkpoint.cloned();
    }

    // Build context for the summarizer
    let mut context = String::new();
    if let Some(cp) = current_checkpoint {
        context.push_str(&format!(
            "Previous summary (steps 1–{}):\n{}\n\n",
            cp.up_to_step, cp.text
        ));
    }

    context.push_str("Steps to incorporate:\n");
    for s in &steps_to_summarize {
        let desc = format_action_short(&s.action);
        if s.success {
            let summary = s.finding.as_deref().unwrap_or("OK");
            context.push_str(&format!("  Step {}: {} → {summary}\n", s.step, desc));
        } else {
            let reason = extract_error_reason(&s.output);
            context.push_str(&format!("  Step {}: {} → FAILED: {reason}\n", s.step, desc));
        }
    }

    let prompt = format!(
        "Summarize this agent execution history into a compact checkpoint. Include:\n\
         - What data was successfully retrieved and key values\n\
         - What approaches failed and why (so they are not retried)\n\
         - What tools were created or removed\n\
         Be concise — this replaces the full history in future prompts.\n\n\
         {context}"
    );

    match fast_backend.complete(prompt).await {
        Ok(summary) => {
            let text = summary.trim().to_string();
            if text.is_empty() {
                eprintln!("[agent] checkpoint creation failed: empty summary");
                return current_checkpoint.cloned();
            }
            eprintln!(
                "[agent] checkpoint created at step {current_step} ({} chars)",
                text.len()
            );
            Some(Checkpoint {
                text,
                up_to_step: current_step,
            })
        }
        Err(e) => {
            eprintln!("[agent] checkpoint creation failed: {e}");
            current_checkpoint.cloned()
        }
    }
}

/// Check whether a checkpoint should be created based on step count or prompt size.
fn needs_checkpoint(
    history: &[StepResult],
    checkpoint: Option<&Checkpoint>,
    prompt_chars: usize,
) -> bool {
    let steps_since = if let Some(cp) = checkpoint {
        history.iter().filter(|s| s.step > cp.up_to_step).count() as u32
    } else {
        history.len() as u32
    };

    // Trigger on step count
    if steps_since >= CHECKPOINT_STEP_INTERVAL {
        return true;
    }

    // Trigger on prompt size
    if prompt_chars > PROMPT_CHAR_BUDGET {
        return true;
    }

    false
}

fn truncate_chars_with_suffix(s: &str, limit: usize, suffix: &str) -> String {
    let mut chars = s.chars();
    let truncated = chars.by_ref().take(limit).collect::<String>();
    if chars.next().is_some() {
        format!("{truncated}{suffix}")
    } else {
        truncated
    }
}

fn truncate_preview(s: &str, limit: usize) -> String {
    truncate_chars_with_suffix(s, limit, "...")
}

/// Truncate a string to at most `limit` chars.
fn truncate_str(s: &str, limit: usize) -> String {
    truncate_chars_with_suffix(s, limit, "... (truncated)")
}

/// Parse a single JSON action string into an Action, or return a descriptive
/// error message that gets fed back to the model for self-correction.
fn parse_json_action(json_str: &str) -> Result<Action, String> {
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
        block: Option<String>,
        #[serde(default)]
        params: HashMap<String, serde_json::Value>,
    }

    let raw: RawAction = match serde_json::from_str(json_str) {
        Ok(r) => r,
        Err(e) => {
            // Check if it at least looks like a JSON action (has "action" key)
            if json_str.contains("\"action\"") {
                return Err(format!(
                    "Malformed JSON action: {e}. Fix the syntax and resubmit. \
                     Example: {{\"action\": \"call_tool\", \"tool\": \"TOOL_NAME\", \"params\": {{\"KEY\": \"value\"}}}}"
                ));
            }
            // Doesn't look like an action at all — skip silently (could be
            // stray JSON in the response text)
            return Err(String::new());
        }
    };

    let coerce_params = |params: HashMap<String, serde_json::Value>| -> HashMap<String, String> {
        params
            .into_iter()
            .map(|(k, v)| {
                let s = match v {
                    serde_json::Value::String(s) => s,
                    other => other.to_string(),
                };
                (k, s)
            })
            .collect()
    };

    match raw.action.as_str() {
        "call_tool" => {
            let tool = raw.tool.ok_or_else(|| {
                "call_tool action is missing the required \"tool\" field. \
                 Correct format: {\"action\": \"call_tool\", \"tool\": \"TOOL_NAME\", \"params\": {\"KEY\": \"value\"}}"
                    .to_string()
            })?;
            Ok(Action::CallTool {
                tool,
                params: coerce_params(raw.params),
            })
        }
        "save_tool" => {
            let name = raw.name.ok_or_else(|| {
                "save_tool action is missing the required \"name\" field. \
                 Correct format: {\"action\": \"save_tool\", \"name\": \"tool_name\", \"description\": \"...\", \"block\": \"block_name\"}"
                    .to_string()
            })?;
            let description = raw.description.ok_or_else(|| {
                format!(
                    "save_tool \"{name}\" is missing the required \"description\" field. \
                     Correct format: {{\"action\": \"save_tool\", \"name\": \"{name}\", \"description\": \"one line description\", \"block\": \"block_name\"}}"
                )
            })?;
            Ok(Action::SaveTool {
                name,
                description,
                block: raw.block,
            })
        }
        "remove_tool" => {
            let name = raw.name.or(raw.tool).ok_or_else(|| {
                "remove_tool action is missing the required \"name\" field. \
                 Correct format: {\"action\": \"remove_tool\", \"name\": \"tool_name\"}"
                    .to_string()
            })?;
            Ok(Action::RemoveTool { name })
        }
        "progress" => {
            let text = raw.text.filter(|t| !t.trim().is_empty()).ok_or_else(|| {
                "progress action requires a non-empty \"text\" field. \
                     Correct format: {\"action\": \"progress\", \"text\": \"status message\"}"
                    .to_string()
            })?;
            Ok(Action::Progress { text })
        }
        "load_skill" => {
            let name = raw.name.ok_or_else(|| {
                "load_skill action is missing the required \"name\" field. \
                 Correct format: {\"action\": \"load_skill\", \"name\": \"skill-name.md\"}"
                    .to_string()
            })?;
            Ok(Action::LoadSkill { name })
        }
        "done" => {
            let text = raw.text.ok_or_else(|| {
                "done action is missing the required \"text\" field. \
                 Correct format: {\"action\": \"done\", \"text\": \"your complete answer\"}"
                    .to_string()
            })?;
            Ok(Action::Done {
                text,
                fallback: false,
            })
        }
        // Unknown action name — if it has a tool field or params, treat as call_tool.
        // Models sometimes use the tool name as the action (e.g. "action": "remove_schedule").
        unknown => {
            let tool = raw.tool.unwrap_or_else(|| unknown.to_string());
            if !raw.params.is_empty() || tool != unknown {
                Ok(Action::CallTool {
                    tool,
                    params: coerce_params(raw.params),
                })
            } else {
                Err(format!(
                    "Unknown action \"{unknown}\". Available actions: call_tool, save_tool, \
                     remove_tool, progress, load_skill, done. To call a tool, use: \
                     {{\"action\": \"call_tool\", \"tool\": \"{unknown}\", \"params\": {{}}}}"
                ))
            }
        }
    }
}

/// Extract all JSON actions from the response text, ignoring anything inside
/// ```lua blocks. Supports both single objects and JSON arrays.
/// Returns (valid_actions, validation_errors).
fn extract_json_actions(text: &str) -> (Vec<Action>, Vec<String>) {
    // Remove ```lua...``` blocks from the text so we don't match JSON inside them
    let mut stripped = String::new();
    let mut pos = 0;
    while pos < text.len() {
        if let Some(start) = text[pos..].find("```lua") {
            stripped.push_str(&text[pos..pos + start]);
            let block_start = pos + start + 6;
            if let Some(nl) = text[block_start..].find('\n') {
                let code_start = block_start + nl + 1;
                if let Some(end) = text[code_start..].find("```") {
                    pos = code_start + end + 3;
                    continue;
                }
            }
            // Malformed block — just skip the marker
            stripped.push_str(&text[pos..pos + start + 6]);
            pos = block_start;
        } else {
            stripped.push_str(&text[pos..]);
            break;
        }
    }

    let mut actions = Vec::new();
    let mut errors = Vec::new();

    let collect_result =
        |result: Result<Action, String>, actions: &mut Vec<Action>, errors: &mut Vec<String>| {
            match result {
                Ok(action) => actions.push(action),
                Err(msg) if !msg.is_empty() => errors.push(msg),
                _ => {} // empty error = not an action at all, skip
            }
        };

    // Try to find a JSON array first: [{ ... }, { ... }]
    if let Some(arr_start) = stripped.find('[')
        && let Some(arr_end) = stripped.rfind(']')
    {
        let arr_str = &stripped[arr_start..=arr_end];
        if let Ok(arr) = serde_json::from_str::<Vec<serde_json::Value>>(arr_str) {
            for val in arr {
                if val.is_object() {
                    collect_result(
                        parse_json_action(&val.to_string()),
                        &mut actions,
                        &mut errors,
                    );
                }
            }
            if !actions.is_empty() || !errors.is_empty() {
                return (actions, errors);
            }
        }
    }

    // Fallback: find individual JSON objects by scanning for { ... }
    let stripped_bytes = stripped.as_bytes();
    let mut i = 0;
    while i < stripped.len() {
        if stripped_bytes[i] == b'{' {
            let mut depth = 0;
            let mut j = i;
            let mut in_string = false;
            let mut escape = false;
            while j < stripped.len() {
                let ch = stripped_bytes[j];
                if escape {
                    escape = false;
                } else if ch == b'\\' && in_string {
                    escape = true;
                } else if ch == b'"' {
                    in_string = !in_string;
                } else if !in_string {
                    if ch == b'{' {
                        depth += 1;
                    } else if ch == b'}' {
                        depth -= 1;
                        if depth == 0 {
                            let candidate = &stripped[i..=j];
                            collect_result(parse_json_action(candidate), &mut actions, &mut errors);
                            i = j + 1;
                            break;
                        }
                    }
                }
                j += 1;
            }
            if depth != 0 {
                i += 1; // malformed, skip
            }
        } else {
            i += 1;
        }
    }

    (actions, errors)
}

/// Parse the full model response into a ParsedResponse containing all actions.
pub fn parse_response(response: &str) -> ParsedResponse {
    let trimmed = response.trim();

    let lua_blocks = extract_lua_blocks(trimmed);
    let (json_actions, errors) = extract_json_actions(trimmed);

    let mut actions: Vec<Action> = lua_blocks
        .into_iter()
        .map(|(name, code)| Action::RunCode { name, code })
        .collect();
    actions.extend(json_actions);

    // If nothing was parsed, treat the whole response as a fallback done
    if actions.is_empty() {
        actions.push(Action::Done {
            text: trimmed.to_string(),
            fallback: true,
        });
    }

    ParsedResponse { actions, errors }
}

/// Backward-compatible single-action parse (for tests and simple call sites).
pub fn parse_action(response: &str) -> Action {
    let parsed = parse_response(response);
    parsed.actions.into_iter().next().unwrap_or(Action::Done {
        text: response.trim().to_string(),
        fallback: true,
    })
}

/// Heuristic: does this text look like the model was mid-thought rather than
/// giving a complete answer?  Used to decide whether a fallback answer should
/// be nudged back into the agent loop.
fn looks_incomplete(text: &str) -> bool {
    let trimmed = text.trim();

    // Backend-injected truncation marker (token limit hit)
    if trimmed.ends_with("[response truncated by token limit]") {
        return true;
    }

    // Trailing punctuation that signals "I was about to list/continue"
    if trimmed.ends_with(':') || trimmed.ends_with("...") || trimmed.ends_with('\u{2014}') {
        return true;
    }

    // Future-intent phrases — the model is narrating what it's *about* to do
    let lower = trimmed.to_lowercase();
    let intent_phrases = [
        "i will ",
        "i'll ",
        "let me ",
        "let's ",
        "i'm going to ",
        "i am going to ",
        "checking ",
        "i need to ",
        "first, i",
        "sure, i",
    ];
    for phrase in &intent_phrases {
        if lower.contains(phrase) {
            return true;
        }
    }

    false
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
        Action::RunCode { name, .. } => format!("Ran inline Lua [{name}]"),
        Action::SaveTool { name, .. } => format!("Saved tool \"{name}\""),
        Action::RemoveTool { name } => format!("Removed tool \"{name}\""),
        Action::LoadSkill { name } => format!("Loaded skill \"{name}\""),
        Action::Progress { text } => {
            format!("Progress: \"{}\"", truncate_preview(text, 60))
        }
        Action::Done { fallback, .. } => {
            if *fallback {
                "Done (fallback — no action parsed)".to_string()
            } else {
                "Done".to_string()
            }
        }
        Action::UserMessage { text } => format!("User: \"{text}\""),
    }
}

/// Extract a brief error reason from a failed step output.
fn extract_error_reason(output: &str) -> String {
    // Try to parse as JSON and extract "error" field
    if let Ok(val) = serde_json::from_str::<serde_json::Value>(output)
        && let Some(err) = val.get("error").and_then(|e| e.as_str())
    {
        return truncate_preview(err, 120);
    }
    // Fallback: first 120 chars of output
    truncate_preview(output, 120)
}

fn is_sensitive_key(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    key.contains("token")
        || key.contains("password")
        || key.contains("secret")
        || key.contains("api_key")
        || key == "key"
        || key.contains("authorization")
        || key.contains("cookie")
}

fn format_param_for_log(key: &str, value: &str) -> String {
    if is_sensitive_key(key) {
        format!("  {key} = \"[redacted]\"")
    } else {
        format!("  {key} = \"{value}\"")
    }
}

/// Produce a finding for the working context.
///
/// Small outputs (≤ 800 chars) are used verbatim — the agent can read raw
/// JSON just fine and it preserves more information than any summary.
/// Large outputs are summarized by the fast model.
const FINDING_INLINE_LIMIT: usize = 800;

async fn make_finding(output: &str, fast_backend: &dyn ModelBackend) -> Option<String> {
    if output.len() <= FINDING_INLINE_LIMIT {
        return Some(output.to_string());
    }

    // Large output — ask the fast model for a one-line summary
    let truncated = output.chars().take(2000).collect::<String>();
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

/// Extract all ```lua blocks from a response. Each block can optionally be
/// named with `-- name: xyz` on the first line. Unnamed blocks get indexed
/// names: "inline 1", "inline 2", etc.
fn extract_lua_blocks(text: &str) -> Vec<(String, String)> {
    let mut blocks = Vec::new();
    let mut unnamed_idx = 0u32;
    let mut search_from = 0;

    while let Some(start) = text[search_from..].find("```lua") {
        let abs_start = search_from + start + 6;
        let Some(newline) = text[abs_start..].find('\n') else {
            break;
        };
        let code_start = abs_start + newline + 1;
        let Some(end) = text[code_start..].find("```") else {
            break;
        };
        let code = text[code_start..code_start + end].trim();
        search_from = code_start + end + 3;

        if code.is_empty() {
            continue;
        }

        // Check for -- name: xyz on the first line
        let (name, code) = if let Some(rest) = code.strip_prefix("-- name:") {
            if let Some(nl) = rest.find('\n') {
                let name = rest[..nl].trim().to_string();
                let remaining = rest[nl + 1..].trim();
                if remaining.is_empty() {
                    continue;
                }
                (name, remaining.to_string())
            } else {
                // Only a name line, no actual code
                continue;
            }
        } else {
            unnamed_idx += 1;
            (format!("inline {unnamed_idx}"), code.to_string())
        };

        blocks.push((name, code));
    }

    blocks
}

fn loop_stats(history: &[StepResult]) -> (u32, u32) {
    let tool_calls = history
        .iter()
        .filter(|s| matches!(s.action, Action::CallTool { .. }))
        .count() as u32;
    let code_runs = history
        .iter()
        .filter(|s| matches!(s.action, Action::RunCode { .. }))
        .count() as u32;
    (tool_calls, code_runs)
}

#[allow(clippy::too_many_arguments)]
pub async fn run_loop(
    task: &str,
    task_id: &str,
    backend: &dyn ModelBackend,
    fast_backend: &dyn ModelBackend,
    registry: Arc<ToolRegistry>,
    client: Arc<Client>,
    memories: &[Memory],
    skill_store: &SkillStore,
    log: &EventLog,
    secrets: Option<&Secrets>,
    progress: Option<&ProgressTx>,
    conversation: &[Message],
    mut incoming: Option<&mut IncomingRx>,
    formatting_hint: Option<&str>,
    schedule_store: Option<Arc<crate::schedule::ScheduleStore>>,
    memory_store: Option<Arc<crate::memory::MemoryStore>>,
    frontend_context: Option<FrontendContext>,
    frontend: &str,
) -> Result<LoopResult, Box<dyn Error + Send + Sync>> {
    let emit = |update: ProgressUpdate| {
        if let Some(tx) = progress {
            let _ = tx.send(update);
        }
    };

    let tool_ctx = ToolContext {
        client: client.clone(),
        secrets: Arc::new(secrets.cloned().unwrap_or_default()),
        task_description: task.to_string(),
        schedule_store,
        memory_store,
        frontend_context,
    };

    let mut history: Vec<StepResult> = Vec::new();
    let mut checkpoint: Option<Checkpoint> = None;
    let mut step_timings: Vec<StepTiming> = Vec::new();
    let mut tool_fail_counts: HashMap<String, u32> = HashMap::new();
    let mut last_successful_code: Option<String> = None; // fallback for save_tool without block ref
    let mut successful_blocks: HashMap<String, String> = HashMap::new(); // name -> code
    let mut incomplete_nudges: u32 = 0;
    const MAX_INCOMPLETE_NUDGES: u32 = 2;

    for step in 1..=MAX_AGENT_STEPS {
        let step_start = Instant::now();

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

        let skill_catalog = skill_store.catalog().unwrap_or_default();
        let secret_descs = secrets.map(|s| s.descriptions()).unwrap_or_default();
        let mut messages = build_agent_prompt(
            task,
            &tools_section,
            memories,
            &skill_catalog,
            &history,
            checkpoint.as_ref(),
            &secret_descs,
            conversation,
            frontend,
        );

        // Check if we need a checkpoint before sending
        let prompt_chars: usize = messages.iter().map(|m| m.content.len()).sum();
        if needs_checkpoint(&history, checkpoint.as_ref(), prompt_chars) {
            checkpoint =
                create_checkpoint(&history, checkpoint.as_ref(), step - 1, fast_backend).await;
            // Rebuild the prompt with the new checkpoint
            messages = build_agent_prompt(
                task,
                &tools_section,
                memories,
                &skill_catalog,
                &history,
                checkpoint.as_ref(),
                &secret_descs,
                conversation,
                frontend,
            );
        }

        let response = backend.complete_chat(messages).await?;

        log.emit(Event::AgentModelResponse {
            task_id: task_id.to_string(),
            step,
            response_len: response.len(),
        })
        .await;

        let parsed = parse_response(&response);
        let mut actions = parsed.actions;

        // Feed validation errors back into history so the model can self-correct
        if !parsed.errors.is_empty() {
            let error_feedback = format!(
                "SYSTEM: {} of your actions had validation errors:\n{}",
                parsed.errors.len(),
                parsed
                    .errors
                    .iter()
                    .enumerate()
                    .map(|(i, e)| format!("  {}. {e}", i + 1))
                    .collect::<Vec<_>>()
                    .join("\n")
            );
            if log.is_verbose() {
                eprintln!(
                    "[agent] step {step}: {} validation error(s) in parsed actions",
                    parsed.errors.len()
                );
                for err in &parsed.errors {
                    eprintln!("[agent] step {step}:   validation error: {err}");
                }
            }
            history.push(StepResult {
                step,
                action: Action::UserMessage {
                    text: String::new(),
                },
                output: error_feedback,
                success: false,
                finding: None,
            });
        }

        if log.is_verbose() {
            eprintln!("[agent] step {step}: parsed {} action(s)", actions.len());
            for a in &actions {
                match a {
                    Action::CallTool { tool, params } => {
                        let params_str = params
                            .iter()
                            .map(|(k, v)| format_param_for_log(k, v))
                            .collect::<Vec<_>>()
                            .join("\n");
                        let expected = registry.extract_params(tool);
                        eprintln!(
                            "[agent] step {step}:   call_tool \"{tool}\"\n  params passed:\n{params_str}\n  tool expects: {:?}",
                            expected
                        );
                    }
                    Action::RunCode { name, code } => {
                        let preview = truncate_preview(code, 200);
                        eprintln!("[agent] step {step}:   run_code [{name}]:\n{preview}");
                    }
                    Action::SaveTool {
                        name,
                        description,
                        block,
                    } => {
                        eprintln!(
                            "[agent] step {step}:   save_tool \"{name}\" — {description} (block: {block:?})"
                        );
                    }
                    Action::RemoveTool { name } => {
                        eprintln!("[agent] step {step}:   remove_tool \"{name}\"");
                    }
                    Action::Progress { text } => {
                        let preview = truncate_preview(text, 200);
                        eprintln!("[agent] step {step}:   progress — {preview}");
                    }
                    Action::LoadSkill { name } => {
                        eprintln!("[agent] step {step}:   load_skill \"{name}\"");
                    }
                    Action::Done { text, fallback } => {
                        let preview = truncate_preview(text, 200);
                        let tag = if *fallback { "fallback done" } else { "done" };
                        eprintln!("[agent] step {step}:   {tag} — {preview}");
                    }
                    Action::UserMessage { .. } => {}
                }
            }
        }

        // Check for done exclusivity: if done is mixed with other actions,
        // drop the done and inform the model.
        let has_done = actions.iter().any(|a| matches!(a, Action::Done { .. }));
        let has_other = actions.iter().any(|a| !matches!(a, Action::Done { .. }));
        if has_done && has_other {
            actions.retain(|a| !matches!(a, Action::Done { .. }));
            // We'll append a system message after executing the other actions
        }

        // If the only action is a single Done, handle it directly (with nudging)
        if actions.len() == 1 && matches!(actions[0], Action::Done { .. }) {
            let action = actions.remove(0);
            if let Action::Done { ref text, fallback } = action {
                let is_incomplete = if fallback {
                    looks_incomplete(text)
                } else {
                    let t = text.trim();
                    t.ends_with(':') || t.ends_with("...") || t.ends_with('\u{2014}')
                };

                if is_incomplete && !history.is_empty() && incomplete_nudges < MAX_INCOMPLETE_NUDGES
                {
                    incomplete_nudges += 1;
                    let kind = if fallback { "fallback" } else { "explicit" };
                    if log.is_verbose() {
                        eprintln!(
                            "[agent] step {step}: {kind} done looks incomplete, nudging model (attempt {incomplete_nudges}/{MAX_INCOMPLETE_NUDGES})"
                        );
                    }
                    let nudge_msg = if fallback {
                        "SYSTEM: Your response was plain text without a valid action. \
                         If you are not done, respond with your next action as a JSON \
                         object or a ```lua block. If you are done, use \
                         {\"action\": \"done\", \"text\": \"...\"}."
                    } else {
                        "SYSTEM: Your response text ends mid-thought — it looks like you \
                         were about to take further action. If you still have work to \
                         do, respond with your next action as a JSON object or ```lua \
                         block. If you are truly done, resubmit with a complete response."
                    };
                    history.push(StepResult {
                        step,
                        action,
                        output: nudge_msg.to_string(),
                        success: false,
                        finding: None,
                    });
                    let step_dur = step_start.elapsed();
                    step_timings.push(StepTiming {
                        step,
                        action: "done_nudge".to_string(),
                        duration: step_dur,
                    });
                    log.emit(Event::StepCompleted {
                        task_id: task_id.to_string(),
                        step,
                        action_type: "done_nudge".to_string(),
                        duration_ms: step_dur.as_millis() as u64,
                        success: false,
                    })
                    .await;
                    continue;
                }

                log.emit(Event::AgentAction {
                    task_id: task_id.to_string(),
                    step,
                    action_type: "done".to_string(),
                    detail: String::new(),
                })
                .await;

                if !text.trim().is_empty() {
                    let step_dur = step_start.elapsed();
                    step_timings.push(StepTiming {
                        step,
                        action: "done".to_string(),
                        duration: step_dur,
                    });
                    log.emit(Event::StepCompleted {
                        task_id: task_id.to_string(),
                        step,
                        action_type: "done".to_string(),
                        duration_ms: step_dur.as_millis() as u64,
                        success: true,
                    })
                    .await;
                    let (tool_calls, code_runs) = loop_stats(&history);
                    return Ok(LoopResult {
                        answer: text.clone(),
                        steps: step,
                        tool_calls,
                        code_runs,
                        hit_step_limit: false,
                        step_timings,
                    });
                }

                if !history.is_empty() {
                    emit(ProgressUpdate::Thinking);
                }
                let answer = format_answer(
                    task,
                    memories,
                    &history,
                    backend,
                    conversation,
                    registry.as_ref(),
                    formatting_hint,
                )
                .await?;
                let step_dur = step_start.elapsed();
                step_timings.push(StepTiming {
                    step,
                    action: "done".to_string(),
                    duration: step_dur,
                });
                log.emit(Event::StepCompleted {
                    task_id: task_id.to_string(),
                    step,
                    action_type: "done".to_string(),
                    duration_ms: step_dur.as_millis() as u64,
                    success: true,
                })
                .await;
                let (tool_calls, code_runs) = loop_stats(&history);
                return Ok(LoopResult {
                    answer,
                    steps: step,
                    tool_calls,
                    code_runs,
                    hit_step_limit: false,
                    step_timings,
                });
            }
        }

        // Execute all actions in parallel using JoinSet
        let mut join_set = tokio::task::JoinSet::new();
        let mut sync_results: Vec<StepResult> = Vec::new();

        for action in &actions {
            match action {
                Action::CallTool { tool, params } => {
                    let fail_count = tool_fail_counts.get(tool.as_str()).copied().unwrap_or(0);
                    if fail_count >= 2 {
                        if log.is_verbose() {
                            eprintln!(
                                "[agent] step {step}: BLOCKED tool \"{tool}\" — failed {fail_count} times already"
                            );
                        }
                        sync_results.push(StepResult {
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

                    emit(ProgressUpdate::ToolCallStart);

                    let upper_params: HashMap<String, String> = params
                        .iter()
                        .map(|(k, v)| (k.to_uppercase(), v.clone()))
                        .collect();

                    let tool_name = tool.clone();
                    let action_clone = action.clone();
                    let registry = registry.clone();
                    let tool_ctx_client = tool_ctx.client.clone();
                    let tool_ctx_secrets = tool_ctx.secrets.clone();
                    let tool_ctx_task = tool_ctx.task_description.clone();
                    let tool_ctx_schedule = tool_ctx.schedule_store.clone();
                    let tool_ctx_memory = tool_ctx.memory_store.clone();
                    let tool_ctx_frontend = tool_ctx.frontend_context.clone();

                    let ctx = ToolContext {
                        client: tool_ctx_client,
                        secrets: tool_ctx_secrets,
                        task_description: tool_ctx_task,
                        schedule_store: tool_ctx_schedule,
                        memory_store: tool_ctx_memory,
                        frontend_context: tool_ctx_frontend,
                    };

                    join_set.spawn(async move {
                        let (output, success) =
                            match registry.execute_tool(&tool_name, &upper_params, &ctx).await {
                                Ok(value) => {
                                    let has_error = value.get("error").is_some();
                                    (value.to_string(), !has_error)
                                }
                                Err(e) => (
                                    format!("{{\"error\": \"tool execution failed: {e}\"}}"),
                                    false,
                                ),
                            };
                        (action_clone, output, success)
                    });
                }

                Action::RunCode { name, code } => {
                    log.emit(Event::AgentAction {
                        task_id: task_id.to_string(),
                        step,
                        action_type: "run_code".to_string(),
                        detail: name.clone(),
                    })
                    .await;

                    emit(ProgressUpdate::CodeRunStart);

                    let block_name = name.clone();
                    let code = code.clone();
                    let task_str = task.to_string();
                    let client_clone = client.clone();
                    let toolbox_dir = Some(registry.toolbox_path().to_path_buf());
                    let secrets_clone = secrets.cloned();
                    let builtins = registry.builtins_arc();
                    let action_clone = action.clone();

                    join_set.spawn(async move {
                        let provider = crate::context::LuaProvider::new(&block_name, &code);
                        let (output, success) = match provider
                            .execute_with_params(
                                &task_str,
                                client_clone,
                                &HashMap::new(),
                                toolbox_dir,
                                secrets_clone.as_ref(),
                                builtins,
                            )
                            .await
                        {
                            Ok(value) => (value.to_string(), true),
                            Err(e) => {
                                (format!("{{\"error\": \"inline code failed: {e}\"}}"), false)
                            }
                        };
                        (action_clone, output, success)
                    });
                }

                Action::SaveTool {
                    name,
                    description,
                    block,
                } => {
                    log.emit(Event::AgentAction {
                        task_id: task_id.to_string(),
                        step,
                        action_type: "save_tool".to_string(),
                        detail: name.clone(),
                    })
                    .await;

                    let code = block
                        .as_ref()
                        .and_then(|b| successful_blocks.get(b))
                        .or(last_successful_code.as_ref());

                    let output = if let Some(code) = code {
                        let meta = crate::toolbox::ToolMeta {
                            name: name.clone(),
                            description: description.clone(),
                            provides: vec![name.clone()],
                            validated: false,
                        };
                        match registry.toolbox().save_tool(&meta, code) {
                            Ok(()) => {
                                emit(ProgressUpdate::ToolCreated);
                                format!(
                                    "Tool \"{name}\" saved. You can now call it with call_tool."
                                )
                            }
                            Err(e) => format!("Failed to save tool: {e}"),
                        }
                    } else {
                        block
                            .as_deref()
                            .map(|b| format!("No successful block named \"{b}\" found."))
                            .unwrap_or_else(|| {
                                "No successful inline code to save. Run inline code first."
                                    .to_string()
                            })
                    };

                    let step_success =
                        output.starts_with("Tool \"") && output.ends_with("call_tool.");
                    sync_results.push(StepResult {
                        step,
                        action: action.clone(),
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
                            emit(ProgressUpdate::ToolRemoved);
                            format!("Tool \"{name}\" has been removed.")
                        }
                        Err(e) => format!("Failed to remove tool: {e}"),
                    };

                    let step_success = !output.starts_with("Failed");
                    sync_results.push(StepResult {
                        step,
                        action: action.clone(),
                        output,
                        success: step_success,
                        finding: None,
                    });
                }

                Action::LoadSkill { name } => {
                    // Accept both "check-calendar" and "check-calendar.md"
                    let normalized = if name.ends_with(".md") {
                        name.clone()
                    } else {
                        format!("{name}.md")
                    };

                    log.emit(Event::AgentAction {
                        task_id: task_id.to_string(),
                        step,
                        action_type: "load_skill".to_string(),
                        detail: normalized.clone(),
                    })
                    .await;

                    let (output, success) = match skill_store.load(&normalized) {
                        Ok(content) => (content, true),
                        Err(_) => (
                            format!(
                                "Skill \"{normalized}\" not found. Check the catalog for available skill names."
                            ),
                            false,
                        ),
                    };

                    sync_results.push(StepResult {
                        step,
                        action: action.clone(),
                        output,
                        success,
                        finding: Some(format!("Loaded skill: {normalized}")),
                    });
                }

                Action::Progress { text } => {
                    log.emit(Event::AgentAction {
                        task_id: task_id.to_string(),
                        step,
                        action_type: "progress".to_string(),
                        detail: truncate_preview(text, 80),
                    })
                    .await;

                    emit(ProgressUpdate::Notification(text.clone()));

                    sync_results.push(StepResult {
                        step,
                        action: action.clone(),
                        output: "Progress update sent.".to_string(),
                        success: true,
                        finding: None,
                    });
                }

                Action::Done { .. } | Action::UserMessage { .. } => {}
            }
        }

        // Collect parallel results
        while let Some(result) = join_set.join_next().await {
            match result {
                Ok((action, output, success)) => {
                    // Track tool fail counts
                    if let Action::CallTool { ref tool, .. } = action {
                        if !success {
                            *tool_fail_counts.entry(tool.clone()).or_insert(0) += 1;
                        }
                        emit(ProgressUpdate::ToolCallEnd);
                        log.emit(Event::AgentToolResult {
                            task_id: task_id.to_string(),
                            step,
                            tool: tool.clone(),
                            success,
                            output_len: output.len(),
                        })
                        .await;
                    }
                    if let Action::RunCode { ref name, ref code } = action {
                        if success {
                            successful_blocks.insert(name.clone(), code.clone());
                            last_successful_code = Some(code.clone());
                        }
                        emit(ProgressUpdate::CodeRunEnd);
                        log.emit(Event::AgentToolResult {
                            task_id: task_id.to_string(),
                            step,
                            tool: name.clone(),
                            success,
                            output_len: output.len(),
                        })
                        .await;
                    }

                    let finding = if success {
                        make_finding(&output, fast_backend).await
                    } else {
                        None
                    };

                    history.push(StepResult {
                        step,
                        action,
                        output,
                        success,
                        finding,
                    });
                }
                Err(e) => {
                    eprintln!("[agent] step {step}: parallel task panicked: {e}");
                }
            }
        }

        // Add synchronous results to history
        history.extend(sync_results);

        // If done was mixed with other actions, inform the model
        if has_done && has_other {
            history.push(StepResult {
                step,
                action: Action::UserMessage {
                    text: String::new(),
                },
                output: "SYSTEM: Your done action was ignored because it was combined with other actions. The done action must be the only action in a response. Continue working or submit done on its own.".to_string(),
                success: false,
                finding: None,
            });
        }

        // Record step timing
        let step_dur = step_start.elapsed();
        let action_types: Vec<&str> = actions.iter().map(|a| a.type_str()).collect();
        let action_label = if action_types.len() == 1 {
            action_types[0].to_string()
        } else {
            format!("parallel({})", action_types.join("+"))
        };
        let step_success = history.iter().filter(|s| s.step == step).all(|s| s.success);
        step_timings.push(StepTiming {
            step,
            action: action_label.clone(),
            duration: step_dur,
        });
        log.emit(Event::StepCompleted {
            task_id: task_id.to_string(),
            step,
            action_type: action_label,
            duration_ms: step_dur.as_millis() as u64,
            success: step_success,
        })
        .await;
    }

    // Max steps reached — force an answer with what we have
    let forced_start = Instant::now();
    history.push(StepResult {
        step: MAX_AGENT_STEPS + 1,
        action: Action::UserMessage {
            text: String::new(),
        },
        output: "SYSTEM: You have run out of steps. Summarize what you accomplished and what remains unfinished so the user knows where things stand.".to_string(),
        success: false,
        finding: None,
    });
    let answer = format_answer(
        task,
        memories,
        &history,
        backend,
        conversation,
        registry.as_ref(),
        formatting_hint,
    )
    .await?;
    let forced_dur = forced_start.elapsed();
    step_timings.push(StepTiming {
        step: MAX_AGENT_STEPS + 1,
        action: "forced_done".to_string(),
        duration: forced_dur,
    });
    log.emit(Event::StepCompleted {
        task_id: task_id.to_string(),
        step: MAX_AGENT_STEPS + 1,
        action_type: "forced_done".to_string(),
        duration_ms: forced_dur.as_millis() as u64,
        success: true,
    })
    .await;
    let (tool_calls, code_runs) = loop_stats(&history);
    Ok(LoopResult {
        answer,
        steps: MAX_AGENT_STEPS,
        tool_calls,
        code_runs,
        hit_step_limit: true,
        step_timings,
    })
}

async fn format_answer(
    task: &str,
    memories: &[Memory],
    history: &[StepResult],
    backend: &dyn ModelBackend,
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
        return backend.complete(context).await;
    }

    let data = history
        .iter()
        .filter_map(|s| match &s.action {
            Action::CallTool { tool, .. } => Some(format!("[{tool}]: {}", s.output)),
            Action::RunCode { .. } => Some(format!("[inline]: {}", s.output)),
            Action::LoadSkill { name, .. } => Some(format!("[skill:{name}]: {}", s.output)),
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

    backend.complete(context).await
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
    fn parse_done_action() {
        let input = r#"{"action": "done", "text": "The answer is 42."}"#;
        match parse_action(input) {
            Action::Done { text, fallback } => {
                assert_eq!(text, "The answer is 42.");
                assert!(!fallback, "explicit done should not be fallback");
            }
            other => panic!("expected Done, got {other:?}"),
        }
    }

    #[test]
    fn parse_with_surrounding_text() {
        let input = r#"Here is my action: {"action": "done", "text": "done"} hope that helps"#;
        match parse_action(input) {
            Action::Done { text, fallback } => {
                assert_eq!(text, "done");
                assert!(!fallback, "explicit done should not be fallback");
            }
            other => panic!("expected Done, got {other:?}"),
        }
    }

    #[test]
    fn parse_malformed_defaults_to_done() {
        let input = "I don't know what to do";
        match parse_action(input) {
            Action::Done { text, fallback } => {
                assert_eq!(text, "I don't know what to do");
                assert!(fallback, "malformed input should be fallback");
            }
            other => panic!("expected Done, got {other:?}"),
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
    fn truncate_preview_handles_non_ascii() {
        assert_eq!(truncate_preview("åäö🙂abcd", 4), "åäö🙂...");
        assert_eq!(truncate_preview("åäö🙂", 4), "åäö🙂");
    }

    #[test]
    fn truncate_str_handles_non_ascii() {
        assert_eq!(truncate_str("åäö🙂abcd", 4), "åäö🙂... (truncated)");
    }

    #[test]
    fn parse_inline_lua_block() {
        let input = "Let me fetch that:\n```lua\nlocal r = http_get(\"https://example.com\")\nreturn r\n```";
        match parse_action(input) {
            Action::RunCode { code, .. } => {
                assert!(code.contains("http_get"));
                assert!(code.contains("return r"));
            }
            other => panic!("expected RunCode, got {other:?}"),
        }
    }

    #[test]
    fn parse_lua_block_preferred_over_json() {
        // If response has both a lua block and JSON, lua wins (checked first)
        let input = "```lua\nreturn {}\n```\n{\"action\": \"done\", \"text\": \"done\"}";
        assert!(matches!(parse_action(input), Action::RunCode { .. }));
    }

    #[test]
    fn parse_empty_lua_block_falls_through() {
        let input = "```lua\n\n```\n{\"action\": \"done\", \"text\": \"done\"}";
        assert!(matches!(parse_action(input), Action::Done { .. }));
    }

    #[test]
    fn looks_incomplete_trailing_colon() {
        assert!(looks_incomplete("Sure, here are the results:"));
    }

    #[test]
    fn looks_incomplete_trailing_ellipsis() {
        assert!(looks_incomplete("Let me check that for you..."));
    }

    #[test]
    fn looks_incomplete_trailing_dash() {
        assert!(looks_incomplete("I'll verify each item —"));
    }

    #[test]
    fn looks_incomplete_intent_phrase() {
        assert!(looks_incomplete("Sure, I will check this for you"));
        assert!(looks_incomplete("Let me fetch the latest data"));
        assert!(looks_incomplete("I'll look into that now"));
        assert!(looks_incomplete("I'm going to verify those results"));
    }

    #[test]
    fn looks_incomplete_false_for_real_answers() {
        assert!(!looks_incomplete("The weather in Tokyo is 22°C and sunny."));
        assert!(!looks_incomplete("Here are your results: done."));
        assert!(!looks_incomplete("No data found for that query."));
    }

    #[test]
    fn parse_load_skill_action() {
        let input = r#"{"action": "load_skill", "name": "check-calendar.md"}"#;
        match parse_action(input) {
            Action::LoadSkill { name } => {
                assert_eq!(name, "check-calendar.md");
            }
            other => panic!("expected LoadSkill, got {other:?}"),
        }
    }

    #[test]
    fn parse_load_skill_missing_name() {
        let input = r#"{"action": "load_skill"}"#;
        let parsed = parse_response(input);
        // Falls back to done since the action is invalid
        assert_eq!(parsed.actions.len(), 1);
        assert!(matches!(
            parsed.actions[0],
            Action::Done { fallback: true, .. }
        ));
        // But also reports the validation error
        assert_eq!(parsed.errors.len(), 1);
        assert!(parsed.errors[0].contains("load_skill"));
        assert!(parsed.errors[0].contains("name"));
    }

    #[test]
    fn parse_progress_action() {
        let input = r#"{"action": "progress", "text": "Working on it..."}"#;
        match parse_action(input) {
            Action::Progress { text } => {
                assert_eq!(text, "Working on it...");
            }
            other => panic!("expected Progress, got {other:?}"),
        }
    }

    #[test]
    fn parse_progress_without_text() {
        let input = r#"{"action": "progress"}"#;
        let parsed = parse_response(input);
        assert_eq!(parsed.actions.len(), 1);
        assert!(matches!(
            parsed.actions[0],
            Action::Done { fallback: true, .. }
        ));
        assert_eq!(parsed.errors.len(), 1);
        assert!(parsed.errors[0].contains("progress"));
        assert!(parsed.errors[0].contains("text"));
    }

    #[test]
    fn parse_progress_empty_text() {
        let input = r#"{"action": "progress", "text": ""}"#;
        let parsed = parse_response(input);
        assert_eq!(parsed.actions.len(), 1);
        assert!(matches!(
            parsed.actions[0],
            Action::Done { fallback: true, .. }
        ));
        assert_eq!(parsed.errors.len(), 1);
        assert!(parsed.errors[0].contains("progress"));
    }

    // --- Multi-action parsing tests ---

    #[test]
    fn parse_response_multiple_lua_blocks() {
        let input = "```lua\n-- name: fetch_a\nreturn {a=1}\n```\n\n```lua\n-- name: fetch_b\nreturn {b=2}\n```";
        let parsed = parse_response(input);
        assert_eq!(parsed.actions.len(), 2);
        match &parsed.actions[0] {
            Action::RunCode { name, code } => {
                assert_eq!(name, "fetch_a");
                assert!(code.contains("a=1"));
            }
            other => panic!("expected RunCode, got {other:?}"),
        }
        match &parsed.actions[1] {
            Action::RunCode { name, code } => {
                assert_eq!(name, "fetch_b");
                assert!(code.contains("b=2"));
            }
            other => panic!("expected RunCode, got {other:?}"),
        }
    }

    #[test]
    fn parse_response_unnamed_lua_blocks_get_indexed() {
        let input = "```lua\nreturn {a=1}\n```\n```lua\nreturn {b=2}\n```";
        let parsed = parse_response(input);
        assert_eq!(parsed.actions.len(), 2);
        match &parsed.actions[0] {
            Action::RunCode { name, .. } => assert_eq!(name, "inline 1"),
            other => panic!("expected RunCode, got {other:?}"),
        }
        match &parsed.actions[1] {
            Action::RunCode { name, .. } => assert_eq!(name, "inline 2"),
            other => panic!("expected RunCode, got {other:?}"),
        }
    }

    #[test]
    fn parse_response_mixed_lua_and_json() {
        let input = "```lua\n-- name: fetch\nreturn {}\n```\n{\"action\": \"call_tool\", \"tool\": \"weather\", \"params\": {}}";
        let parsed = parse_response(input);
        assert_eq!(parsed.actions.len(), 2);
        assert!(matches!(parsed.actions[0], Action::RunCode { .. }));
        assert!(matches!(parsed.actions[1], Action::CallTool { .. }));
    }

    #[test]
    fn parse_response_json_array() {
        let input = r#"[{"action": "call_tool", "tool": "a", "params": {}}, {"action": "call_tool", "tool": "b", "params": {}}]"#;
        let parsed = parse_response(input);
        assert_eq!(parsed.actions.len(), 2);
        match &parsed.actions[0] {
            Action::CallTool { tool, .. } => assert_eq!(tool, "a"),
            other => panic!("expected CallTool, got {other:?}"),
        }
        match &parsed.actions[1] {
            Action::CallTool { tool, .. } => assert_eq!(tool, "b"),
            other => panic!("expected CallTool, got {other:?}"),
        }
    }

    #[test]
    fn parse_response_done_with_other_actions_keeps_all() {
        let input = "```lua\nreturn {}\n```\n{\"action\": \"done\", \"text\": \"all done\"}";
        let parsed = parse_response(input);
        // Both are parsed — the loop handles done exclusivity, not the parser
        assert_eq!(parsed.actions.len(), 2);
        assert!(matches!(parsed.actions[0], Action::RunCode { .. }));
        assert!(matches!(parsed.actions[1], Action::Done { .. }));
    }

    #[test]
    fn parse_response_save_tool_with_block_reference() {
        let input = r#"{"action": "save_tool", "name": "my_tool", "description": "desc", "block": "fetch_weather"}"#;
        let parsed = parse_response(input);
        assert_eq!(parsed.actions.len(), 1);
        match &parsed.actions[0] {
            Action::SaveTool {
                name,
                description,
                block,
            } => {
                assert_eq!(name, "my_tool");
                assert_eq!(description, "desc");
                assert_eq!(block.as_deref(), Some("fetch_weather"));
            }
            other => panic!("expected SaveTool, got {other:?}"),
        }
    }

    #[test]
    fn parse_response_fallback_to_done_on_plain_text() {
        let input = "I don't know what to do";
        let parsed = parse_response(input);
        assert_eq!(parsed.actions.len(), 1);
        match &parsed.actions[0] {
            Action::Done { text, fallback } => {
                assert_eq!(text, "I don't know what to do");
                assert!(fallback);
            }
            other => panic!("expected Done, got {other:?}"),
        }
    }

    #[test]
    fn extract_lua_blocks_named_and_unnamed() {
        let input = "```lua\n-- name: foo\nreturn 1\n```\n```lua\nreturn 2\n```";
        let blocks = extract_lua_blocks(input);
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0].0, "foo");
        assert_eq!(blocks[1].0, "inline 1");
    }

    #[test]
    fn extract_json_actions_ignores_json_inside_lua() {
        let input = "```lua\nlocal x = json_parse('{\"action\": \"done\", \"text\": \"nope\"}')\nreturn x\n```\n{\"action\": \"progress\", \"text\": \"real\"}";
        let (actions, errors) = extract_json_actions(input);
        assert_eq!(actions.len(), 1);
        assert!(errors.is_empty());
        assert!(matches!(actions[0], Action::Progress { .. }));
    }

    #[test]
    fn parse_unknown_action_with_tool_field_becomes_call_tool() {
        // Model used tool name as action instead of "call_tool"
        let input = r#"{"action": "remove_schedule", "tool": "remove_schedule", "params": {"SCHEDULE_ID": "abc-123"}}"#;
        match parse_action(input) {
            Action::CallTool { tool, params } => {
                assert_eq!(tool, "remove_schedule");
                assert_eq!(params.get("SCHEDULE_ID").unwrap(), "abc-123");
            }
            other => panic!("expected CallTool, got {other:?}"),
        }
    }

    #[test]
    fn parse_unknown_action_with_params_only_uses_action_as_tool() {
        // Model used tool name as action, no separate tool field
        let input = r#"{"action": "memory_update", "params": {"ID": "x", "FACT": "something"}}"#;
        match parse_action(input) {
            Action::CallTool { tool, params } => {
                assert_eq!(tool, "memory_update");
                assert_eq!(params.get("FACT").unwrap(), "something");
            }
            other => panic!("expected CallTool, got {other:?}"),
        }
    }

    #[test]
    fn parse_unknown_action_without_tool_or_params_reports_error() {
        // Truly unknown action with no useful fields — reports helpful error
        let input = r#"{"action": "something_random"}"#;
        let parsed = parse_response(input);
        assert_eq!(parsed.actions.len(), 1);
        assert!(matches!(
            parsed.actions[0],
            Action::Done { fallback: true, .. }
        ));
        assert_eq!(parsed.errors.len(), 1);
        assert!(parsed.errors[0].contains("Unknown action"));
        assert!(parsed.errors[0].contains("something_random"));
        assert!(parsed.errors[0].contains("call_tool"));
    }

    #[test]
    fn parse_double_brace_json_recovery() {
        // Model generated double braces (legacy behavior)
        let input =
            r#"{{"action": "call_tool", "tool": "weather", "params": {{"CITY": "Stockholm"}}}}"#;
        match parse_action(input) {
            Action::CallTool { tool, params } => {
                assert_eq!(tool, "weather");
                assert_eq!(params.get("CITY").unwrap(), "Stockholm");
            }
            other => panic!("expected CallTool, got {other:?}"),
        }
    }

    #[test]
    fn parse_call_tool_missing_tool_field_reports_error() {
        let input = r#"{"action": "call_tool", "params": {"X": "1"}}"#;
        let parsed = parse_response(input);
        assert_eq!(parsed.actions.len(), 1);
        assert!(matches!(
            parsed.actions[0],
            Action::Done { fallback: true, .. }
        ));
        assert_eq!(parsed.errors.len(), 1);
        assert!(parsed.errors[0].contains("call_tool"));
        assert!(parsed.errors[0].contains("tool"));
    }

    #[test]
    fn parse_save_tool_missing_description_reports_error() {
        let input = r#"{"action": "save_tool", "name": "my_tool"}"#;
        let parsed = parse_response(input);
        assert_eq!(parsed.actions.len(), 1);
        assert!(matches!(
            parsed.actions[0],
            Action::Done { fallback: true, .. }
        ));
        assert_eq!(parsed.errors.len(), 1);
        assert!(parsed.errors[0].contains("save_tool"));
        assert!(parsed.errors[0].contains("description"));
    }

    #[test]
    fn parse_done_missing_text_reports_error() {
        let input = r#"{"action": "done"}"#;
        let parsed = parse_response(input);
        assert_eq!(parsed.actions.len(), 1);
        assert!(matches!(
            parsed.actions[0],
            Action::Done { fallback: true, .. }
        ));
        assert_eq!(parsed.errors.len(), 1);
        assert!(parsed.errors[0].contains("done"));
        assert!(parsed.errors[0].contains("text"));
    }

    #[test]
    fn valid_actions_produce_no_errors() {
        let input =
            r#"{"action": "call_tool", "tool": "weather", "params": {"CITY": "Stockholm"}}"#;
        let parsed = parse_response(input);
        assert_eq!(parsed.actions.len(), 1);
        assert!(
            parsed.errors.is_empty(),
            "valid action should produce no errors"
        );
    }

    #[test]
    fn parse_malformed_json_with_action_key_reports_error() {
        let input = r#"{"action": "call_tool", "tool": }"#;
        let parsed = parse_response(input);
        // Malformed JSON with "action" key should report helpful error
        assert!(
            !parsed.errors.is_empty(),
            "malformed JSON should produce error"
        );
        assert!(parsed.errors[0].contains("Malformed JSON"));
    }
}
