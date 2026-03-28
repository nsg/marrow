use std::error::Error;
use std::time::Duration;

use tokio::time::sleep;

use crate::events::{Event, EventLog};
use crate::model::ModelBackend;
use crate::toolbox::{ToolMeta, Toolbox};

const MAX_FIX_ATTEMPTS: u32 = 3;

const REVIEW_PROMPT_TEMPLATE: &str = r#"You are a code reviewer for Lua scripts that run in a sandboxed environment. Review the following tool for quality and correctness.

Tool metadata:
- Name: {name}
- Description: {description}

Lua source:
```lua
{source}
```

Available host functions in the sandbox:
- http_get(url) -> {{ status = number, body = string }}
- http_post(url, json_body_string) -> {{ status = number, body = string }}
- json_parse(string) -> table
- json_encode(table) -> string
- log(message) -> nil

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
- http_get(url) -> {{ status = number, body = string }}
- http_post(url, json_body_string) -> {{ status = number, body = string }}
- json_parse(string) -> table
- json_encode(table) -> string
- log(message) -> nil

Global tables available:
- TASK.description (string): the user's task description
- PARAMS (table): per-tool parameters (e.g. PARAMS["LOCATION"])

Rules:
- Return a Lua table with the context data
- Fix ALL issues mentioned above
- Use PARAMS for input values, not hardcoded values
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

fn build_review_prompt(meta: &ToolMeta, source: &str) -> String {
    REVIEW_PROMPT_TEMPLATE
        .replace("{name}", &meta.name)
        .replace("{description}", &meta.description)
        .replace("{source}", source)
}

fn build_regenerate_prompt(meta: &ToolMeta, source: &str, review: &ReviewResult) -> String {
    REGENERATE_PROMPT_TEMPLATE
        .replace("{name}", &meta.name)
        .replace("{description}", &meta.description)
        .replace("{source}", source)
        .replace("{issues}", &review.issues)
        .replace("{suggestions}", &review.suggestions)
}

async fn review_tool(
    meta: &ToolMeta,
    source: &str,
    backend: &dyn ModelBackend,
) -> Result<ReviewResult, Box<dyn Error + Send + Sync>> {
    let prompt = build_review_prompt(meta, source);
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
) -> Result<(ToolMeta, String), Box<dyn Error + Send + Sync>> {
    let prompt = build_regenerate_prompt(meta, source, review);
    let response = backend.complete(prompt).await?;

    let (name, description, lua_code) = parse_codegen_response(&response)?;

    let new_meta = ToolMeta {
        name,
        description,
        provides: meta.provides.clone(),
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

fn extract_block(response: &str, tag: &str) -> Option<String> {
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
) -> Result<bool, Box<dyn Error + Send + Sync>> {
    let mut meta = toolbox.load_meta(tool_name)?;
    let mut source = toolbox.load_source(tool_name)?;

    for attempt in 1..=MAX_FIX_ATTEMPTS {
        let review = review_tool(&meta, &source, backend).await?;

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

        // Extract lessons from review issues and save to knowledge file
        for line in review.issues.lines() {
            let line = line.trim().trim_start_matches('-').trim();
            if !line.is_empty() && line.len() > 10 && line != "none" && line != "unknown" {
                let _ = toolbox.append_knowledge(line);
            }
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

        let (new_meta, new_source) = regenerate_tool(&meta, &source, &review, backend).await?;
        if new_meta.name != meta.name {
            toolbox.delete_tool(&meta.name)?;
        }
        toolbox.save_tool(&new_meta, &new_source)?;
        meta = new_meta;
        source = new_source;
    }

    Ok(false)
}

const CONSOLIDATE_PROMPT: &str = r#"You are maintaining a knowledge file of lessons learned from code generation. The file has grown and needs cleanup.

Current notes:
{notes}

Consolidate these into a clean, deduplicated list of general lessons. Rules:
- Remove duplicates and near-duplicates
- Merge related points into single concise lessons
- Remove tool-specific details (tool names, specific URLs) — keep only the general pattern
- Each lesson should be one line starting with "- "
- Keep only lessons that would help a code generator avoid common mistakes
- Maximum 20 lessons

Respond with ONLY the cleaned list, nothing else."#;

const CONSOLIDATE_THRESHOLD: usize = 16384;

/// Returns true if consolidation ran, false if skipped.
pub async fn consolidate_knowledge(
    toolbox: &Toolbox,
    backend: &dyn ModelBackend,
) -> Result<bool, Box<dyn Error + Send + Sync>> {
    let notes = toolbox.read_knowledge();
    if notes.len() < CONSOLIDATE_THRESHOLD {
        return Ok(false);
    }

    let prompt = CONSOLIDATE_PROMPT.replace("{notes}", &notes);
    let response = backend.complete(prompt).await?;
    let cleaned = response.trim().to_string();

    if !cleaned.is_empty() && cleaned.len() < notes.len() {
        std::fs::write(toolbox.knowledge_path(), &cleaned)?;
        eprintln!("[janitor] consolidated codegen knowledge file ({} -> {} bytes)", notes.len(), cleaned.len());
        Ok(true)
    } else {
        Ok(false)
    }
}

const REDUNDANCY_PROMPT: &str = r#"You are reviewing a toolbox for redundant tools. Here are all the tools:

{tool_list}

Identify groups of tools that do the same or very similar thing. For each group, decide:
1. If one tool is clearly better (more generic, better error handling), keep it and delete the others.
2. If multiple tools have complementary features, merge them into the best one.
3. If a tool is site-specific (hardcoded domain/URL) but could be generic, note it for refactoring.

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
) -> Result<bool, Box<dyn Error + Send + Sync>> {
    let tools = toolbox.list_tools()?;
    if tools.len() < 3 {
        return Ok(false);
    }

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

    let prompt = REDUNDANCY_PROMPT.replace("{tool_list}", &tool_list);
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

    // Delete redundant tools
    for name in &result.delete {
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

    // Refactor site-specific tools
    for name in &result.refactor {
        if let Ok(meta) = toolbox.load_meta(name) {
            if let Ok(source) = toolbox.load_source(name) {
                let refactor_prompt = REFACTOR_PROMPT
                    .replace("{name}", &meta.name)
                    .replace("{description}", &meta.description)
                    .replace("{source}", &source);

                if let Ok(refactored) = backend.complete(refactor_prompt).await {
                    if let Ok((new_name, new_desc, new_source)) =
                        parse_codegen_response(&refactored)
                    {
                        let new_name = new_name.trim().to_string();
                        let new_meta = ToolMeta {
                            name: new_name.clone(),
                            description: new_desc.trim().to_string(),
                            provides: vec![new_name.clone()],
                            validated: false,
                        };
                        toolbox.save_tool(&new_meta, &new_source)?;
                        if new_name != meta.name {
                            toolbox.delete_tool(&meta.name)?;
                        }
                        eprintln!(
                            "[janitor] refactored '{}' -> '{}'",
                            meta.name, new_name
                        );
                        changed = true;
                    }
                }
            }
        }
    }

    Ok(changed)
}

fn extract_json_block(response: &str) -> String {
    // Try ```json block first
    if let Some(start) = response.find("```json") {
        let rest = &response[start + 7..];
        if let Some(end) = rest.find("```") {
            return rest[..end].trim().to_string();
        }
    }
    // Fall back to first { ... }
    if let Some(start) = response.find('{') {
        if let Some(end) = response.rfind('}') {
            return response[start..=end].to_string();
        }
    }
    "{}".to_string()
}

pub async fn run(toolbox: &Toolbox, backend: &dyn ModelBackend, log: &EventLog) {
    let mut idle_cycles: u32 = 0;
    let mut knowledge_backed_off = false;
    let mut cleanup_backed_off = false;

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

            // Consolidate knowledge file (~50s after idle)
            if !knowledge_backed_off && idle_cycles == 10 {
                match consolidate_knowledge(toolbox, backend).await {
                    Ok(true) => {}
                    Ok(false) => knowledge_backed_off = true,
                    Err(e) => eprintln!("[janitor] knowledge consolidation error: {e}"),
                }
            }

            // Clean up redundant/site-specific tools (~100s after idle)
            if !cleanup_backed_off && idle_cycles == 20 {
                match cleanup_toolbox(toolbox, backend, log).await {
                    Ok(true) => {}
                    Ok(false) => cleanup_backed_off = true,
                    Err(e) => eprintln!("[janitor] toolbox cleanup error: {e}"),
                }
            }

            sleep(Duration::from_secs(5)).await;
            continue;
        }

        idle_cycles = 0;
        knowledge_backed_off = false;
        cleanup_backed_off = false;
        for tool in &unvalidated {
            if let Err(e) = review_and_fix(toolbox, &tool.name, backend, log).await {
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
        assert_eq!(extract_block("```lua\ncode here\n```", "lua").unwrap(), "code here");
    }

    #[test]
    fn extract_block_missing() {
        assert!(extract_block("no blocks", "lua").is_none());
    }
}
