use std::error::Error;
use std::path::Path;

use crate::events::{Event, EventLog};
use crate::janitor::{extract_block, extract_json_block};
use crate::memory::MemoryStore;
use crate::model::ModelBackend;

pub const DOCUMENT_FILES: &[(&str, &str)] = &[
    (
        "profile.md",
        "User profile: name, email, preferences, personal details, communication style",
    ),
    (
        "infrastructure.md",
        "Infrastructure: services, servers, endpoints, ports, credentials, software stack",
    ),
];

const DOCUMENT_PROMPT: &str = r#"You are a memory curator. Your job is to organize individual memory facts into structured living documents.

Below are the current documents and all remaining individual facts. Update the documents by incorporating relevant facts, then list which fact UUIDs were promoted (absorbed into a document).

{documents_section}
## Individual facts (JSON memories)

{facts}

## Instructions

For each document, output an updated version inside a fenced block like:
```document:profile.md
<updated markdown content>
```

Then output a JSON block listing the UUIDs of facts you incorporated into documents:
```json
{{"promoted": ["uuid1", "uuid2", ...]}}
```

Rules:
- Only promote a fact if its information is fully captured in a document
- Keep documents concise — use bullet points and sections, not prose
- If a document has no relevant facts to add, omit its block (don't output unchanged docs)
- If a fact doesn't fit any document category, leave it as an individual fact (don't promote it)
- Preserve existing document content that is still accurate
- Organize information logically with markdown headers"#;

fn load_document(dir: &Path, name: &str) -> String {
    let path = dir.join(name);
    std::fs::read_to_string(path).unwrap_or_default()
}

/// Returns (filename, content) pairs for all existing document files.
pub fn list_documents(dir: &Path) -> Vec<(String, String)> {
    let mut docs = Vec::new();
    for (name, _) in DOCUMENT_FILES {
        let content = load_document(dir, name);
        if !content.is_empty() {
            docs.push((name.to_string(), content));
        }
    }
    docs
}

/// Generate/update living documents from memory facts.
/// Returns (documents_updated, facts_promoted).
pub async fn generate_documents(
    store: &MemoryStore,
    backend: &dyn ModelBackend,
    log: &EventLog,
) -> Result<(u32, u32), Box<dyn Error + Send + Sync>> {
    let memories = store.list()?;
    if memories.is_empty() {
        return Ok((0, 0));
    }

    let dir = store.dir();

    // Build documents section
    let mut documents_section = String::new();
    for (name, description) in DOCUMENT_FILES {
        let content = load_document(dir, name);
        documents_section.push_str(&format!("## {name}\nPurpose: {description}\n"));
        if content.is_empty() {
            documents_section.push_str("(empty — not yet created)\n\n");
        } else {
            documents_section.push_str(&format!("Current content:\n{content}\n\n"));
        }
    }

    // Build facts section
    let facts = memories
        .iter()
        .map(|m| format!("- [{}] {}", m.id, m.fact))
        .collect::<Vec<_>>()
        .join("\n");

    let prompt = DOCUMENT_PROMPT
        .replace("{documents_section}", &documents_section)
        .replace("{facts}", &facts);

    let response = backend.complete(prompt).await?;

    let mut docs_updated: u32 = 0;
    let mut facts_promoted: u32 = 0;

    // Extract and write document blocks
    for (name, _) in DOCUMENT_FILES {
        let tag = format!("document:{name}");
        if let Some(content) = extract_block(&response, &tag) {
            let trimmed = content.trim();
            if !trimmed.is_empty() {
                // Ensure the memory directory exists for writing document files
                store.ensure_dir()?;
                std::fs::write(dir.join(name), trimmed)?;
                docs_updated += 1;
            }
        }
    }

    // Extract and delete promoted facts
    let json_str = extract_json_block(&response);
    if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&json_str)
        && let Some(promoted) = parsed.get("promoted").and_then(|v| v.as_array())
    {
        for uuid_val in promoted {
            if let Some(uuid_str) = uuid_val.as_str()
                && let Ok(uuid) = uuid_str.parse::<uuid::Uuid>()
            {
                if let Err(e) = store.delete(uuid) {
                    eprintln!("[janitor] document promotion delete error: {e}");
                } else {
                    facts_promoted += 1;
                }
            }
        }
    }

    if docs_updated > 0 || facts_promoted > 0 {
        log.emit(Event::DocumentsUpdated {
            documents: docs_updated,
            promoted: facts_promoted,
        })
        .await;
    }

    Ok((docs_updated, facts_promoted))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_documents_empty_dir() {
        let dir = tempfile::Builder::new()
            .prefix("marrow_docs")
            .tempdir()
            .unwrap();
        let docs = list_documents(dir.path());
        assert!(docs.is_empty());
    }

    #[test]
    fn list_documents_with_existing() {
        let dir = tempfile::Builder::new()
            .prefix("marrow_docs")
            .tempdir()
            .unwrap();
        std::fs::write(dir.path().join("profile.md"), "# Profile\n- Name: Alice").unwrap();
        let docs = list_documents(dir.path());
        assert_eq!(docs.len(), 1);
        assert_eq!(docs[0].0, "profile.md");
        assert!(docs[0].1.contains("Alice"));
    }

    #[test]
    fn load_document_missing() {
        let dir = tempfile::Builder::new()
            .prefix("marrow_docs")
            .tempdir()
            .unwrap();
        let content = load_document(dir.path(), "nonexistent.md");
        assert!(content.is_empty());
    }
}
