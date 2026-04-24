use std::error::Error;
use std::path::{Path, PathBuf};

use crate::events::{Event, EventLog};
use crate::memory::MemoryStore;
use crate::memory_documents;
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

    pub fn save(&self, name: &str, content: &str) -> Result<(), Box<dyn Error + Send + Sync>> {
        self.ensure_dir()?;
        std::fs::write(self.dir.join(name), content)?;
        Ok(())
    }

    pub fn load(&self, name: &str) -> Result<String, Box<dyn Error + Send + Sync>> {
        let path = self.dir.join(name);
        Ok(std::fs::read_to_string(path)?)
    }
}

/// Select relevant skills for a task based on keyword matching.
/// Returns up to 3 skills sorted by relevance score.
pub fn select_skills(
    task: &str,
    store: &SkillStore,
) -> Result<Vec<(String, String)>, Box<dyn Error + Send + Sync>> {
    let skills = store.list()?;
    if skills.is_empty() {
        return Ok(vec![]);
    }

    let task_words: Vec<String> = task
        .to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.len() >= 3)
        .map(String::from)
        .collect();

    if task_words.is_empty() {
        return Ok(vec![]);
    }

    let mut scored: Vec<(usize, &(String, String))> = skills
        .iter()
        .map(|skill| {
            // Extract keywords from filename (without .md) and first line
            let name_part = skill.0.trim_end_matches(".md").to_lowercase();
            let first_line = skill.1.lines().next().unwrap_or("").to_lowercase();
            let skill_text = format!("{name_part} {first_line}");

            let skill_words: Vec<String> = skill_text
                .split(|c: char| !c.is_alphanumeric())
                .filter(|w| w.len() >= 3)
                .map(String::from)
                .collect();

            let score = task_words
                .iter()
                .filter(|tw| {
                    skill_words
                        .iter()
                        .any(|sw| sw.contains(tw.as_str()) || tw.contains(sw.as_str()))
                })
                .count();

            (score, skill)
        })
        .filter(|(score, _)| *score >= 1)
        .collect();

    scored.sort_by_key(|b| std::cmp::Reverse(b.0));
    scored.truncate(3);

    Ok(scored.into_iter().map(|(_, skill)| skill.clone()).collect())
}

const SKILL_GENERATION_PROMPT: &str = r#"You are a skill author for a workflow automation agent. Review the agent's knowledge base and tools, then create or update procedural skill guides.

## Knowledge base (living documents)

{documents}

## Individual memory facts

{facts}

## Available tools

{tools}

## Existing skills

{existing_skills}

## Instructions

Create markdown skill files that combine knowledge from the documents and individual facts with tool references into step-by-step procedural guides. Each skill should help the agent accomplish a specific category of task.

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
- Only create skills when there's enough knowledge AND relevant tools to make them useful
- Individual facts may contain specific details worth embedding in skills
- Skill filenames should be short, descriptive, kebab-case (e.g. check-calendar.md)
- Include specific parameter values the agent should use (URLs, service names, etc.)
- Reference tools by name so the agent knows what to call
- Keep each skill focused on one task category
- Update existing skills if the knowledge base has changed
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

/// Generate or update skill files based on knowledge and tools.
/// Returns the number of skills created/updated.
pub async fn generate_skills(
    skill_store: &SkillStore,
    store: &MemoryStore,
    knowledge_dir: &Path,
    tools: &[ToolInfo],
    backend: &dyn ModelBackend,
    log: &EventLog,
) -> Result<u32, Box<dyn Error + Send + Sync>> {
    let documents = memory_documents::list_documents(knowledge_dir);
    let facts = store.list().unwrap_or_default();
    if documents.is_empty() && facts.is_empty() {
        // No knowledge base yet — nothing to build skills from
        return Ok(0);
    }

    let documents_section = if documents.is_empty() {
        "(no living documents yet)".to_string()
    } else {
        documents
            .iter()
            .map(|(name, content)| format!("### {name}\n{content}"))
            .collect::<Vec<_>>()
            .join("\n\n")
    };

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
        .replace("{documents}", &documents_section)
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
    fn select_skills_matching() {
        let dir = tempfile::Builder::new()
            .prefix("marrow_skills")
            .tempdir()
            .unwrap();
        let store = SkillStore::new(dir.path());
        store
            .save(
                "check-calendar.md",
                "# Check Calendar\nUse nextcloud calendar tool",
            )
            .unwrap();
        store
            .save("deploy-blog.md", "# Deploy Blog\nSSH and pull latest code")
            .unwrap();

        let selected = select_skills("Check my nextcloud calendar for tomorrow", &store).unwrap();
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].0, "check-calendar.md");
    }

    #[test]
    fn select_skills_no_match() {
        let dir = tempfile::Builder::new()
            .prefix("marrow_skills")
            .tempdir()
            .unwrap();
        let store = SkillStore::new(dir.path());
        store
            .save("check-calendar.md", "# Check Calendar\nUse cal tool")
            .unwrap();

        let selected = select_skills("What is the weather?", &store).unwrap();
        assert!(selected.is_empty());
    }

    #[test]
    fn select_skills_empty_store() {
        let dir = tempfile::Builder::new()
            .prefix("marrow_skills")
            .tempdir()
            .unwrap();
        let store = SkillStore::new(dir.path());
        let selected = select_skills("anything", &store).unwrap();
        assert!(selected.is_empty());
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
