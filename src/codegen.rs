use std::error::Error;
use std::sync::Arc;

use reqwest::Client;

use crate::context::LuaProvider;
use crate::model::ModelBackend;
use crate::toolbox::{ToolMeta, Toolbox};

const CODEGEN_PROMPT_TEMPLATE: &str = r#"You are a Lua code generator for a sandboxed runtime. Generate a Lua script that provides context data for the given task.

The sandbox has these host functions available:
- http_get(url) -> { status = number, body = string }
- http_post(url, json_body_string) -> { status = number, body = string }
- json_parse(string) -> table
- json_encode(table) -> string
- log(message) -> nil

Global variable available:
- TASK_DESCRIPTION (string): the user's task description

Rules:
- Return a Lua table with the context data
- Use http_get/http_post for external API calls
- Use json_parse to parse JSON responses
- Do NOT use io, os, require, dofile, loadfile, or debug
- Keep it simple and focused on the task
- Handle errors gracefully (check response status)

Task: {task}

Also provide a short name (lowercase, no spaces) and one-line description for this tool.

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

pub fn build_codegen_prompt(task_description: &str) -> String {
    CODEGEN_PROMPT_TEMPLATE.replace("{task}", task_description)
}

pub async fn generate_provider(
    task_description: &str,
    backend: &dyn ModelBackend,
    toolbox: &Toolbox,
    client: Arc<Client>,
) -> Result<String, Box<dyn Error + Send + Sync>> {
    let prompt = build_codegen_prompt(task_description);
    let response = backend.complete(prompt).await?;

    let (name, description, lua_code) = parse_codegen_response(&response)?;

    // Test-run the generated Lua before saving
    let provider = LuaProvider::new(&name, &lua_code);
    if let Err(e) = provider.execute(task_description, client).await {
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

    // Skip to next line after the opening marker
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
}
