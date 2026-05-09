use std::collections::{HashMap, HashSet};
use std::error::Error;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tokio::time::sleep;

use crate::events::{Event, EventLog};
use crate::memory::{Memory, MemorySource, MemoryStore};
use crate::model::ModelBackend;
use crate::schedule::ScheduleStore;
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
        "\nBuilt-in tools (compiled into the application, always available as Lua functions):\n\
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
- All built-in tools are also available as Lua global functions (see builtins list below)
- secret(name) -> string (retrieve a secret by name, e.g. API keys — NEVER hardcode credentials)
{builtins}
Global tables available:
- TASK.description (string): the user's task description
- PARAMS (table): per-tool parameters (e.g. PARAMS["LOCATION"])

Review criteria:
1. Does the code actually do what the description claims?
2. Tool design: A data tool should do ONE thing (fetch one data source). A glue tool may call other tool functions to compose data tools — that is fine. But a data tool should NOT do multiple unrelated things.
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
- All built-in tools are also available as Lua global functions (see builtins list below)
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
            "\nBuilt-in tools (available as Lua functions, cannot be modified or deleted):\n{list}\n"
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

fn cluster_with_threshold<'a>(
    memories: &[&'a Memory],
    min_shared: usize,
) -> (Vec<Vec<&'a Memory>>, Vec<&'a Memory>) {
    if memories.is_empty() {
        return (vec![], vec![]);
    }

    let n = memories.len();
    let word_sets: Vec<HashSet<String>> = memories
        .iter()
        .map(|m| extract_significant_words(&m.fact))
        .collect();

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

    for i in 0..n {
        for j in (i + 1)..n {
            let shared = word_sets[i].intersection(&word_sets[j]).count();
            if shared >= min_shared {
                union(&mut parent, i, j);
            }
        }
    }

    let mut groups: HashMap<usize, Vec<usize>> = HashMap::new();
    for i in 0..n {
        let root = find(&mut parent, i);
        groups.entry(root).or_default().push(i);
    }

    let mut clusters = Vec::new();
    let mut singletons = Vec::new();
    for indices in groups.into_values() {
        if indices.len() >= 2 {
            clusters.push(indices.into_iter().map(|i| memories[i]).collect());
        } else {
            singletons.push(memories[indices[0]]);
        }
    }
    (clusters, singletons)
}

/// Cluster memories requiring 2+ shared words. Returns only clusters with 2+ members.
pub fn cluster_memories(memories: &[Memory]) -> Vec<Vec<&Memory>> {
    let refs: Vec<&Memory> = memories.iter().collect();
    let (clusters, _) = cluster_with_threshold(&refs, 2);
    clusters
}

/// Two-tier clustering: tight clusters (2+ shared words), then loose groups
/// (1+ shared word) for the remaining singletons. Every memory is accounted for.
pub fn cluster_all_memories(memories: &[Memory]) -> Vec<Vec<&Memory>> {
    let refs: Vec<&Memory> = memories.iter().collect();

    // Pass 1: tight clusters
    let (mut clusters, singletons) = cluster_with_threshold(&refs, 2);

    if singletons.is_empty() {
        return clusters;
    }

    // Pass 2: looser grouping on leftovers
    let (loose_clusters, remaining) = cluster_with_threshold(&singletons, 1);
    clusters.extend(loose_clusters);

    // Anything still ungrouped stays as individual clusters
    for m in remaining {
        clusters.push(vec![m]);
    }

    clusters
}

const MEMORY_CLEANUP_PROMPT: &str = r#"You are a memory janitor. Review the following cluster of related memory facts.
These facts may contain duplicates, contradictions, or outdated information.

Each fact is tagged with its source:
- (user) — the user explicitly stated this
- (auto) — the agent discovered or derived this

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
When two facts conflict, user-sourced facts always take precedence over auto-sourced facts.
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
            .map(|m| format!("- [{}] ({}) {}", m.id, m.source.as_str(), m.fact))
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

const MAX_FACT_LENGTH: usize = 300;

const DECOMPOSE_PROMPT: &str = r##"You are a memory decomposition system. The following stored fact is too large — it contains multiple pieces of information packed into one entry. Break it into individual, atomic facts.

Large fact:
{fact}

Rules:
- Each output fact must be a single, self-contained piece of information
- Be lean: "Nextcloud hosted at nextcloud.example.com" not "The user's Nextcloud server is hosted at..."
- Preserve ALL information — don't drop details
- Drop markdown formatting, headers, and structure — output plain facts
- Skip empty/meta lines (headers that just say "# Infrastructure" add nothing)

Respond with ONLY a JSON array of strings:
["fact one", "fact two", ...]"##;

async fn decompose_large_memories(
    store: &MemoryStore,
    backend: &dyn ModelBackend,
    log: &EventLog,
) -> Result<(u32, u32), Box<dyn Error + Send + Sync>> {
    let memories = store.list()?;
    let large: Vec<&Memory> = memories
        .iter()
        .filter(|m| m.fact.len() > MAX_FACT_LENGTH)
        .collect();

    if large.is_empty() {
        return Ok((0, 0));
    }

    log.emit(Event::MemoryCleanupStarted {
        clusters: large.len() as u32,
    })
    .await;

    let mut total_created = 0u32;
    let mut total_deleted = 0u32;

    for memory in &large {
        let prompt = DECOMPOSE_PROMPT.replace("{fact}", &memory.fact);
        let response = match backend.complete(prompt).await {
            Ok(r) => r,
            Err(e) => {
                eprintln!("[janitor] decompose model error: {e}");
                continue;
            }
        };

        let facts = parse_fact_array(&response);
        if facts.is_empty() {
            continue;
        }

        // Save individual facts, then delete the blob
        for fact_text in &facts {
            let new_mem = Memory::new(fact_text, MemorySource::Auto);
            if let Err(e) = store.save(&new_mem) {
                eprintln!("[janitor] decompose save error: {e}");
                continue;
            }
            total_created += 1;
        }

        if let Err(e) = store.delete(memory.id) {
            eprintln!("[janitor] decompose delete error: {e}");
        } else {
            total_deleted += 1;
        }

        eprintln!(
            "[janitor] decomposed 1 large fact into {} atomic facts",
            facts.len()
        );
    }

    if total_created > 0 || total_deleted > 0 {
        log.emit(Event::MemoryCleanupResult {
            updated: total_created,
            deleted: total_deleted,
        })
        .await;
    }

    Ok((total_created, total_deleted))
}

fn parse_fact_array(response: &str) -> Vec<String> {
    let trimmed = response.trim();

    // Try to find JSON array in markdown fence first
    let json_str = if let Some(start) = trimmed.find("```json") {
        let content_start = start + 7;
        let rest = &trimmed[content_start..];
        let end = rest.find("```").unwrap_or(rest.len());
        rest[..end].trim()
    } else if let Some(start) = trimmed.find('[') {
        let end = trimmed.rfind(']').unwrap_or(trimmed.len() - 1);
        &trimmed[start..=end]
    } else {
        return Vec::new();
    };

    serde_json::from_str::<Vec<String>>(json_str).unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Memory cluster summarization
// ---------------------------------------------------------------------------

const CLUSTER_SUMMARY_PROMPT: &str = r#"Generate a short topic label (2-4 words) for each group of related memory facts.

{clusters}

Respond with a JSON object mapping group number to label:
```json
{{"0": "Nextcloud Infrastructure", "1": "Blog & RSS"}}
```

Rules:
- Labels should be concise topic descriptions, not sentences
- Title case
- If a group has just one fact, derive the label from that fact"#;

pub async fn summarize_clusters(
    store: &MemoryStore,
    backend: &dyn ModelBackend,
    log: &EventLog,
) -> Result<u32, Box<dyn Error + Send + Sync>> {
    let memories = store.list()?;
    if memories.is_empty() {
        store.save_clusters(&[])?;
        return Ok(0);
    }

    let clusters = cluster_all_memories(&memories);
    if clusters.is_empty() {
        return Ok(0);
    }

    // Build prompt with numbered clusters
    let clusters_text = clusters
        .iter()
        .enumerate()
        .map(|(i, cluster)| {
            let facts: String = cluster
                .iter()
                .map(|m| format!("  - {}", m.fact))
                .collect::<Vec<_>>()
                .join("\n");
            format!("Group {i}:\n{facts}")
        })
        .collect::<Vec<_>>()
        .join("\n\n");

    let prompt = CLUSTER_SUMMARY_PROMPT.replace("{clusters}", &clusters_text);
    let response = backend.complete(prompt).await?;
    let json_str = extract_json_block(&response);
    let summaries: HashMap<String, String> = serde_json::from_str(&json_str).unwrap_or_default();

    let result: Vec<crate::memory::MemoryCluster> = clusters
        .iter()
        .enumerate()
        .map(|(i, cluster)| {
            let summary = summaries
                .get(&i.to_string())
                .cloned()
                .unwrap_or_else(|| cluster[0].fact.chars().take(40).collect());
            crate::memory::MemoryCluster {
                cluster_id: i,
                summary,
                member_ids: cluster.iter().map(|m| m.id.to_string()).collect(),
            }
        })
        .collect();

    let count = result.len() as u32;
    store.save_clusters(&result)?;

    eprintln!(
        "[janitor] clustered {} memories into {} groups",
        memories.len(),
        count
    );

    log.emit(Event::MemoryCleanupResult {
        updated: count,
        deleted: 0,
    })
    .await;

    Ok(count)
}

async fn backfill_embeddings(
    store: &MemoryStore,
    embed_backend: &dyn crate::model::EmbedBackend,
) -> Result<u32, Box<dyn Error + Send + Sync>> {
    let unembedded = store.unembedded()?;
    if unembedded.is_empty() {
        return Ok(0);
    }

    let mut count = 0u32;
    // Batch in chunks of 50
    for chunk in unembedded.chunks(50) {
        let texts: Vec<String> = chunk.iter().map(|m| m.fact.clone()).collect();
        let embeddings = embed_backend.embed(texts).await?;
        for (memory, embedding) in chunk.iter().zip(embeddings.iter()) {
            store.set_embedding(memory.id, embedding)?;
            count += 1;
        }
    }

    Ok(count)
}

// ---------------------------------------------------------------------------
// Schedule description review
// ---------------------------------------------------------------------------

const SCHEDULE_REVIEW_PROMPT: &str = r#"You are reviewing a scheduled task description. The description serves two purposes:
1. The FIRST LINE is shown as the card title in the dashboard (keep it under 60 chars)
2. The rest (after a blank line) is the full instruction the agent follows at runtime

Current description:
---
{description}
---

User memories that may be relevant to this schedule:
{memories}

Tasks:
1. FORMAT: If the description doesn't start with a short plain-text title line followed by a blank line, restructure it. The title must be plain text (no markdown, no `#` prefix). The body keeps ALL the original instructions — do not remove or simplify them.
2. MEMORIES: If any user memories contain preferences, corrections, or context relevant to this schedule's purpose, incorporate them into the body instructions. Only incorporate clearly relevant memories — don't stretch.
3. If the description already has proper format AND no relevant memories need incorporating, respond with SKIP.

Respond in this exact format:

If changes needed:
```description
Short title here

Full instructions here, incorporating any relevant memory context...
```

If no changes needed:
```verdict
SKIP
```"#;

const SCHEDULE_MEMORY_LIMIT: usize = 15;

fn format_memories_for_prompt(memories: &[(Memory, f32)]) -> String {
    if memories.is_empty() {
        return "(no relevant memories found)".to_string();
    }
    memories
        .iter()
        .map(|(m, _)| format!("- ({}) {}", m.source.as_str(), m.fact))
        .collect::<Vec<_>>()
        .join("\n")
}

pub async fn review_schedules(
    schedule_store: &ScheduleStore,
    memory_store: &MemoryStore,
    backend: &dyn ModelBackend,
    log: &EventLog,
    embed_backend: &dyn crate::model::EmbedBackend,
) -> Result<u32, Box<dyn Error + Send + Sync>> {
    let schedules = schedule_store.list()?;
    if schedules.is_empty() {
        return Ok(0);
    }

    let mut updated = 0u32;

    for schedule in &schedules {
        let query_vec = embed_backend
            .embed(vec![schedule.description.clone()])
            .await?;
        let query_vec = query_vec.first().ok_or("embedding returned no vectors")?;
        let relevant = memory_store.nearest(query_vec, SCHEDULE_MEMORY_LIMIT)?;
        let memory_text = format_memories_for_prompt(&relevant);

        let prompt = SCHEDULE_REVIEW_PROMPT
            .replace("{description}", &schedule.description)
            .replace("{memories}", &memory_text);

        let response = match backend.complete(prompt).await {
            Ok(r) => r,
            Err(e) => {
                eprintln!("[janitor] schedule review model error: {e}");
                continue;
            }
        };

        if response.contains("SKIP") && extract_block(&response, "verdict").is_some() {
            log.emit(Event::ScheduleReviewed {
                schedule_id: schedule.id.to_string(),
                updated: false,
            })
            .await;
            continue;
        }

        if let Some(new_desc) = extract_block(&response, "description") {
            let new_desc = new_desc.trim().to_string();
            if !new_desc.is_empty() && new_desc != schedule.description {
                let mut updated_schedule = schedule.clone();
                updated_schedule.description = new_desc;
                if let Err(e) = schedule_store.update(&updated_schedule) {
                    eprintln!("[janitor] schedule update error: {e}");
                    continue;
                }
                log.emit(Event::ScheduleReviewed {
                    schedule_id: schedule.id.to_string(),
                    updated: true,
                })
                .await;
                updated += 1;
            }
        }
    }

    Ok(updated)
}

/// Run a single janitor pass: review all unvalidated tools, then run cleanup.
/// Returns the number of tools processed.
#[allow(clippy::too_many_arguments)]
pub async fn run_once(
    toolbox: &Toolbox,
    backend: &dyn ModelBackend,
    log: &EventLog,
    builtins: &[BuiltinInfo],
    store: &MemoryStore,
    skill_store: &crate::skills::SkillStore,
    tools: &[crate::tool::ToolInfo],
    schedule_store: &ScheduleStore,
    embed_backend: Option<&dyn crate::model::EmbedBackend>,
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

    match decompose_large_memories(store, backend, log).await {
        Ok(_) => {}
        Err(e) => eprintln!("[janitor] memory decompose error: {e}"),
    }

    match cleanup_memories(store, backend, log).await {
        Ok(_) => {}
        Err(e) => eprintln!("[janitor] memory cleanup error: {e}"),
    }

    match summarize_clusters(store, backend, log).await {
        Ok(_) => {}
        Err(e) => eprintln!("[janitor] cluster summarization error: {e}"),
    }

    if let Some(eb) = embed_backend {
        match review_schedules(schedule_store, store, backend, log, eb).await {
            Ok(_) => {}
            Err(e) => eprintln!("[janitor] schedule review error: {e}"),
        }
    } else {
        eprintln!("[janitor] skipping schedule review: no embedding backend configured");
    }

    match crate::skills::generate_skills(skill_store, store, tools, backend, log).await {
        Ok(_) => {}
        Err(e) => eprintln!("[janitor] skill generation error: {e}"),
    }

    Ok(processed)
}

#[allow(clippy::too_many_arguments)]
pub async fn run(
    toolbox: &Toolbox,
    backend: &dyn ModelBackend,
    log: &EventLog,
    builtins: &[BuiltinInfo],
    store: &MemoryStore,
    skill_store: &crate::skills::SkillStore,
    tools: &[crate::tool::ToolInfo],
    memory_changed: &AtomicBool,
    embed_backend: Option<&dyn crate::model::EmbedBackend>,
    schedule_store: &ScheduleStore,
) {
    let mut idle_cycles: u32 = 0;
    let mut cleanup_backed_off = false;
    let mut decompose_backed_off = false;
    let mut memory_cleanup_backed_off = false;
    let mut embeddings_backed_off = false;
    let mut clusters_backed_off = false;
    let mut skills_backed_off = false;
    let mut schedules_backed_off = false;

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

            // Reset memory-related tasks when new memories arrive
            if memory_changed.swap(false, Ordering::Relaxed) {
                decompose_backed_off = false;
                memory_cleanup_backed_off = false;
                embeddings_backed_off = false;
                clusters_backed_off = false;
                skills_backed_off = false;
                schedules_backed_off = false;
                idle_cycles = idle_cycles.min(10);
            }

            // Clean up redundant/site-specific tools (~50s after idle)
            if !cleanup_backed_off && idle_cycles == 10 {
                match cleanup_toolbox(toolbox, backend, log, builtins).await {
                    Ok(true) => {}
                    Ok(false) => cleanup_backed_off = true,
                    Err(e) => eprintln!("[janitor] toolbox cleanup error: {e}"),
                }
            }

            // Decompose large memory blobs into atomic facts (~60s after idle)
            if !decompose_backed_off && idle_cycles == 12 {
                match decompose_large_memories(store, backend, log).await {
                    Ok((0, _)) => decompose_backed_off = true,
                    Ok(_) => {
                        // Don't back off — there may be more blobs, and new facts
                        // need dedup in the cleanup pass
                    }
                    Err(e) => {
                        eprintln!("[janitor] memory decompose error: {e}");
                        decompose_backed_off = true;
                    }
                }
            }

            // Clean up redundant/conflicting memories (~75s after idle)
            if !memory_cleanup_backed_off && idle_cycles == 15 {
                match cleanup_memories(store, backend, log).await {
                    Ok((_, _)) => memory_cleanup_backed_off = true,
                    Err(e) => eprintln!("[janitor] memory cleanup error: {e}"),
                }
            }

            // Cluster and summarize memories (~85s after idle)
            if !clusters_backed_off && idle_cycles == 17 {
                match summarize_clusters(store, backend, log).await {
                    Ok(_) => clusters_backed_off = true,
                    Err(e) => eprintln!("[janitor] cluster summarization error: {e}"),
                }
            }

            // Backfill embeddings for facts that don't have them yet (~100s after idle)
            if !embeddings_backed_off && idle_cycles == 20 {
                if let Some(eb) = embed_backend {
                    match backfill_embeddings(store, eb).await {
                        Ok(0) => embeddings_backed_off = true,
                        Ok(n) => eprintln!("[janitor] embedded {n} fact(s)"),
                        Err(e) => {
                            eprintln!("[janitor] embedding backfill error: {e}");
                            embeddings_backed_off = true;
                        }
                    }
                } else {
                    embeddings_backed_off = true;
                }
            }

            // Generate/update skills (~125s after idle)
            if !skills_backed_off && idle_cycles == 25 {
                match crate::skills::generate_skills(skill_store, store, tools, backend, log).await
                {
                    Ok(_) => skills_backed_off = true,
                    Err(e) => eprintln!("[janitor] skill generation error: {e}"),
                }
            }

            // Review schedule descriptions (~150s after idle)
            if !schedules_backed_off && idle_cycles == 30 {
                if let Some(eb) = embed_backend {
                    match review_schedules(schedule_store, store, backend, log, eb).await {
                        Ok(_) => schedules_backed_off = true,
                        Err(e) => eprintln!("[janitor] schedule review error: {e}"),
                    }
                } else {
                    schedules_backed_off = true;
                }
            }

            sleep(Duration::from_secs(5)).await;
            continue;
        }

        idle_cycles = 0;
        cleanup_backed_off = false;
        decompose_backed_off = false;
        memory_cleanup_backed_off = false;
        embeddings_backed_off = false;
        clusters_backed_off = false;
        skills_backed_off = false;
        schedules_backed_off = false;
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
                "Nextcloud server runs on port 8443",
                crate::memory::MemorySource::Auto,
            ),
            Memory::new(
                "Blog deployment uses WordPress hosting",
                crate::memory::MemorySource::Auto,
            ),
            Memory::new(
                "Nextcloud server storage limit is 50GB",
                crate::memory::MemorySource::Auto,
            ),
            Memory::new(
                "Blog deployment theme is flavor-flavor",
                crate::memory::MemorySource::Auto,
            ),
        ];
        let clusters = cluster_memories(&mems);
        assert_eq!(clusters.len(), 2);
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
