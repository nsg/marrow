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

const SKILL_PLANNING_PROMPT: &str = r#"You are planning which skills to create for a workflow automation agent. Below are numbered clusters of related memory facts and the available tools.

## Memory clusters

{clusters}

## Available tools

{tools}

## Existing skills

{existing_skills}

## Instructions

Decide which clusters contain enough facts AND have relevant tools to form useful procedural skills.

For each skill worth creating or updating, output a JSON object mapping skill filename to the cluster numbers it needs:

```json
{{
  "check-calendar.md": [1, 3],
  "deploy-blog.md": [5]
}}
```

Rules:
- A skill can reference multiple clusters (e.g. a "morning briefing" skill might need calendar + RSS clusters)
- Only select clusters that have matching tools — facts without tools aren't actionable
- Use kebab-case .md filenames
- If an existing skill's clusters have changed, include it for regeneration
- If nothing useful can be created or updated, output: `{{}}`"#;

const SKILL_GENERATION_PROMPT: &str = r#"You are a skill author for a workflow automation agent. Create or update the following skill using the provided facts and tools.

## Relevant facts

{facts}

## Available tools

{tools}

## Existing skill content (if updating)

{existing_skill}

## Instructions

Create a markdown skill file that combines the facts with tool references into a step-by-step procedural guide.

Output the skill as a fenced block:
```skill:{filename}
# Skill Title
<procedural markdown content>
```

Rules:
- Include specific parameter values the agent should use (URLs, service names, etc.)
- Reference tools by name so the agent knows what to call
- Keep it focused on one task category
- If updating, preserve any working instructions and incorporate new facts"#;

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
    let db_clusters = store.load_clusters()?;
    if db_clusters.is_empty() {
        return Ok(0);
    }

    // Build a map from memory ID to fact text
    let all_facts = store.list().unwrap_or_default();
    let fact_map: std::collections::HashMap<String, &str> = all_facts
        .iter()
        .map(|m| (m.id.to_string(), m.fact.as_str()))
        .collect();

    // Build cluster summaries for the planning prompt
    let clusters_section = db_clusters
        .iter()
        .map(|c| {
            let facts: String = c
                .member_ids
                .iter()
                .take(3)
                .filter_map(|id| fact_map.get(id.as_str()))
                .map(|f| format!("\n  - {f}"))
                .collect();
            let more = if c.member_ids.len() > 3 {
                format!("\n  ... and {} more", c.member_ids.len() - 3)
            } else {
                String::new()
            };
            format!(
                "Cluster {} — {} ({} facts){facts}{more}",
                c.cluster_id,
                c.summary,
                c.member_ids.len()
            )
        })
        .collect::<Vec<_>>()
        .join("\n");

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
            .map(|(name, _)| format!("- {name}"))
            .collect::<Vec<_>>()
            .join("\n")
    };

    // Pass 1: plan which clusters map to which skills
    let plan_prompt = SKILL_PLANNING_PROMPT
        .replace("{clusters}", &clusters_section)
        .replace("{tools}", &tools_section)
        .replace("{existing_skills}", &existing_section);

    let plan_response = backend.complete(plan_prompt).await?;
    let plan_json = crate::janitor::extract_json_block(&plan_response);
    let plan: std::collections::HashMap<String, Vec<usize>> =
        serde_json::from_str(&plan_json).unwrap_or_default();

    if plan.is_empty() {
        return Ok(0);
    }

    // Pass 2: generate each skill with only its relevant facts
    let mut count: u32 = 0;

    for (filename, cluster_ids) in &plan {
        let safe_name: String = filename
            .chars()
            .filter(|c| c.is_alphanumeric() || *c == '-' || *c == '_' || *c == '.')
            .collect();
        if safe_name.is_empty() || !safe_name.ends_with(".md") {
            continue;
        }

        let selected_facts: String = cluster_ids
            .iter()
            .filter_map(|&id| db_clusters.iter().find(|c| c.cluster_id == id))
            .flat_map(|c| c.member_ids.iter())
            .filter_map(|mid| fact_map.get(mid.as_str()))
            .map(|f| format!("- {f}"))
            .collect::<Vec<_>>()
            .join("\n");

        if selected_facts.is_empty() {
            continue;
        }

        let existing_content = existing
            .iter()
            .find(|(name, _)| name == &safe_name)
            .map(|(_, content)| content.as_str())
            .unwrap_or("(new skill)");

        let gen_prompt = SKILL_GENERATION_PROMPT
            .replace("{facts}", &selected_facts)
            .replace("{tools}", &tools_section)
            .replace("{existing_skill}", existing_content)
            .replace("{filename}", &safe_name);

        let gen_response = match backend.complete(gen_prompt).await {
            Ok(r) => r,
            Err(e) => {
                eprintln!("[janitor] skill generation error for {safe_name}: {e}");
                continue;
            }
        };

        let skill_blocks = parse_skill_blocks(&gen_response);
        for (_, content) in &skill_blocks {
            if let Err(e) = skill_store.save(&safe_name, content) {
                eprintln!("[janitor] skill save error: {e}");
            } else {
                count += 1;
            }
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
