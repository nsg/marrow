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

pub async fn run(toolbox: &Toolbox, backend: &dyn ModelBackend, log: &EventLog) {
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
            sleep(Duration::from_secs(5)).await;
            continue;
        }

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
