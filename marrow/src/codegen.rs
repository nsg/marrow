use std::collections::HashMap;
use std::error::Error;
use std::sync::Arc;

use reqwest::Client;

use crate::context::LuaProvider;
use crate::model::ModelBackend;
use crate::toolbox::{ToolMeta, Toolbox};

const CODEGEN_PROMPT_TEMPLATE: &str = r#"You are a Lua code generator for a sandboxed runtime. Generate a Lua script that provides context data for the given task.

The sandbox has these host functions available:
- http_get(url) -> {{ status = number, body = string }}
- http_post(url, json_body_string) -> {{ status = number, body = string }}
- json_parse(string) -> table
- json_encode(table) -> string
- log(message) -> nil
- run_tool(name, params_table) -> table (call another tool by name, passing it a params table)

Global tables available:
- TASK.description (string): the user's task description
- PARAMS (table): per-tool parameters set by the orchestrator (e.g. PARAMS["LOCATION"])

{available_tools}{knowledge}Design philosophy — each tool does ONE thing well:
- A data tool fetches one data source (weather, calendar, RSS feed, etc.)
- A glue tool composes data tools using run_tool() to build a combined result
- Example glue tool:
  local weather = run_tool("weather_lookup", {{LOCATION = PARAMS["LOCATION"]}})
  local calendar = run_tool("calendar", {{DATE = PARAMS["DATE"]}})
  return {{ weather = weather, calendar = calendar }}

Rules:
- Return a Lua table with the context data
- Use PARAMS for input values (location, timezone, date, url, etc.)
- Be resourceful: if you need to discover something (like an RSS feed URL), fetch the page and parse the HTML to find it rather than requiring it as a parameter
- Use http_get/http_post for external API calls
- Use run_tool() to call existing tools instead of reimplementing their logic
- Use json_parse to parse JSON responses
- Use Lua string patterns for HTML/XML parsing (the sandbox has no DOM library)
- Do NOT use io, os, require, dofile, loadfile, or debug
- Handle errors gracefully (check response status, log useful messages with log())

{tool_hint}Task: {task}

{name_instruction}

Respond in this exact format:
```name
<tool_name>
```
```description
<one line description>
```
```lua
<your lua code>
```"#;

pub struct ToolRequest {
    pub name: String,
    pub expected_params: HashMap<String, String>,
}

pub fn build_codegen_prompt(
    task_description: &str,
    request: Option<&ToolRequest>,
    available_tools: &[ToolMeta],
    knowledge: &str,
) -> String {
    let available_section = if available_tools.is_empty() {
        String::new()
    } else {
        let list = available_tools
            .iter()
            .map(|t| format!("- {}: {}", t.name, t.description))
            .collect::<Vec<_>>()
            .join("\n");
        format!("Existing tools available via run_tool():\n{list}\n\n")
    };

    let (tool_hint, name_instruction) = if let Some(req) = request {
        let params_desc = if req.expected_params.is_empty() {
            String::new()
        } else {
            let params: Vec<String> = req
                .expected_params
                .iter()
                .map(|(k, v)| format!("  - PARAMS[\"{k}\"] = \"{v}\""))
                .collect();
            format!(
                "\nThe orchestrator will pass these parameters:\n{}\n",
                params.join("\n")
            )
        };

        (
            format!(
                "Generate a tool named \"{name}\" that the orchestrator needs.\n{params_desc}\n",
                name = req.name
            ),
            format!(
                "IMPORTANT: The tool MUST be named \"{}\". Use exactly this name.",
                req.name
            ),
        )
    } else {
        (
            String::new(),
            "Also provide a short name (lowercase, no spaces) and one-line description for this tool."
                .to_string(),
        )
    };

    CODEGEN_PROMPT_TEMPLATE
        .replace("{available_tools}", &available_section)
        .replace(
            "{knowledge}",
            &if knowledge.is_empty() {
                String::new()
            } else {
                format!(
                    "Lessons learned from previous code generation (follow these):\n{knowledge}\n\n"
                )
            },
        )
        .replace("{tool_hint}", &tool_hint)
        .replace("{task}", task_description)
        .replace("{name_instruction}", &name_instruction)
}

pub async fn generate_provider(
    task_description: &str,
    backend: &dyn ModelBackend,
    toolbox: &Toolbox,
    client: Arc<Client>,
) -> Result<String, Box<dyn Error + Send + Sync>> {
    let available = toolbox.list_tools().unwrap_or_default();
    generate_provider_with_hint(task_description, backend, toolbox, client, None, &available).await
}

pub async fn generate_provider_with_hint(
    task_description: &str,
    backend: &dyn ModelBackend,
    toolbox: &Toolbox,
    client: Arc<Client>,
    request: Option<&ToolRequest>,
    available_tools: &[ToolMeta],
) -> Result<String, Box<dyn Error + Send + Sync>> {
    let knowledge = toolbox.read_knowledge();
    let prompt = build_codegen_prompt(task_description, request, available_tools, &knowledge);
    let response = backend.complete(prompt).await?;

    let (mut name, description, lua_code) = parse_codegen_response(&response)?;

    if let Some(req) = request {
        name = req.name.clone();
    }

    // Test-run without run_tool access (toolbox_dir = None)
    let provider = LuaProvider::new(&name, &lua_code);
    let test_params = request
        .map(|r| r.expected_params.clone())
        .unwrap_or_default();
    if let Err(e) = provider
        .execute_with_params(task_description, client, &test_params, None)
        .await
    {
        return Err(format!("generated tool '{name}' failed test run: {e}").into());
    }

    let meta = ToolMeta {
        name: name.clone(),
        description,
        provides: vec![name.clone()],
        validated: false,
    };

    toolbox.ensure_dir()?;
    toolbox.save_tool(&meta, &lua_code)?;

    Ok(name)
}

pub async fn generate_provider_for_agent(
    tool_name: &str,
    tool_description: &str,
    task_description: &str,
    backend: &dyn ModelBackend,
    toolbox: &Toolbox,
    client: Arc<Client>,
    available_tools: &[ToolMeta],
) -> Result<String, Box<dyn Error + Send + Sync>> {
    let request = ToolRequest {
        name: tool_name.to_string(),
        expected_params: HashMap::new(),
    };
    let knowledge = toolbox.read_knowledge();
    let mut prompt = build_codegen_prompt(
        task_description,
        Some(&request),
        available_tools,
        &knowledge,
    );
    prompt = prompt.replace(
        &format!("Generate a tool named \"{tool_name}\" that the orchestrator needs.\n"),
        &format!("Generate a tool named \"{tool_name}\": {tool_description}\n"),
    );

    let response = backend.complete(prompt).await?;
    let (_, description, lua_code) = parse_codegen_response(&response)?;

    // Test-run — tool may need params so we accept graceful errors
    let provider = LuaProvider::new(tool_name, &lua_code);
    if let Err(e) = provider
        .execute_with_params(task_description, client, &HashMap::new(), None)
        .await
    {
        let err_str = e.to_string();
        // Allow graceful param-missing errors (tool returns error table), reject crashes
        if !err_str.contains("attempt to index a nil")
            && !err_str.contains("attempt to call a nil")
            && !err_str.contains("stack overflow")
        {
            // Likely a graceful error — save the tool
        } else {
            return Err(format!("generated tool '{tool_name}' failed test run: {e}").into());
        }
    }

    let meta = ToolMeta {
        name: tool_name.to_string(),
        description,
        provides: vec![tool_name.to_string()],
        validated: false,
    };

    toolbox.ensure_dir()?;
    toolbox.save_tool(&meta, &lua_code)?;

    Ok(tool_name.to_string())
}

fn parse_codegen_response(
    response: &str,
) -> Result<(String, String, String), Box<dyn Error + Send + Sync>> {
    let name = extract_block(response, "name").ok_or("model response missing ```name block")?;
    let description = extract_block(response, "description")
        .ok_or("model response missing ```description block")?;
    let lua_code = extract_block(response, "lua").ok_or("model response missing ```lua block")?;

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_block_basic() {
        let input = "```name\nweather\n```";
        assert_eq!(extract_block(input, "name").unwrap(), "weather");
    }

    #[test]
    fn extract_block_with_surrounding_text() {
        let input = "Here is the tool:\n```lua\nreturn {}\n```\nDone.";
        assert_eq!(extract_block(input, "lua").unwrap(), "return {}");
    }

    #[test]
    fn extract_block_missing() {
        assert!(extract_block("no blocks here", "name").is_none());
    }

    #[test]
    fn extract_block_trims_whitespace() {
        let input = "```name\n  weather_tool  \n```";
        assert_eq!(extract_block(input, "name").unwrap(), "weather_tool");
    }

    #[test]
    fn parse_codegen_response_full() {
        let input = r#"Here is the tool:
```name
my_tool
```
```description
Does something useful
```
```lua
return { ok = true }
```"#;
        let (name, desc, code) = parse_codegen_response(input).unwrap();
        assert_eq!(name, "my_tool");
        assert_eq!(desc, "Does something useful");
        assert_eq!(code, "return { ok = true }");
    }

    #[test]
    fn parse_codegen_response_missing_block() {
        let input = "```name\ntest\n```\n```lua\nreturn {}\n```";
        assert!(parse_codegen_response(input).is_err());
    }

    #[test]
    fn codegen_prompt_includes_available_tools() {
        let tools = vec![ToolMeta {
            name: "weather".to_string(),
            description: "Get weather data".to_string(),
            provides: vec![],
            validated: true,
        }];
        let prompt = build_codegen_prompt("test task", None, &tools, "");
        assert!(prompt.contains("weather: Get weather data"));
        assert!(prompt.contains("run_tool"));
    }

    #[test]
    fn codegen_prompt_empty_tools() {
        let prompt = build_codegen_prompt("test task", None, &[], "");
        assert!(!prompt.contains("Existing tools available"));
    }
}
