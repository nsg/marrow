use std::error::Error;
use std::path::{Path, PathBuf};

use crate::events::{Event, EventLog};
use crate::memory::MemoryStore;
use crate::model::ModelBackend;
use crate::tool::ToolInfo;

pub struct SkillStore {
    dir: PathBuf,
}

impl SkillStore {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn ensure_dir(&self) -> Result<(), Box<dyn Error + Send + Sync>> {
        std::fs::create_dir_all(&self.dir)?;
        Ok(())
    }

    pub fn list(&self) -> Result<Vec<(String, String)>, Box<dyn Error + Send + Sync>> {
        let mut skills = Vec::new();
        if !self.dir.exists() {
            return Ok(skills);
        }
        for entry in std::fs::read_dir(&self.dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().is_some_and(|e| e == "md")
                && let Some(name) = path.file_name().and_then(|n| n.to_str())
            {
                let content = std::fs::read_to_string(&path).unwrap_or_default();
                if !content.is_empty() {
                    skills.push((name.to_string(), content));
                }
            }
        }
        Ok(skills)
    }

    /// Return a lightweight catalog: `(filename, description)` for each skill.
    /// Description is the heading + the first non-empty body line, giving the
    /// model enough signal to decide whether to load the full skill.
    pub fn catalog(&self) -> Result<Vec<(String, String)>, Box<dyn Error + Send + Sync>> {
        let mut entries = Vec::new();
        if !self.dir.exists() {
            return Ok(entries);
        }
        for entry in std::fs::read_dir(&self.dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().is_some_and(|e| e == "md")
                && let Some(name) = path.file_name().and_then(|n| n.to_str())
            {
                let content = std::fs::read_to_string(&path).unwrap_or_default();
                let mut non_empty = content.lines().filter(|l| !l.trim().is_empty());

                let heading = non_empty
                    .next()
                    .unwrap_or("")
                    .trim_start_matches('#')
                    .trim()
                    .to_string();
                if heading.is_empty() {
                    continue;
                }

                // Grab the first body line as a short description
                let body_line = non_empty
                    .next()
                    .map(|l| l.trim().to_string())
                    .unwrap_or_default();

                let desc = if body_line.is_empty() {
                    heading
                } else {
                    format!("{heading} — {body_line}")
                };
                entries.push((name.to_string(), desc));
            }
        }
        Ok(entries)
    }

    pub fn save(&self, name: &str, content: &str) -> Result<(), Box<dyn Error + Send + Sync>> {
        self.ensure_dir()?;
        std::fs::write(self.dir.join(name), content)?;
        Ok(())
    }

    pub fn load(&self, name: &str) -> Result<String, Box<dyn Error + Send + Sync>> {
        let path = self.dir.join(name);
        Ok(std::fs::read_to_string(path)?)
    }

    pub fn load_dynamic(
        &self,
        name: &str,
        tools: &[ToolInfo],
    ) -> Result<String, Box<dyn Error + Send + Sync>> {
        let normalized = name.strip_suffix(".md").unwrap_or(name);
        if normalized == "lua" {
            return Ok(generate_lua_reference(tools));
        }
        self.load(name)
    }

    pub fn catalog_with_lua(&self) -> Result<Vec<(String, String)>, Box<dyn Error + Send + Sync>> {
        let mut entries = self.catalog()?;
        entries.push((
            "lua.md".to_string(),
            "Lua API Reference — All available functions and tools".to_string(),
        ));
        Ok(entries)
    }
}

fn generate_lua_reference(tools: &[ToolInfo]) -> String {
    let mut out = String::from("# Lua API Reference\n\n");

    out.push_str("## Sandbox built-in functions\n\n");
    out.push_str("### HTTP\n");
    out.push_str("- `http_request({method, url, body?, headers?})` → `{status, body}` — General HTTP request\n");
    out.push_str("- `http_get(url)` → `{status, body}` — GET shorthand\n");
    out.push_str("- `http_post(url, body)` → `{status, body}` — POST shorthand (Content-Type: application/json)\n\n");

    out.push_str("### JSON / XML\n");
    out.push_str("- `json_parse(string)` → table — Parse JSON string into Lua table\n");
    out.push_str("- `json_encode(table)` → string — Encode Lua table as JSON string\n");
    out.push_str(
        "- `xml_parse(string)` → table — Parse XML into `{tag, attrs?, text?, children?}` tree\n",
    );
    out.push_str("- `xml_encode(table)` → string — Encode table tree back to XML\n\n");

    out.push_str("### Secrets\n");
    out.push_str("- `secret(name)` → string — Retrieve an API key or password by name. Use `secret(\"name\")` and concatenate into URLs/headers. Never write `\"secret:name\"` as a literal string.\n\n");

    out.push_str("### Utility\n");
    out.push_str("- `log(message)` — Print to stderr (for debugging)\n\n");

    out.push_str("### Standard Lua (available)\n");
    out.push_str("string.*, table.*, math.*, tonumber, tostring, type, pairs, ipairs, pcall, select, unpack\n\n");

    out.push_str("### Unavailable (sandboxed out)\n");
    out.push_str("require, os, io, debug, dofile, loadfile, package, base64, collectgarbage\n\n");

    if !tools.is_empty() {
        out.push_str("## Tool functions\n\n");
        out.push_str("All tools are available as Lua global functions. Built-in tools use their name directly. Saved toolbox tools are prefixed with `tool_`.\n\n");
        for t in tools {
            let params_str = if t.params.is_empty() {
                String::new()
            } else {
                let inner: Vec<String> = t
                    .params
                    .iter()
                    .map(|(name, required)| {
                        if *required {
                            name.clone()
                        } else {
                            format!("{name}?")
                        }
                    })
                    .collect();
                format!("{{{}}}", inner.join(", "))
            };
            let returns_str = if t.returns.is_empty() {
                String::new()
            } else {
                format!(" → `{{{}}}`", t.returns.join(", "))
            };
            out.push_str(&format!(
                "### `{}({})`{}\n{}\n\n",
                t.lua_name(),
                params_str,
                returns_str,
                t.description
            ));
        }
    }

    out
}

const SKILL_GENERATION_PROMPT: &str = r#"You are a skill author for a workflow automation agent. Review the agent's memory facts and tools, then create or update procedural skill guides.

## Memory facts

{facts}

## Available tools

{tools}

## Existing skills

{existing_skills}

## Instructions

Create markdown skill files that combine memory facts with tool references into step-by-step procedural guides. Each skill should help the agent accomplish a specific category of task.

Good skills:
- "Check calendar" — combines calendar service URL + authentication details + the right tool to call
- "Deploy blog" — step-by-step using known infrastructure + available tools
- "Send notification" — which service to use, what credentials, which tool

Output each skill as a fenced block:
```skill:filename.md
# Skill Title
<procedural markdown content>
```

Rules:
- Only create skills when there's enough facts AND relevant tools to make them useful
- Facts may contain specific details worth embedding in skills
- Skill filenames should be short, descriptive, kebab-case (e.g. check-calendar.md)
- Include specific parameter values the agent should use (URLs, service names, etc.)
- Reference tools by name so the agent knows what to call
- Keep each skill focused on one task category
- Update existing skills if the facts have changed
- If there's nothing useful to create or update, output nothing"#;

pub fn parse_skill_blocks(response: &str) -> Vec<(String, String)> {
    let mut skills = Vec::new();
    let mut search_from = 0;

    while search_from < response.len() {
        let rest = &response[search_from..];
        if let Some(start) = rest.find("```skill:") {
            let after_tag = &rest[start + 9..];
            if let Some(newline) = after_tag.find('\n') {
                let filename = after_tag[..newline].trim().to_string();
                let content_start = &after_tag[newline + 1..];
                if let Some(end) = content_start.find("```") {
                    let content = content_start[..end].trim().to_string();
                    if !filename.is_empty() && !content.is_empty() {
                        skills.push((filename, content));
                    }
                    search_from += start + 9 + newline + 1 + end + 3;
                } else {
                    break;
                }
            } else {
                break;
            }
        } else {
            break;
        }
    }

    skills
}

/// Generate or update skill files based on memory facts and tools.
/// Returns the number of skills created/updated.
pub async fn generate_skills(
    skill_store: &SkillStore,
    store: &MemoryStore,
    tools: &[ToolInfo],
    backend: &dyn ModelBackend,
    log: &EventLog,
) -> Result<u32, Box<dyn Error + Send + Sync>> {
    let facts = store.list().unwrap_or_default();
    if facts.is_empty() {
        return Ok(0);
    }

    let facts_section = if facts.is_empty() {
        "(no individual facts)".to_string()
    } else {
        facts
            .iter()
            .map(|m| format!("- {}", m.fact))
            .collect::<Vec<_>>()
            .join("\n")
    };

    let tools_section = if tools.is_empty() {
        "(no tools available)".to_string()
    } else {
        tools
            .iter()
            .map(|t| t.usage_line())
            .collect::<Vec<_>>()
            .join("\n")
    };

    let existing = skill_store.list().unwrap_or_default();
    let existing_section = if existing.is_empty() {
        "(none yet)".to_string()
    } else {
        existing
            .iter()
            .map(|(name, content)| format!("### {name}\n{content}"))
            .collect::<Vec<_>>()
            .join("\n\n")
    };

    let prompt = SKILL_GENERATION_PROMPT
        .replace("{facts}", &facts_section)
        .replace("{tools}", &tools_section)
        .replace("{existing_skills}", &existing_section);

    let response = backend.complete(prompt).await?;
    let skill_blocks = parse_skill_blocks(&response);
    let mut count: u32 = 0;

    for (filename, content) in &skill_blocks {
        // Sanitize filename
        let safe_name = filename
            .chars()
            .filter(|c| c.is_alphanumeric() || *c == '-' || *c == '_' || *c == '.')
            .collect::<String>();
        if safe_name.is_empty() || !safe_name.ends_with(".md") {
            continue;
        }
        if let Err(e) = skill_store.save(&safe_name, content) {
            eprintln!("[janitor] skill save error: {e}");
        } else {
            count += 1;
        }
    }

    if count > 0 {
        log.emit(Event::SkillsGenerated { count }).await;
    }

    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_skill_blocks_basic() {
        let response = r#"Here are the skills:
```skill:check-calendar.md
# Check Calendar
1. Call calendar_events tool
2. Format results
```

```skill:deploy-blog.md
# Deploy Blog
1. SSH to server
2. Pull latest
```
"#;
        let blocks = parse_skill_blocks(response);
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0].0, "check-calendar.md");
        assert!(blocks[0].1.contains("Check Calendar"));
        assert_eq!(blocks[1].0, "deploy-blog.md");
        assert!(blocks[1].1.contains("Deploy Blog"));
    }

    #[test]
    fn parse_skill_blocks_empty() {
        let blocks = parse_skill_blocks("No skills to create.");
        assert!(blocks.is_empty());
    }

    #[test]
    fn skill_store_catalog() {
        let dir = tempfile::Builder::new()
            .prefix("marrow_skills")
            .tempdir()
            .unwrap();
        let store = SkillStore::new(dir.path());
        store
            .save(
                "check-calendar.md",
                "# Check Calendar\nFetches events from CalDAV",
            )
            .unwrap();
        store
            .save("check-weather.md", "# Check Weather\nGets forecast data")
            .unwrap();

        let catalog = store.catalog().unwrap();
        assert_eq!(catalog.len(), 2);
        // Check that we got heading + body description
        assert!(
            catalog.iter().any(|(n, l)| n == "check-calendar.md"
                && l == "Check Calendar — Fetches events from CalDAV")
        );
        assert!(
            catalog
                .iter()
                .any(|(n, l)| n == "check-weather.md" && l == "Check Weather — Gets forecast data")
        );
    }

    #[test]
    fn skill_store_crud() {
        let dir = tempfile::Builder::new()
            .prefix("marrow_skills")
            .tempdir()
            .unwrap();
        let store = SkillStore::new(dir.path());

        store.save("test.md", "# Test Skill").unwrap();
        let content = store.load("test.md").unwrap();
        assert_eq!(content, "# Test Skill");

        let list = store.list().unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].0, "test.md");
    }
}
