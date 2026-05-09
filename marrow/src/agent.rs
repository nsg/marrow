use std::collections::HashMap;
use std::error::Error;
use std::hash::{Hash, Hasher};
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

/// The outcome of an agent loop: either a user-facing answer or a silent dismissal.
#[derive(Debug, Clone)]
pub enum Outcome {
    /// The agent produced a response for the user.
    Answer(String),
    /// The agent completed its work with nothing to report (e.g. a monitoring
    /// task where the condition was not met).
    Dismissed,
}

/// Result of a single agent loop run, returned to the runtime layer.
#[derive(Debug, Clone)]
pub struct LoopResult {
    pub outcome: Outcome,
    pub steps: u32,
    pub lua_runs: u32,
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
    CodeRunStart, // ⚡
    CodeRunEnd,   // ⚡
    Thinking,     // 💭

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

/// Maximum inline Lua blocks per model response. Excess blocks are dropped.
const MAX_LUA_BLOCKS_PER_STEP: usize = 5;

/// A compacted summary of all history up to a certain step. When present,
/// the prompt shows the checkpoint text + only the steps that followed it.
#[derive(Debug, Clone)]
pub struct Checkpoint {
    /// Compacted summary produced by the fast model.
    pub text: String,
    /// This checkpoint covers all steps up to and including this step number.
    pub up_to_step: u32,
}

/// Configuration for a single agent loop invocation.
pub struct LoopConfig<'a> {
    pub task: &'a str,
    pub task_id: &'a str,
    pub backend: &'a dyn ModelBackend,
    pub fast_backend: &'a dyn ModelBackend,
    pub registry: Arc<ToolRegistry>,
    pub client: Arc<Client>,
    pub memories: &'a [Memory],
    pub skill_store: &'a SkillStore,
    pub log: &'a EventLog,
    pub secrets: Option<&'a Secrets>,
    pub progress: Option<&'a ProgressTx>,
    pub conversation: &'a [Message],
    pub incoming: Option<&'a mut IncomingRx>,
    pub formatting_hint: Option<&'a str>,
    pub schedule_store: Option<Arc<crate::schedule::ScheduleStore>>,
    pub memory_store: Option<Arc<crate::memory::MemoryStore>>,
    pub frontend_context: Option<FrontendContext>,
    pub frontend: &'a str,
    pub max_steps: Option<u32>,
    pub prior_context: Option<String>,
}

/// Static system prompt — identical across all steps and tasks.
/// Placed in the system message so API providers can cache it.
const AGENT_SYSTEM_PROMPT: &str = r#"You are an agent that completes tasks step by step.

CRITICAL: You have NO shell access. No curl, no bash, no command line. You can ONLY interact through Lua code blocks and JSON control actions.

Each response can contain ```lua code blocks and/or JSON control actions. They all execute in parallel.

## Lua code — calling tools and writing logic

Write ```lua code blocks to call tools, make HTTP requests, and process data. All tools listed under "Available Lua functions" are callable directly as Lua functions.

Available sandbox functions:
- http_request({ method, url, body?, headers? }) / http_get(url) / http_post(url, body)
- json_parse(string) / json_encode(table)
- xml_parse(string) / xml_encode(table)
- secret(name) — retrieve API keys/passwords. ALWAYS use secret("name") and concatenate into your URL or header. NEVER write "secret:name" as a literal string — it will NOT be resolved.
- log(message)
- Standard Lua: string.*, table.*, math.*, tonumber, tostring, type, pairs, ipairs, pcall

UNAVAILABLE (sandboxed out): require, os, io, debug, dofile, loadfile, package, base64.
There is no base64 library — for HTTP Basic auth, embed credentials in the URL (https://user:pass@host).

Return a table with the results. Example:
```lua
local events = caldav_calendar({CALENDAR_URL = "https://cal.example.com/cal", FROM = "2025-01-01", TO = "2025-01-31"})
return events
```

### Multiple Lua blocks & naming

You can write multiple ```lua blocks in one response. They run in parallel in separate sandboxes — they cannot access each other's variables or results. Name blocks with a `-- name: xyz` comment on the first line:

```lua
-- name: fetch_weather
return rss_feed({URL = "https://weather.example.com/feed"})
```

```lua
-- name: get_state
return state_get({KEY = "last_check"})
```

Results come back labeled by name (e.g. [fetch_weather]: ..., [get_state]: ...). If you omit the name, blocks are labeled by index ([inline 1], [inline 2]).

## JSON control actions

JSON actions handle control flow only — NOT tool calls. A single action can be a plain object (no array needed).

**save_tool** — save a previously successful ```lua block as a reusable tool (it becomes a Lua function prefixed with tool_, e.g. saving "weather" creates tool_weather()):
{"action": "save_tool", "name": "generic_tool_name", "description": "one line description", "block": "fetch_weather"}

**remove_tool** — remove a broken tool from the toolbox:
{"action": "remove_tool", "name": "tool_name"}

**progress** — send the user a progress update while you continue working (does NOT end the task):
{"action": "progress", "text": "status message"}

**load_skill** — load full procedural steps for a skill (see "Available skills" catalog):
{"action": "load_skill", "name": "skill-filename.md"}

**done** — give your final answer (this text is shown directly to the user — make it complete and well-formatted):
{"action": "done", "text": "your complete answer to the user"}
IMPORTANT: done MUST be the only action in a response. If you combine done with other actions, the done will be IGNORED and all other actions will execute.

**dismiss** — exit silently with NO message to the user:
{"action": "dismiss"}
Use dismiss when a task completes with nothing to report (e.g. a monitoring check found no issues). Do NOT use dismiss to avoid answering a direct question. Like done, dismiss must be the only action in a response.

## Mixing Lua and control actions

You can freely combine ```lua blocks with JSON actions (save_tool, remove_tool, progress, load_skill) in a single response. Everything executes in parallel. The only exceptions are done and dismiss, which must appear alone.

Example — fetch data and save a previous block in the same response:
```lua
-- name: fetch_prices
local resp = http_get("https://api.example.com/prices")
return json_parse(resp.body)
```
{"action": "save_tool", "name": "weather_lookup", "description": "Fetches weather data", "block": "fetch_weather"}

## Rules

- CRITICAL: Before writing ANY http_request/http_get/http_post code, check the "Available Lua functions" list. If a built-in function already does what you need (rss_feed for RSS, http_fetch for HTTP, caldav_calendar for CalDAV, etc.), call it directly — it handles auth, parsing, and error cases that raw HTTP will not. Only use http_get/http_request for APIs that have NO matching function.
- IMPORTANT: If a relevant skill exists (especially ones marked "suggested"), load it FIRST. Skills contain the correct URLs, credentials, parameter formats, and known workarounds. Guessing these values wastes steps.
- After inline Lua succeeds: if the code is generally useful, save it with save_tool. You can save a block in the same response as new work.
- CRITICAL: When a step already returned the data you need, do NOT rewrite the code. Use save_tool referencing the successful block.
- If code fails, read the error carefully. Do NOT repeat the same approach — fix the specific issue.
- NEVER retry something that already failed with the same error.
- If something worked in a previous step, reuse that exact approach.
- Do NOT answer prematurely. If data collection failed, try a different approach before giving up.
- If a follow-up question asks about different data, you MUST fetch new data.
- If a saved tool fails repeatedly, use remove_tool to delete it.
- When saving tools, prefer generic names and use PARAMS for inputs (e.g. PARAMS["LOCATION"]).
- Use known facts to fill in real parameter values.
- The done action text should be a natural language response. Include all relevant details from your findings.
- CRITICAL: Every response MUST contain at least one action (JSON or ```lua block). Plain text without an action will be treated as your final answer.
- When you can do independent work in parallel, use multiple ```lua blocks in one response.
- If the user sends a follow-up message during your work, you'll see it in the history. Adjust your plan accordingly."#;

/// Dynamic user prompt — ordered for prompt cache efficiency: stable sections
/// first (skills, memories, secrets, conversation, context, task), then
/// volatile sections (tools, history, datetime) so that cache-breaking changes
/// only invalidate the tail.
const AGENT_USER_PROMPT_TEMPLATE: &str = r#"{memories}{conversation}{execution_context}{prior_context}Task: {task}

Available Lua functions (load lua.md skill for full docs):
{tools}

{history}Current date/time: {datetime}
Step {step} of {max_steps}. Your action:"#;

#[derive(Debug, Clone)]
pub enum Action {
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
    /// Silent exit — the agent completed its work with nothing to report.
    Dismiss,
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
            Action::RunCode { .. } => "run_code",
            Action::SaveTool { .. } => "save_tool",
            Action::RemoveTool { .. } => "remove_tool",
            Action::LoadSkill { .. } => "load_skill",
            Action::Progress { .. } => "progress",
            Action::Done { .. } => "done",
            Action::Dismiss => "dismiss",
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

// ---------------------------------------------------------------------------
// Transition — unified guardrail state between loop iterations
// ---------------------------------------------------------------------------

/// A guardrail transition determined at the end of one iteration, whose system
/// message is injected at the start of the next iteration. This unifies all
/// loop-control mechanisms (repeat detection, fail blocking, nudging, budget
/// warnings) into a single enum so the model sees one clear signal per issue
/// and the history stays clean.
#[derive(Debug, Clone)]
enum Transition {
    /// No guardrail fired — normal continuation.
    None,
    /// A tool was called with identical params/output to a previous call.
    /// On first repeat: warn but still execute. On second repeat: block.
    ToolRepeatWarned { tool: String, original_step: u32 },
    /// A tool has been called 3+ times with identical params/output. Blocked.
    ToolRepeatBlocked { tool: String, original_step: u32 },
    /// A tool has failed too many times and is now blocked.
    ToolFailBlocked { tool: String, fail_count: u32 },
    /// The step budget is running low.
    StepBudgetWarning { remaining: u32 },
}

impl Transition {
    /// Generate the system message to inject into history for this transition.
    fn system_message(&self) -> Option<String> {
        match self {
            Transition::None => Option::None,
            Transition::ToolRepeatWarned {
                tool,
                original_step,
            } => Some(format!(
                "SYSTEM: Tool \"{tool}\" returned the same data as step {original_step}. \
                 The result was included this time, but calling it again will be BLOCKED. \
                 Use the data you have to proceed or finish the task."
            )),
            Transition::ToolRepeatBlocked {
                tool,
                original_step,
            } => Some(format!(
                "SYSTEM: Tool \"{tool}\" was already called with the same result at \
                 step {original_step}. This is the third identical call — it was blocked and \
                 NOT executed. Use the result from previous steps to proceed or finish the task. \
                 Do NOT rewrite the same call in Lua as a workaround."
            )),
            Transition::ToolFailBlocked { tool, fail_count } => Some(format!(
                "SYSTEM: Tool \"{tool}\" has failed {fail_count} times and is now blocked. \
                 You MUST use a different tool or create a new one."
            )),
            Transition::StepBudgetWarning { remaining } => Some(format!(
                "SYSTEM: You have {remaining} steps remaining out of {MAX_AGENT_STEPS}. \
                 Start wrapping up — prioritize answering the user's question with what \
                 you have. If you need more data, make it your very next action."
            )),
        }
    }

    fn type_str(&self) -> &'static str {
        match self {
            Transition::None => "none",
            Transition::ToolRepeatWarned { .. } => "tool_repeat_warned",
            Transition::ToolRepeatBlocked { .. } => "tool_repeat_blocked",
            Transition::ToolFailBlocked { .. } => "tool_fail_blocked",
            Transition::StepBudgetWarning { .. } => "step_budget_warning",
        }
    }

    fn detail_str(&self) -> String {
        match self {
            Transition::None => String::new(),
            Transition::ToolRepeatWarned {
                tool,
                original_step,
            } => format!("{tool} (repeat of step {original_step}, warned)"),
            Transition::ToolRepeatBlocked {
                tool,
                original_step,
            } => format!("{tool} (duplicate of step {original_step})"),
            Transition::ToolFailBlocked { tool, fail_count } => {
                format!("{tool} ({fail_count} failures)")
            }
            Transition::StepBudgetWarning { remaining } => format!("{remaining} steps left"),
        }
    }
}

/// Compute a u64 hash of a string for output deduplication.
fn hash_output(s: &str) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    s.hash(&mut hasher);
    hasher.finish()
}

/// Number of steps remaining before we warn the model about the budget.
const BUDGET_WARNING_THRESHOLD: u32 = 5;

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
    prior_context: Option<&str>,
    step: u32,
    max_steps: u32,
) -> Vec<Message> {
    let tools_section = if tools_section.is_empty() {
        "(none available — create one if needed)"
    } else {
        tools_section
    };

    let skills_section = if skill_catalog.is_empty() {
        String::new()
    } else {
        let task_lower = task.to_lowercase();
        let lines: Vec<String> = skill_catalog
            .iter()
            .map(|(name, desc)| {
                // Lightweight keyword match: check if the skill name or description
                // shares words with the task. Tag matches as (suggested) so the
                // model sees a hint without an extra model call.
                let skill_lower = format!("{} {}", name.replace(".md", "").replace('-', " "), desc)
                    .to_lowercase();
                let suggested = skill_lower
                    .split_whitespace()
                    .any(|word| word.len() >= 4 && task_lower.contains(word));
                if suggested {
                    format!("- {name}: {desc}  ← suggested")
                } else {
                    format!("- {name}: {desc}")
                }
            })
            .collect();
        format!(
            "Available skills (load_skill to get full steps — always load a skill before using its tools):\n{}\n\n",
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
        format!("Available secrets (use secret(\"NAME\") in Lua to retrieve):\n{list}\n\n")
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

    let prior_section = match prior_context {
        Some(ctx) if !ctx.is_empty() => format!(
            "Prior completed work:\n{ctx}\n\n\
             Focus: You are working on one step of a larger task. Complete only the objective below.\n\n"
        ),
        _ => String::new(),
    };

    let user_content = AGENT_USER_PROMPT_TEMPLATE
        .replace("{tools}", tools_section)
        .replace(
            "{memories}",
            &format!("{skills_section}{memories_section}{secrets_section}"),
        )
        .replace("{conversation}", &conversation_section)
        .replace("{execution_context}", &execution_context)
        .replace("{prior_context}", &prior_section)
        .replace("{datetime}", &datetime)
        .replace("{task}", task)
        .replace("{history}", &history_section)
        .replace("{step}", &step.to_string())
        .replace("{max_steps}", &max_steps.to_string());

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
        #[allow(dead_code)]
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

    match raw.action.as_str() {
        "call_tool" => {
            let tool = raw.tool.unwrap_or_default();
            Err(format!(
                "call_tool is not available. Call tools directly as Lua functions inside a ```lua block: \
                 ```lua\nreturn {tool}({{KEY = \"value\"}})\n```"
            ))
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
        "dismiss" => Ok(Action::Dismiss),
        unknown => Err(format!(
            "Unknown action \"{unknown}\". Available actions: save_tool, \
                 remove_tool, progress, load_skill, done, dismiss. To call a tool, \
                 use a ```lua block: ```lua\nreturn {unknown}({{}})\n```"
        )),
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
    let (json_actions, mut errors) = extract_json_actions(trimmed);

    let dropped_lua = lua_blocks.len().saturating_sub(MAX_LUA_BLOCKS_PER_STEP);
    let mut actions: Vec<Action> = lua_blocks
        .into_iter()
        .take(MAX_LUA_BLOCKS_PER_STEP)
        .map(|(name, code)| Action::RunCode { name, code })
        .collect();
    if dropped_lua > 0 {
        errors.push(format!(
            "You submitted {} Lua blocks but the limit is {MAX_LUA_BLOCKS_PER_STEP} per step. \
             {dropped_lua} block(s) were dropped. Batch your work into fewer, more focused blocks.",
            actions.len() + dropped_lua
        ));
    }
    actions.extend(json_actions);

    // If nothing was parsed, treat the whole response as a fallback done.
    // Discard content that is just a bare JSON object/array with no action
    // key — the model occasionally emits "{}" or similar non-action JSON
    // which should not be forwarded as an answer.
    if actions.is_empty() {
        let fallback_text = if is_bare_json(trimmed) {
            String::new()
        } else {
            trimmed.to_string()
        };
        actions.push(Action::Done {
            text: fallback_text,
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

/// Returns true if the text is a bare JSON value with no `"action"` key —
/// e.g. `{}`, `{"key": 1}`, `[]`. These are not valid answers.
fn is_bare_json(text: &str) -> bool {
    let t = text.trim();
    if t.is_empty() {
        return false;
    }
    let first = t.as_bytes()[0];
    if first != b'{' && first != b'[' {
        return false;
    }
    // Must parse as valid JSON and must NOT contain an action key (those
    // would have been caught by the action parser already).
    serde_json::from_str::<serde_json::Value>(t).is_ok()
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
        Action::Dismiss => "Dismissed (nothing to report)".to_string(),
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

fn loop_stats(history: &[StepResult]) -> u32 {
    history
        .iter()
        .filter(|s| matches!(s.action, Action::RunCode { .. }))
        .count() as u32
}

pub async fn run_loop(config: LoopConfig<'_>) -> Result<LoopResult, Box<dyn Error + Send + Sync>> {
    let LoopConfig {
        task,
        task_id,
        backend,
        fast_backend,
        registry,
        client,
        memories,
        skill_store,
        log,
        secrets,
        progress,
        conversation,
        mut incoming,
        formatting_hint,
        schedule_store,
        memory_store,
        frontend_context,
        frontend,
        max_steps,
        prior_context,
    } = config;
    let max_steps = max_steps.unwrap_or(MAX_AGENT_STEPS);
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
    let mut last_successful_code: Option<String> = None; // fallback for save_tool without block ref
    let mut successful_blocks: HashMap<String, String> = HashMap::new(); // name -> code
    let mut incomplete_nudges: u32 = 0;
    const MAX_INCOMPLETE_NUDGES: u32 = 2;

    // Transition state: determined at end of iteration, injected at start of next.
    let mut pending_transition = Transition::None;
    // Track (source, output_hash) -> (first_step, hit_count) for post-execution output dedup.
    let mut tool_output_seen: HashMap<(String, u64), (u32, u32)> = HashMap::new();
    // Track Lua error hash -> count for fail-blocking across differently-named blocks.
    let mut lua_error_counts: HashMap<u64, u32> = HashMap::new();
    // Track loaded skills to avoid re-loading the same skill multiple times.
    let mut loaded_skills: std::collections::HashSet<String> = std::collections::HashSet::new();

    for step in 1..=max_steps {
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

        // Inject any pending transition message from the previous iteration
        if let Some(msg) = pending_transition.system_message() {
            history.push(StepResult {
                step,
                action: Action::UserMessage {
                    text: String::new(),
                },
                output: msg,
                success: false,
                finding: None,
            });
            log.emit(Event::AgentTransition {
                task_id: task_id.to_string(),
                step,
                transition_type: pending_transition.type_str().to_string(),
                detail: pending_transition.detail_str(),
            })
            .await;
        }
        pending_transition = Transition::None;

        let available_tools = registry.list_all();
        let tools_section = available_tools
            .iter()
            .map(|t| t.usage_line())
            .collect::<Vec<_>>()
            .join("\n");

        if log.is_verbose() {
            eprintln!("[agent] step {step}: tools shown to model:\n{tools_section}");
        }

        let skill_catalog = skill_store.catalog_with_lua().unwrap_or_default();
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
            prior_context.as_deref(),
            step,
            max_steps,
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
                prior_context.as_deref(),
                step,
                max_steps,
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
                    Action::Dismiss => {
                        eprintln!("[agent] step {step}:   dismiss — nothing to report");
                    }
                    Action::UserMessage { .. } => {}
                }
            }
        }

        // Check for done/dismiss exclusivity: if done/dismiss is mixed with
        // other actions, drop the done/dismiss and inform the model.
        let is_terminal = |a: &Action| matches!(a, Action::Done { .. } | Action::Dismiss);
        let has_done = actions.iter().any(is_terminal);
        let has_other = actions.iter().any(|a| !is_terminal(a));
        if has_done && has_other {
            actions.retain(|a| !is_terminal(a));
            // We'll append a system message after executing the other actions
        }

        // If the only action is Dismiss, exit silently
        if actions.len() == 1 && matches!(actions[0], Action::Dismiss) {
            log.emit(Event::AgentAction {
                task_id: task_id.to_string(),
                step,
                action_type: "dismiss".to_string(),
                detail: String::new(),
                params_json: None,
                code: None,
            })
            .await;

            let step_dur = step_start.elapsed();
            step_timings.push(StepTiming {
                step,
                action: "dismiss".to_string(),
                duration: step_dur,
            });
            log.emit(Event::StepCompleted {
                task_id: task_id.to_string(),
                step,
                action_type: "dismiss".to_string(),
                duration_ms: step_dur.as_millis() as u64,
                success: true,
            })
            .await;
            let lua_runs = loop_stats(&history);
            return Ok(LoopResult {
                outcome: Outcome::Dismissed,
                steps: step,
                lua_runs,
                hit_step_limit: false,
                step_timings,
            });
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
                    detail: text.clone(),
                    params_json: None,
                    code: None,
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
                    let lua_runs = loop_stats(&history);
                    return Ok(LoopResult {
                        outcome: Outcome::Answer(text.clone()),
                        steps: step,
                        lua_runs,
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
                let lua_runs = loop_stats(&history);
                return Ok(LoopResult {
                    outcome: Outcome::Answer(answer),
                    steps: step,
                    lua_runs,
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
                Action::RunCode { name, code } => {
                    log.emit(Event::AgentAction {
                        task_id: task_id.to_string(),
                        step,
                        action_type: "run_code".to_string(),
                        detail: name.clone(),
                        params_json: None,
                        code: Some(code.clone()),
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
                    let sched_store = tool_ctx.schedule_store.clone();
                    let mem_store = tool_ctx.memory_store.clone();
                    let fe_ctx = tool_ctx.frontend_context.clone();

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
                                sched_store,
                                mem_store,
                                fe_ctx,
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
                        params_json: None,
                        code: None,
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
                                    "Tool \"{name}\" saved. You can now call it as tool_{name}() in Lua."
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
                        params_json: None,
                        code: None,
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

                    if loaded_skills.contains(&normalized) {
                        if log.is_verbose() {
                            eprintln!(
                                "[agent] step {step}: skill \"{normalized}\" already loaded, skipping"
                            );
                        }
                        sync_results.push(StepResult {
                            step,
                            action: action.clone(),
                            output: format!(
                                "Skill \"{normalized}\" is already loaded. Refer to the earlier step where it was loaded."
                            ),
                            success: true,
                            finding: None,
                        });
                        continue;
                    }

                    log.emit(Event::AgentAction {
                        task_id: task_id.to_string(),
                        step,
                        action_type: "load_skill".to_string(),
                        detail: normalized.clone(),
                        params_json: None,
                        code: None,
                    })
                    .await;

                    let (output, success) = match skill_store
                        .load_dynamic(&normalized, &available_tools)
                    {
                        Ok(content) => {
                            loaded_skills.insert(normalized.clone());
                            (content, true)
                        }
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
                        params_json: None,
                        code: None,
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

                Action::Done { .. } | Action::Dismiss | Action::UserMessage { .. } => {}
            }
        }

        // Collect parallel results
        while let Some(result) = join_set.join_next().await {
            match result {
                Ok((action, output, success)) => {
                    if let Action::RunCode { ref name, ref code } = action {
                        if success {
                            successful_blocks.insert(name.clone(), code.clone());
                            last_successful_code = Some(code.clone());

                            // Name-agnostic output dedup: use a fixed key so
                            // differently-named Lua blocks producing identical
                            // output are caught (e.g. fetch_rss vs fetch_full_rss
                            // both returning the same RSS data).
                            let out_hash = hash_output(&output);
                            let out_key = ("__lua__".to_string(), out_hash);
                            let out_entry = tool_output_seen.entry(out_key).or_insert((step, 0));
                            out_entry.1 += 1;
                            if out_entry.1 > 1 && out_entry.0 != step {
                                let original_step = out_entry.0;
                                if out_entry.1 >= 3 {
                                    if log.is_verbose() {
                                        eprintln!(
                                            "[agent] step {step}: code block \"{name}\" returned identical output to a block at step {original_step}, suppressing"
                                        );
                                    }
                                    pending_transition = Transition::ToolRepeatBlocked {
                                        tool: name.clone(),
                                        original_step,
                                    };
                                    emit(ProgressUpdate::CodeRunEnd);
                                    log.emit(Event::AgentToolResult {
                                        task_id: task_id.to_string(),
                                        step,
                                        tool: name.clone(),
                                        success,
                                        output_len: output.len(),
                                    })
                                    .await;
                                    continue; // skip adding to history
                                }
                                if log.is_verbose() {
                                    eprintln!(
                                        "[agent] step {step}: code block \"{name}\" returned identical output to step {original_step}, warning"
                                    );
                                }
                                pending_transition = Transition::ToolRepeatWarned {
                                    tool: name.clone(),
                                    original_step,
                                };
                            }
                        } else {
                            // Track Lua failures by error hash so differently-named
                            // blocks hitting the same error accumulate correctly.
                            let err_hash = hash_output(&output);
                            let count = lua_error_counts.entry(err_hash).or_insert(0);
                            *count += 1;
                            if *count >= 2 {
                                if log.is_verbose() {
                                    eprintln!(
                                        "[agent] step {step}: code block \"{name}\" hit a repeated error ({count} times), suppressing"
                                    );
                                }
                                pending_transition = Transition::ToolFailBlocked {
                                    tool: name.clone(),
                                    fail_count: *count,
                                };
                                emit(ProgressUpdate::CodeRunEnd);
                                log.emit(Event::AgentToolResult {
                                    task_id: task_id.to_string(),
                                    step,
                                    tool: name.clone(),
                                    success,
                                    output_len: output.len(),
                                })
                                .await;
                                continue; // skip adding to history
                            }
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
                output: "SYSTEM: Your done/dismiss action was ignored because it was combined with other actions. done and dismiss must be the only action in a response. Continue working or submit done/dismiss on its own.".to_string(),
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

        // Step budget warning: fire once when approaching the limit,
        // but don't overwrite a more specific transition from this iteration.
        let remaining = max_steps.saturating_sub(step);
        if remaining == BUDGET_WARNING_THRESHOLD && matches!(pending_transition, Transition::None) {
            pending_transition = Transition::StepBudgetWarning { remaining };
        }
    }

    // Max steps reached — force an answer with what we have
    let forced_start = Instant::now();
    let forced_msg = "SYSTEM: You have run out of steps. Summarize what you accomplished \
                      and what remains unfinished so the user knows where things stand."
        .to_string();
    history.push(StepResult {
        step: max_steps + 1,
        action: Action::UserMessage {
            text: String::new(),
        },
        output: forced_msg,
        success: false,
        finding: None,
    });
    log.emit(Event::AgentTransition {
        task_id: task_id.to_string(),
        step: max_steps + 1,
        transition_type: "forced_done".to_string(),
        detail: String::new(),
    })
    .await;
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
        step: max_steps + 1,
        action: "forced_done".to_string(),
        duration: forced_dur,
    });
    log.emit(Event::StepCompleted {
        task_id: task_id.to_string(),
        step: max_steps + 1,
        action_type: "forced_done".to_string(),
        duration_ms: forced_dur.as_millis() as u64,
        success: true,
    })
    .await;
    let lua_runs = loop_stats(&history);
    Ok(LoopResult {
        outcome: Outcome::Answer(answer),
        steps: max_steps,
        lua_runs,
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
            Action::RunCode { name, .. } => Some(format!("[{name}]: {}", s.output)),
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
    fn parse_call_tool_returns_error() {
        let input =
            r#"{"action": "call_tool", "tool": "weather", "params": {"LOCATION": "Tokyo"}}"#;
        let parsed = parse_response(input);
        assert!(!parsed.errors.is_empty(), "call_tool should produce error");
        assert!(parsed.errors[0].contains("call_tool is not available"));
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
    fn parse_dismiss_action() {
        let input = r#"{"action": "dismiss"}"#;
        assert!(
            matches!(parse_action(input), Action::Dismiss),
            "expected Dismiss"
        );
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
    fn bare_json_becomes_empty_fallback() {
        // Model sometimes returns "{}" or bare JSON without an action key.
        // These should become a fallback done with empty text so the agent
        // falls through to format_answer instead of echoing JSON to the user.
        for input in ["{}", r#"{"key": 1}"#, "[]", r#"[1, 2]"#] {
            let parsed = parse_response(input);
            assert_eq!(parsed.actions.len(), 1, "input: {input}");
            match &parsed.actions[0] {
                Action::Done { text, fallback } => {
                    assert!(text.is_empty(), "expected empty text for input: {input}");
                    assert!(fallback, "expected fallback=true for input: {input}");
                }
                other => panic!("expected Done for input {input}, got {other:?}"),
            }
        }
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
        let input = "```lua\n-- name: fetch\nreturn {}\n```\n{\"action\": \"progress\", \"text\": \"fetching\"}";
        let parsed = parse_response(input);
        assert_eq!(parsed.actions.len(), 2);
        assert!(matches!(parsed.actions[0], Action::RunCode { .. }));
        assert!(matches!(parsed.actions[1], Action::Progress { .. }));
    }

    #[test]
    fn parse_response_json_array() {
        let input = r#"[{"action": "progress", "text": "a"}, {"action": "progress", "text": "b"}]"#;
        let parsed = parse_response(input);
        assert_eq!(parsed.actions.len(), 2);
        assert!(matches!(parsed.actions[0], Action::Progress { .. }));
        assert!(matches!(parsed.actions[1], Action::Progress { .. }));
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
    fn parse_unknown_action_reports_error() {
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
        let input = r#"{"action": "done", "text": "hello"}"#;
        let parsed = parse_response(input);
        assert_eq!(parsed.actions.len(), 1);
        assert!(
            parsed.errors.is_empty(),
            "valid action should produce no errors"
        );
    }

    #[test]
    fn parse_malformed_json_with_action_key_reports_error() {
        let input = r#"{"action": "done", "text": }"#;
        let parsed = parse_response(input);
        // Malformed JSON with "action" key should report helpful error
        assert!(
            !parsed.errors.is_empty(),
            "malformed JSON should produce error"
        );
        assert!(parsed.errors[0].contains("Malformed JSON"));
    }
}
