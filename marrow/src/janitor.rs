use std::collections::{HashMap, HashSet};
use std::error::Error;
use std::time::Duration;

use tokio::time::sleep;

use crate::events::{Event, EventLog};
use crate::memory::{Memory, MemoryStore};
use crate::model::ModelBackend;
use crate::toolbox::{ToolMeta, Toolbox};

const MAX_FIX_ATTEMPTS: u32 = 3;

pub struct BuiltinInfo {
    pub name: String,
    pub description: String,
}

pub fn format_builtins_for_prompt(builtins: &[BuiltinInfo]) -> String {
    if builtins.is_empty() {
        return String::new();
    }
    let list = builtins
        .iter()
        .map(|b| format!("- {}: {}", b.name, b.description))
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "\nBuilt-in tools (compiled into the application, always available via run_tool()):\n\
         {list}\n\
         These are NOT Lua tools — they cannot be reviewed, modified, or deleted.\n"
    )
}

const REVIEW_PROMPT_TEMPLATE: &str = r#"You are a code reviewer for Lua scripts that run in a sandboxed environment. Review the following tool for quality and correctness.

Tool metadata:
- Name: {name}
- Description: {description}

Lua source:
```lua
{source}
```

Available host functions in the sandbox:
- http_request({{ method = string, url = string, body = string?, headers = {{ [string] = string }}? }}) -> {{ status = number, body = string }}
- http_get(url) -> {{ status = number, body = string }}  (shorthand for GET)
- http_post(url, json_body_string) -> {{ status = number, body = string }}  (shorthand for POST)
- json_parse(string) -> table
- json_encode(table) -> string
- xml_parse(string) -> table (parses XML into {{ tag, attrs?, text?, children? }} tree; namespace URIs are prefixed to tag names)
- xml_encode(table) -> string (encodes a {{ tag, attrs?, text?, children? }} tree back to XML)
- log(message) -> nil
- run_tool(name, params_table) -> table (call another tool by name, passing it a params table)
- secret(name) -> string (retrieve a secret by name, e.g. API keys — NEVER hardcode credentials)
{builtins}
Global tables available:
- TASK.description (string): the user's task description
- PARAMS (table): per-tool parameters (e.g. PARAMS["LOCATION"])

Review criteria:
1. Does the code actually do what the description claims?
2. Tool design: A data tool should do ONE thing (fetch one data source). A glue tool may call run_tool() to compose data tools — that is fine. But a data tool should NOT do multiple unrelated things.
3. Is the tool reusable/generic, or does it have hardcoded values that contradict a generic description? (e.g., description says "any location" but code hardcodes "London")
4. Does the name accurately reflect what the tool does?
5. Does the code use host functions correctly?
6. Does the code handle errors (check HTTP status, handle parse failures)?
7. Credentials: Does the tool use secret() for API keys and passwords? Hardcoded credentials or invented syntax (e.g. @name, ${{name}}) must be replaced with secret("name").

Respond in this exact format:
```verdict
PASS or FAIL
```
```issues
<bullet list of issues found, or "none" if PASS>
```
```suggestions
<specific fix instructions if FAIL, or "none" if PASS>
```"#;

const REGENERATE_PROMPT_TEMPLATE: &str = r#"You are a Lua code generator for a sandboxed runtime. You need to fix a tool that failed code review.

Original tool:
- Name: {name}
- Description: {description}

Original Lua source:
```lua
{source}
```

Issues found by reviewer:
{issues}

Fix instructions:
{suggestions}

Available host functions in the sandbox:
- http_request({{ method = string, url = string, body = string?, headers = {{ [string] = string }}? }}) -> {{ status = number, body = string }}
- http_get(url) -> {{ status = number, body = string }}  (shorthand for GET)
- http_post(url, json_body_string) -> {{ status = number, body = string }}  (shorthand for POST)
- json_parse(string) -> table
- json_encode(table) -> string
- xml_parse(string) -> table (parses XML into {{ tag, attrs?, text?, children? }} tree; namespace URIs are prefixed to tag names)
- xml_encode(table) -> string (encodes a {{ tag, attrs?, text?, children? }} tree back to XML)
- log(message) -> nil
- run_tool(name, params_table) -> table (call another tool by name, passing it a params table)
- secret(name) -> string (retrieve a secret by name, e.g. API keys — NEVER hardcode credentials)
{builtins}
Global tables available:
- TASK.description (string): the user's task description
- PARAMS (table): per-tool parameters (e.g. PARAMS["LOCATION"])

Rules:
- Return a Lua table with the context data
- Fix ALL issues mentioned above
- Use PARAMS for input values, not hardcoded values
- Use secret("name") for ALL credentials (API keys, passwords, tokens) — never hardcode them, never invent alternative syntax
- If the description is too generic for the code, update the description to match
- If the code is too specific, make it generic (e.g., use PARAMS for variable data)
- Keep it simple and focused

Respond in this exact format:
```name
<tool_name>
```
```description
<one line description>
```
```lua
<your fixed lua code>
```"#;

pub struct ReviewResult {
    pub passed: bool,
    pub issues: String,
    pub suggestions: String,
}

fn build_review_prompt(meta: &ToolMeta, source: &str, builtins: &str) -> String {
    REVIEW_PROMPT_TEMPLATE
        .replace("{name}", &meta.name)
        .replace("{description}", &meta.description)
        .replace("{source}", source)
        .replace("{builtins}", builtins)
}

fn build_regenerate_prompt(
    meta: &ToolMeta,
    source: &str,
    review: &ReviewResult,
    builtins: &str,
) -> String {
    REGENERATE_PROMPT_TEMPLATE
        .replace("{name}", &meta.name)
        .replace("{description}", &meta.description)
        .replace("{source}", source)
        .replace("{issues}", &review.issues)
        .replace("{suggestions}", &review.suggestions)
        .replace("{builtins}", builtins)
}

async fn review_tool(
    meta: &ToolMeta,
    source: &str,
    backend: &dyn ModelBackend,
    builtins: &str,
) -> Result<ReviewResult, Box<dyn Error + Send + Sync>> {
    let prompt = build_review_prompt(meta, source, builtins);
    let response = backend.complete(prompt).await?;
    parse_review_response(&response)
}

fn parse_review_response(response: &str) -> Result<ReviewResult, Box<dyn Error + Send + Sync>> {
    let verdict = extract_block(response, "verdict")
        .unwrap_or_default()
        .trim()
        .to_uppercase();
    let issues = extract_block(response, "issues").unwrap_or_else(|| "unknown".to_string());
    let suggestions =
        extract_block(response, "suggestions").unwrap_or_else(|| "unknown".to_string());

    Ok(ReviewResult {
        passed: verdict.contains("PASS"),
        issues,
        suggestions,
    })
}

async fn regenerate_tool(
    meta: &ToolMeta,
    source: &str,
    review: &ReviewResult,
    backend: &dyn ModelBackend,
    builtins: &str,
) -> Result<(ToolMeta, String), Box<dyn Error + Send + Sync>> {
    let prompt = build_regenerate_prompt(meta, source, review, builtins);
    let response = backend.complete(prompt).await?;

    let (name, description, lua_code) = parse_codegen_response(&response)?;

    let new_meta = ToolMeta {
        name: name.clone(),
        description,
        provides: vec![name],
        validated: false,
    };

    Ok((new_meta, lua_code))
}

fn parse_codegen_response(
    response: &str,
) -> Result<(String, String, String), Box<dyn Error + Send + Sync>> {
    let name = extract_block(response, "name").ok_or("missing ```name block")?;
    let description =
        extract_block(response, "description").ok_or("missing ```description block")?;
    let lua_code = extract_block(response, "lua").ok_or("missing ```lua block")?;

    Ok((
        name.trim().to_string(),
        description.trim().to_string(),
        lua_code,
    ))
}

pub(crate) fn extract_block(response: &str, tag: &str) -> Option<String> {
    let start_marker = format!("```{tag}");
    let start = response.find(&start_marker)?;
    let content_start = start + start_marker.len();
    let rest = &response[content_start..];
    let newline = rest.find('\n')?;
    let rest = &rest[newline + 1..];
    let end = rest.find("```")?;
    Some(rest[..end].trim().to_string())
}

pub async fn review_and_fix(
    toolbox: &Toolbox,
    tool_name: &str,
    backend: &dyn ModelBackend,
    log: &EventLog,
    builtins: &[BuiltinInfo],
) -> Result<bool, Box<dyn Error + Send + Sync>> {
    let builtins_str = format_builtins_for_prompt(builtins);
    let mut meta = toolbox.load_meta(tool_name)?;
    let mut source = toolbox.load_source(tool_name)?;

    for attempt in 1..=MAX_FIX_ATTEMPTS {
        let review = review_tool(&meta, &source, backend, &builtins_str).await?;

        log.emit(Event::JanitorReview {
            tool: meta.name.clone(),
            attempt,
            passed: review.passed,
            issues: if review.passed {
                None
            } else {
                Some(review.issues.clone())
            },
        })
        .await;

        if review.passed {
            meta.validated = true;
            toolbox.save_tool(&meta, &source)?;
            return Ok(true);
        }

        if attempt == MAX_FIX_ATTEMPTS {
            let reason = format!("unfixable after {MAX_FIX_ATTEMPTS} attempts");
            log.emit(Event::JanitorEscalated {
                tool: meta.name.clone(),
                reason: reason.clone(),
            })
            .await;

            toolbox.delete_tool(&meta.name)?;
            log.emit(Event::JanitorDeleted {
                tool: meta.name.clone(),
                reason,
            })
            .await;

            return Ok(false);
        }

        log.emit(Event::JanitorRegenerate {
            tool: meta.name.clone(),
            attempt,
        })
        .await;

        let (new_meta, new_source) =
            regenerate_tool(&meta, &source, &review, backend, &builtins_str).await?;
        toolbox.replace_tool(Some(&meta.name), &new_meta, &new_source)?;
        meta = new_meta;
        source = new_source;
    }

    Ok(false)
}

const REDUNDANCY_PROMPT: &str = r#"You are reviewing a Lua toolbox for redundant tools.
{builtin_section}
Lua tools in the toolbox:
{tool_list}

Identify groups of Lua tools that do the same or very similar thing. For each group, decide:
1. If a Lua tool duplicates a built-in tool, recommend deleting the Lua tool.
2. If one Lua tool is clearly better (more generic, better error handling), keep it and delete the others.
3. If multiple Lua tools have complementary features, merge them into the best one.
4. If a Lua tool is site-specific (hardcoded domain/URL) but could be generic, note it for refactoring.

NEVER include built-in tool names in the "delete" or "refactor" lists — they are compiled into the application.

Respond in this exact JSON format:
```json
{{
  "delete": ["tool_name_to_remove", ...],
  "refactor": ["tool_name_thats_too_specific", ...],
  "reason": "brief explanation"
}}
```

If no redundancy found:
```json
{{"delete": [], "refactor": [], "reason": "no redundancy"}}
```"#;

const REFACTOR_PROMPT: &str = r#"This tool has hardcoded site-specific values that should be parameters. Refactor it to be generic and reusable.

Tool: {name}
Description: {description}

Current source:
```lua
{source}
```

Rules:
- Replace hardcoded URLs/domains with PARAMS values
- Make the tool work for ANY site, not just one specific domain
- Keep all existing functionality
- Give it a generic name (e.g. "tag_page_extractor" not "nsg_tag_extractor")

Respond in this exact format:
```name
<generic_tool_name>
```
```description
<generic one line description>
```
```lua
<refactored lua code>
```"#;

pub async fn cleanup_toolbox(
    toolbox: &Toolbox,
    backend: &dyn ModelBackend,
    log: &EventLog,
    builtins: &[BuiltinInfo],
) -> Result<bool, Box<dyn Error + Send + Sync>> {
    let tools = toolbox.list_tools()?;
    if tools.len() < 3 {
        return Ok(false);
    }

    let builtin_names: HashSet<&str> = builtins.iter().map(|b| b.name.as_str()).collect();

    let builtin_section = if builtins.is_empty() {
        String::new()
    } else {
        let list = builtins
            .iter()
            .map(|b| format!("- {} [BUILT-IN]: {}", b.name, b.description))
            .collect::<Vec<_>>()
            .join("\n");
        format!(
            "\nBuilt-in tools (compiled into application, cannot be modified or deleted):\n{list}\n"
        )
    };

    // Build tool list with descriptions and param info
    let tool_list = tools
        .iter()
        .map(|t| {
            let params = toolbox.extract_params(&t.name);
            let params_str = if params.is_empty() {
                String::new()
            } else {
                format!(" (params: {})", params.join(", "))
            };
            format!("- {}: {}{}", t.name, t.description, params_str)
        })
        .collect::<Vec<_>>()
        .join("\n");

    let prompt = REDUNDANCY_PROMPT
        .replace("{builtin_section}", &builtin_section)
        .replace("{tool_list}", &tool_list);
    let response = backend.complete(prompt).await?;

    // Parse response
    let json_str = extract_json_block(&response);

    #[derive(serde::Deserialize)]
    struct CleanupResult {
        #[serde(default)]
        delete: Vec<String>,
        #[serde(default)]
        refactor: Vec<String>,
        #[serde(default)]
        reason: String,
    }

    let result: CleanupResult = serde_json::from_str(&json_str).unwrap_or(CleanupResult {
        delete: Vec::new(),
        refactor: Vec::new(),
        reason: String::new(),
    });

    let mut changed = false;

    // Delete redundant Lua tools (never touch built-ins)
    for name in &result.delete {
        if builtin_names.contains(name.as_str()) {
            continue;
        }
        if toolbox.load_meta(name).is_ok() {
            toolbox.delete_tool(name)?;
            log.emit(Event::JanitorDeleted {
                tool: name.clone(),
                reason: format!("redundant: {}", result.reason),
            })
            .await;
            changed = true;
        }
    }

    // Refactor site-specific Lua tools (never touch built-ins)
    for name in &result.refactor {
        if builtin_names.contains(name.as_str()) {
            continue;
        }
        if let Ok(meta) = toolbox.load_meta(name)
            && let Ok(source) = toolbox.load_source(name)
        {
            let refactor_prompt = REFACTOR_PROMPT
                .replace("{name}", &meta.name)
                .replace("{description}", &meta.description)
                .replace("{source}", &source);

            if let Ok(refactored) = backend.complete(refactor_prompt).await
                && let Ok((new_name, new_desc, new_source)) = parse_codegen_response(&refactored)
            {
                let new_name = new_name.trim().to_string();
                let new_meta = ToolMeta {
                    name: new_name.clone(),
                    description: new_desc.trim().to_string(),
                    provides: vec![new_name.clone()],
                    validated: false,
                };
                toolbox.replace_tool(Some(&meta.name), &new_meta, &new_source)?;
                eprintln!("[janitor] refactored '{}' -> '{}'", meta.name, new_name);
                changed = true;
            }
        }
    }

    Ok(changed)
}

pub(crate) fn extract_json_block(response: &str) -> String {
    // Try ```json block first
    if let Some(start) = response.find("```json") {
        let rest = &response[start + 7..];
        if let Some(end) = rest.find("```") {
            return rest[..end].trim().to_string();
        }
    }
    // Fall back to first { ... }
    if let Some(start) = response.find('{')
        && let Some(end) = response.rfind('}')
    {
        return response[start..=end].to_string();
    }
    "{}".to_string()
}

// ---------------------------------------------------------------------------
// Memory conflict resolution
// ---------------------------------------------------------------------------

const STOP_WORDS: &[&str] = &[
    "user", "is", "has", "the", "a", "an", "in", "on", "at", "to", "for", "of", "with", "by",
    "from", "as", "or", "and", "not", "no", "that", "this", "its", "it", "was", "are", "be",
    "been", "being", "have", "had", "do", "does", "did", "will", "would", "could", "should", "may",
    "might", "can", "shall", "their", "they", "them", "we", "our", "you", "your", "my", "his",
    "her", "he", "she", "but", "if", "then", "than", "so", "such", "very", "too", "also", "just",
    "about", "more", "some", "any", "all", "each", "every", "both", "few", "most", "other", "into",
    "over", "after", "before", "between", "under", "again", "there", "here", "when", "where",
    "how", "what", "which", "who", "whom", "why", "own", "same", "only", "use", "uses", "used",
    "using", "like", "set", "get", "one", "two",
];

fn extract_significant_words(text: &str) -> HashSet<String> {
    let stop: HashSet<&str> = STOP_WORDS.iter().copied().collect();
    text.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.len() >= 3 && !stop.contains(w))
        .map(String::from)
        .collect()
}

fn cluster_memories(memories: &[Memory]) -> Vec<Vec<&Memory>> {
    if memories.is_empty() {
        return vec![];
    }

    // Build word -> memory indices map
    let mut word_map: HashMap<String, Vec<usize>> = HashMap::new();
    for (i, mem) in memories.iter().enumerate() {
        for word in extract_significant_words(&mem.fact) {
            word_map.entry(word).or_default().push(i);
        }
    }

    // Union-find
    let n = memories.len();
    let mut parent: Vec<usize> = (0..n).collect();

    fn find(parent: &mut [usize], mut x: usize) -> usize {
        while parent[x] != x {
            parent[x] = parent[parent[x]];
            x = parent[x];
        }
        x
    }

    fn union(parent: &mut [usize], a: usize, b: usize) {
        let ra = find(parent, a);
        let rb = find(parent, b);
        if ra != rb {
            parent[rb] = ra;
        }
    }

    // Merge indices that share words
    for indices in word_map.values() {
        for window in indices.windows(2) {
            union(&mut parent, window[0], window[1]);
        }
    }

    // Collect clusters
    let mut clusters: HashMap<usize, Vec<usize>> = HashMap::new();
    for i in 0..n {
        let root = find(&mut parent, i);
        clusters.entry(root).or_default().push(i);
    }

    // Return only clusters with 2+ members
    clusters
        .into_values()
        .filter(|c| c.len() >= 2)
        .map(|c| c.into_iter().map(|i| &memories[i]).collect())
        .collect()
}

const MEMORY_CLEANUP_PROMPT: &str = r#"You are a memory janitor. Review the following cluster of related memory facts.
These facts may contain duplicates, contradictions, or outdated information.

Facts:
{facts}

Decide what to do with each fact:
- **keep**: fact is accurate and not redundant — keep as-is
- **update**: fact is partially correct but needs rewording (e.g. merge two near-duplicates into one clearer statement)
- **delete**: fact is redundant (covered by another fact), contradicted by a newer fact, or no longer accurate

Return a JSON object (inside a ```json block) with exactly these fields:
- "keep": array of UUIDs to keep unchanged
- "update": object mapping UUID -> new fact text (the updated wording)
- "delete": array of UUIDs to remove

Every UUID from the input must appear in exactly one of keep, update, or delete.
Prefer keeping the most recent or most specific version of conflicting facts.
When two facts say the same thing differently, keep the better-worded one and delete the other."#;

pub async fn cleanup_memories(
    store: &MemoryStore,
    backend: &dyn ModelBackend,
    log: &EventLog,
) -> Result<(u32, u32), Box<dyn Error + Send + Sync>> {
    let memories = store.list()?;
    if memories.is_empty() {
        return Ok((0, 0));
    }

    let clusters = cluster_memories(&memories);
    if clusters.is_empty() {
        return Ok((0, 0));
    }

    log.emit(Event::MemoryCleanupStarted {
        clusters: clusters.len() as u32,
    })
    .await;

    let mut total_updated: u32 = 0;
    let mut total_deleted: u32 = 0;

    for cluster in &clusters {
        let facts = cluster
            .iter()
            .map(|m| format!("- [{}] {}", m.id, m.fact))
            .collect::<Vec<_>>()
            .join("\n");

        let prompt = MEMORY_CLEANUP_PROMPT.replace("{facts}", &facts);
        let response = match backend.complete(prompt).await {
            Ok(r) => r,
            Err(e) => {
                eprintln!("[janitor] memory cleanup model error: {e}");
                continue;
            }
        };

        let json_str = extract_json_block(&response);
        let parsed: serde_json::Value = match serde_json::from_str(&json_str) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("[janitor] memory cleanup parse error: {e}");
                continue;
            }
        };

        // Apply updates
        if let Some(updates) = parsed.get("update").and_then(|v| v.as_object()) {
            for (uuid_str, new_fact) in updates {
                if let (Ok(uuid), Some(fact_text)) =
                    (uuid_str.parse::<uuid::Uuid>(), new_fact.as_str())
                {
                    if let Err(e) = store.update(uuid, fact_text.to_string()) {
                        eprintln!("[janitor] memory update error: {e}");
                    } else {
                        total_updated += 1;
                    }
                }
            }
        }

        // Apply deletes
        if let Some(deletes) = parsed.get("delete").and_then(|v| v.as_array()) {
            for uuid_val in deletes {
                if let Some(uuid_str) = uuid_val.as_str()
                    && let Ok(uuid) = uuid_str.parse::<uuid::Uuid>()
                {
                    if let Err(e) = store.delete(uuid) {
                        eprintln!("[janitor] memory delete error: {e}");
                    } else {
                        total_deleted += 1;
                    }
                }
            }
        }
    }

    log.emit(Event::MemoryCleanupResult {
        updated: total_updated,
        deleted: total_deleted,
    })
    .await;

    Ok((total_updated, total_deleted))
}

/// Run a single janitor pass: review all unvalidated tools, then run cleanup.
/// Returns the number of tools processed.
pub async fn run_once(
    toolbox: &Toolbox,
    backend: &dyn ModelBackend,
    log: &EventLog,
    builtins: &[BuiltinInfo],
    store: &MemoryStore,
) -> Result<u32, Box<dyn Error + Send + Sync>> {
    let unvalidated = toolbox.list_unvalidated()?;
    let mut processed = 0;

    for tool in &unvalidated {
        if let Err(e) = review_and_fix(toolbox, &tool.name, backend, log, builtins).await {
            eprintln!("[janitor] error processing '{}': {e}", tool.name);
        }
        processed += 1;
    }

    match cleanup_toolbox(toolbox, backend, log, builtins).await {
        Ok(_) => {}
        Err(e) => eprintln!("[janitor] toolbox cleanup error: {e}"),
    }

    match cleanup_memories(store, backend, log).await {
        Ok(_) => {}
        Err(e) => eprintln!("[janitor] memory cleanup error: {e}"),
    }

    match crate::memory_documents::generate_documents(store, backend, log).await {
        Ok(_) => {}
        Err(e) => eprintln!("[janitor] document generation error: {e}"),
    }

    Ok(processed)
}

pub async fn run(
    toolbox: &Toolbox,
    backend: &dyn ModelBackend,
    log: &EventLog,
    builtins: &[BuiltinInfo],
    store: &MemoryStore,
) {
    let mut idle_cycles: u32 = 0;
    let mut cleanup_backed_off = false;
    let mut memory_cleanup_backed_off = false;
    let mut documents_backed_off = false;

    loop {
        let unvalidated = match toolbox.list_unvalidated() {
            Ok(tools) => tools,
            Err(e) => {
                eprintln!("[janitor] error listing tools: {e}");
                sleep(Duration::from_secs(10)).await;
                continue;
            }
        };

        if unvalidated.is_empty() {
            idle_cycles += 1;

            // Clean up redundant/site-specific tools (~50s after idle)
            if !cleanup_backed_off && idle_cycles == 10 {
                match cleanup_toolbox(toolbox, backend, log, builtins).await {
                    Ok(true) => {}
                    Ok(false) => cleanup_backed_off = true,
                    Err(e) => eprintln!("[janitor] toolbox cleanup error: {e}"),
                }
            }

            // Clean up redundant/conflicting memories (~75s after idle)
            if !memory_cleanup_backed_off && idle_cycles == 15 {
                match cleanup_memories(store, backend, log).await {
                    Ok((_, _)) => memory_cleanup_backed_off = true,
                    Err(e) => eprintln!("[janitor] memory cleanup error: {e}"),
                }
            }

            // Generate/update living documents (~100s after idle)
            if !documents_backed_off && idle_cycles == 20 {
                match crate::memory_documents::generate_documents(store, backend, log).await {
                    Ok(_) => documents_backed_off = true,
                    Err(e) => eprintln!("[janitor] document generation error: {e}"),
                }
            }

            sleep(Duration::from_secs(5)).await;
            continue;
        }

        idle_cycles = 0;
        cleanup_backed_off = false;
        memory_cleanup_backed_off = false;
        documents_backed_off = false;
        for tool in &unvalidated {
            if let Err(e) = review_and_fix(toolbox, &tool.name, backend, log, builtins).await {
                eprintln!("[janitor] error processing '{}': {e}", tool.name);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_review_pass() {
        let input = r#"```verdict
PASS
```
```issues
none
```
```suggestions
none
```"#;
        let r = parse_review_response(input).unwrap();
        assert!(r.passed);
        assert_eq!(r.issues, "none");
    }

    #[test]
    fn parse_review_fail() {
        let input = r#"```verdict
FAIL
```
```issues
- hardcoded URL
- no error handling
```
```suggestions
- make URL configurable
```"#;
        let r = parse_review_response(input).unwrap();
        assert!(!r.passed);
        assert!(r.issues.contains("hardcoded URL"));
        assert!(r.suggestions.contains("configurable"));
    }

    #[test]
    fn parse_review_missing_blocks_defaults() {
        let r = parse_review_response("some random text").unwrap();
        assert!(!r.passed);
        assert_eq!(r.issues, "unknown");
        assert_eq!(r.suggestions, "unknown");
    }

    #[test]
    fn parse_review_pass_case_insensitive() {
        let input = "```verdict\nPass\n```\n```issues\nnone\n```\n```suggestions\nnone\n```";
        let r = parse_review_response(input).unwrap();
        assert!(r.passed);
    }

    #[test]
    fn extract_block_basic() {
        assert_eq!(
            extract_block("```lua\ncode here\n```", "lua").unwrap(),
            "code here"
        );
    }

    #[test]
    fn extract_block_missing() {
        assert!(extract_block("no blocks", "lua").is_none());
    }

    #[test]
    fn test_extract_significant_words() {
        let words = extract_significant_words("The user is running Nextcloud on port 8443");
        assert!(words.contains("nextcloud"));
        assert!(words.contains("running"));
        assert!(words.contains("port"));
        assert!(words.contains("8443"));
        // Stop words and short words filtered out
        assert!(!words.contains("the"));
        assert!(!words.contains("user"));
        assert!(!words.contains("is"));
        assert!(!words.contains("on"));
    }

    #[test]
    fn test_cluster_memories_groups_related() {
        let mems = vec![
            Memory::new(
                "Nextcloud runs on port 8443",
                crate::memory::MemorySource::Auto,
            ),
            Memory::new(
                "Blog uses WordPress at blog.example.com",
                crate::memory::MemorySource::Auto,
            ),
            Memory::new(
                "Nextcloud storage limit is 50GB",
                crate::memory::MemorySource::Auto,
            ),
            Memory::new(
                "Blog theme is flavor-flavor",
                crate::memory::MemorySource::Auto,
            ),
        ];
        let clusters = cluster_memories(&mems);
        assert_eq!(clusters.len(), 2);
        // Each cluster should have 2 members
        for c in &clusters {
            assert_eq!(c.len(), 2);
        }
    }

    #[test]
    fn test_cluster_memories_no_overlap() {
        let mems = vec![
            Memory::new("alpha bravo charlie", crate::memory::MemorySource::Auto),
            Memory::new("delta foxtrot golf", crate::memory::MemorySource::Auto),
        ];
        let clusters = cluster_memories(&mems);
        assert!(clusters.is_empty());
    }
}
