use std::error::Error;

use uuid::Uuid;

use crate::memory::{Memory, MemorySource, MemoryStore};
use crate::model::ModelBackend;

const WRITER_PROMPT_TEMPLATE: &str = r#"You are a memory management system. After a task completes, review the interaction and decide what facts are worth remembering for future tasks.

Existing memories:
{existing}

Task: {task}
Response: {response}

You can:
1. SAVE new facts (single, lean facts — one per entry)
2. UPDATE existing facts (if the interaction provides more accurate info)
3. DELETE outdated facts
4. Do NOTHING if nothing is worth remembering

Rules:
- Each fact must be a single, self-contained piece of information
- Be lean: "User prefers UTC timezone" not "The user mentioned they prefer UTC timezone when asking about time"
- Don't save task-specific details (like "user asked about weather in London")
- DO save preferences, patterns, and reusable knowledge
- When the user explicitly asks to remember something, always save it
- For each saved fact, indicate the source:
  - "user" — the user explicitly stated or confirmed this fact in their message
  - "auto" — the agent discovered, derived, or looked this up (not directly from the user's words)

Respond in this exact JSON format:
```json
{{
  "save": [{{"fact": "fact 1", "source": "user"}}, {{"fact": "fact 2", "source": "auto"}}],
  "update": {{"<uuid>": "updated fact text"}},
  "delete": ["<uuid>"]
}}
```

If nothing to do, respond with:
```json
{{
  "save": [],
  "update": {{}},
  "delete": []
}}
```"#;

#[derive(Debug, serde::Deserialize)]
struct SaveEntry {
    fact: String,
    #[serde(default = "default_source")]
    source: String,
}

fn default_source() -> String {
    "auto".to_string()
}

#[derive(Debug, serde::Deserialize)]
struct WriterResponse {
    #[serde(default, deserialize_with = "deserialize_save_entries")]
    save: Vec<SaveEntry>,
    #[serde(default)]
    update: std::collections::HashMap<String, String>,
    #[serde(default)]
    delete: Vec<String>,
}

/// Accept both the new format [{"fact": "...", "source": "..."}] and the
/// legacy format ["fact 1", "fact 2"] for backwards compatibility.
fn deserialize_save_entries<'de, D>(deserializer: D) -> Result<Vec<SaveEntry>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value: serde_json::Value = serde::Deserialize::deserialize(deserializer)?;
    match value {
        serde_json::Value::Array(arr) => {
            let mut entries = Vec::new();
            for item in arr {
                match item {
                    serde_json::Value::Object(_) => {
                        if let Ok(entry) = serde_json::from_value::<SaveEntry>(item) {
                            entries.push(entry);
                        }
                    }
                    serde_json::Value::String(s) => {
                        entries.push(SaveEntry {
                            fact: s,
                            source: "auto".to_string(),
                        });
                    }
                    _ => {}
                }
            }
            Ok(entries)
        }
        _ => Ok(Vec::new()),
    }
}

/// Summary of what the memory writer did.
#[derive(Debug, Default)]
pub struct MemoryWriterResult {
    pub saved: Vec<String>,
    pub updated: Vec<String>,
    pub deleted: usize,
}

pub async fn process_interaction(
    task_description: &str,
    response_text: &str,
    store: &MemoryStore,
    backend: &dyn ModelBackend,
) -> Result<MemoryWriterResult, Box<dyn Error + Send + Sync>> {
    let existing = store.list()?;

    let existing_list = if existing.is_empty() {
        "(none)".to_string()
    } else {
        existing
            .iter()
            .map(|m| format!("- [{}] {}", m.id, m.fact))
            .collect::<Vec<_>>()
            .join("\n")
    };

    let prompt = WRITER_PROMPT_TEMPLATE
        .replace("{existing}", &existing_list)
        .replace("{task}", task_description)
        .replace("{response}", response_text);

    let model_response = backend.complete(prompt).await?;
    let actions = parse_writer_response(&model_response)?;

    let mut result = MemoryWriterResult::default();

    for entry in &actions.save {
        let source = match entry.source.as_str() {
            "user" => MemorySource::User,
            _ => MemorySource::Auto,
        };
        let memory = Memory::new(&entry.fact, source);
        store.save(&memory)?;
        result.saved.push(entry.fact.clone());
    }

    for (id_str, new_fact) in &actions.update {
        if let Ok(id) = id_str.parse::<Uuid>()
            && store.update(id, new_fact.clone()).is_ok()
        {
            result.updated.push(new_fact.clone());
        }
    }

    for id_str in &actions.delete {
        if let Ok(id) = id_str.parse::<Uuid>()
            && store.delete(id).is_ok()
        {
            result.deleted += 1;
        }
    }

    Ok(result)
}

fn parse_writer_response(response: &str) -> Result<WriterResponse, Box<dyn Error + Send + Sync>> {
    let trimmed = response.trim();

    // Find JSON block (might be wrapped in ```json ... ```)
    let json_str = if let Some(start) = trimmed.find("```json") {
        let content_start = start + 7;
        let rest = &trimmed[content_start..];
        let end = rest.find("```").unwrap_or(rest.len());
        rest[..end].trim()
    } else if let Some(start) = trimmed.find('{') {
        let end = trimmed.rfind('}').unwrap_or(trimmed.len() - 1);
        &trimmed[start..=end]
    } else {
        return Ok(WriterResponse {
            save: Vec::new(),
            update: std::collections::HashMap::new(),
            delete: Vec::new(),
        });
    };

    let parsed: WriterResponse = serde_json::from_str(json_str).unwrap_or(WriterResponse {
        save: Vec::new(),
        update: std::collections::HashMap::new(),
        delete: Vec::new(),
    });

    Ok(parsed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_save_facts_new_format() {
        let input = r#"{"save": [{"fact": "User prefers UTC", "source": "user"}], "update": {}, "delete": []}"#;
        let r = parse_writer_response(input).unwrap();
        assert_eq!(r.save.len(), 1);
        assert_eq!(r.save[0].fact, "User prefers UTC");
        assert_eq!(r.save[0].source, "user");
        assert!(r.update.is_empty());
        assert!(r.delete.is_empty());
    }

    #[test]
    fn parse_save_facts_legacy_format() {
        let input = r#"{"save": ["User prefers UTC"], "update": {}, "delete": []}"#;
        let r = parse_writer_response(input).unwrap();
        assert_eq!(r.save.len(), 1);
        assert_eq!(r.save[0].fact, "User prefers UTC");
        assert_eq!(r.save[0].source, "auto");
    }

    #[test]
    fn parse_save_facts_mixed_format() {
        let input = r#"{"save": [{"fact": "from user", "source": "user"}, "plain string"], "update": {}, "delete": []}"#;
        let r = parse_writer_response(input).unwrap();
        assert_eq!(r.save.len(), 2);
        assert_eq!(r.save[0].fact, "from user");
        assert_eq!(r.save[0].source, "user");
        assert_eq!(r.save[1].fact, "plain string");
        assert_eq!(r.save[1].source, "auto");
    }

    #[test]
    fn parse_save_default_source() {
        let input = r#"{"save": [{"fact": "no source field"}], "update": {}, "delete": []}"#;
        let r = parse_writer_response(input).unwrap();
        assert_eq!(r.save[0].source, "auto");
    }

    #[test]
    fn parse_wrapped_in_json_block() {
        let input = "Here is what to remember:\n```json\n{\"save\": [{\"fact\": \"likes coffee\", \"source\": \"user\"}], \"update\": {}, \"delete\": []}\n```";
        let r = parse_writer_response(input).unwrap();
        assert_eq!(r.save[0].fact, "likes coffee");
        assert_eq!(r.save[0].source, "user");
    }

    #[test]
    fn parse_with_update_and_delete() {
        let input = r#"{"save": [], "update": {"abc-123": "updated fact"}, "delete": ["def-456"]}"#;
        let r = parse_writer_response(input).unwrap();
        assert!(r.save.is_empty());
        assert_eq!(r.update.get("abc-123").unwrap(), "updated fact");
        assert_eq!(r.delete, vec!["def-456"]);
    }

    #[test]
    fn parse_no_json_returns_empty() {
        let r = parse_writer_response("Nothing to remember here.").unwrap();
        assert!(r.save.is_empty());
        assert!(r.update.is_empty());
        assert!(r.delete.is_empty());
    }

    #[test]
    fn parse_json_with_surrounding_text() {
        let input = r#"I think we should save this: {"save": [{"fact": "fact one", "source": "auto"}], "update": {}, "delete": []} that's all"#;
        let r = parse_writer_response(input).unwrap();
        assert_eq!(r.save[0].fact, "fact one");
    }

    #[test]
    fn parse_malformed_json_returns_empty() {
        let r = parse_writer_response("{broken json").unwrap();
        assert!(r.save.is_empty());
    }
}
