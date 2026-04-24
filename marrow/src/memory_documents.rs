use std::collections::HashSet;
use std::error::Error;
use std::path::Path;

use crate::events::{Event, EventLog};
use crate::janitor::extract_json_block;
use crate::memory::MemoryStore;
use crate::model::ModelBackend;

/// Maximum number of living documents the system will maintain.
const MAX_DOCUMENTS: usize = 12;

const DOCUMENT_PROMPT: &str = r#"You are a memory curator. Your job is to organize individual memory facts into structured living documents.

Below are all current documents and all remaining individual facts. You may update existing documents, create new ones, or mark documents for deletion (e.g. when merging two documents into one).

{documents_section}
## Individual facts (JSON memories)

{facts}

## Instructions

For each document you want to create or update, output its content inside a fenced block:
```document:filename.md
<updated markdown content>
```

Filenames must be short, descriptive, kebab-case (e.g. user-profile.md, home-infrastructure.md, project-notes.md).

Then output a JSON block:
```json
{{"promoted": ["uuid1", "uuid2", ...], "delete_documents": ["old-file.md", ...]}}
```

Rules:
- Only promote a fact if its information is fully captured in a document
- Keep documents concise — use bullet points and sections, not prose
- If a document has no relevant facts to add, omit its block (don't output unchanged docs)
- If a fact doesn't fit any natural grouping, leave it as an individual fact
- Preserve existing document content that is still accurate
- Organize information logically with markdown headers
- Maximum {max_documents} documents — merge related topics rather than creating many small ones
- Use delete_documents to remove a document (e.g. after merging its content into another)
- An empty delete_documents array is fine if nothing needs removal"#;

/// Sanitize a model-chosen filename to safe kebab-case .md.
/// Returns None if the name is invalid or empty after sanitization.
fn sanitize_document_name(raw: &str) -> Option<String> {
    // Strip any directory components (path traversal prevention)
    let base = raw.rsplit('/').next().unwrap_or(raw);
    let base = base.rsplit('\\').next().unwrap_or(base);

    // Strip .md suffix if present, we'll re-add it
    let stem = base.strip_suffix(".md").unwrap_or(base);

    // Keep only alphanumeric and hyphens, collapse multiple hyphens
    let sanitized: String = stem
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();

    // Collapse runs of hyphens, trim leading/trailing hyphens
    let collapsed: String = sanitized
        .split('-')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("-");

    if collapsed.is_empty() {
        return None;
    }

    Some(format!("{collapsed}.md"))
}

/// Extract all ```document:filename blocks from a model response.
/// Returns Vec<(sanitized_filename, content)>.
fn parse_document_blocks(response: &str) -> Vec<(String, String)> {
    let mut docs = Vec::new();
    let mut search_from = 0;

    while search_from < response.len() {
        let rest = &response[search_from..];
        let Some(start) = rest.find("```document:") else {
            break;
        };
        let after_tag = &rest[start + 12..]; // len("```document:") == 12
        let Some(newline) = after_tag.find('\n') else {
            break;
        };
        let raw_name = after_tag[..newline].trim();
        let content_start = &after_tag[newline + 1..];
        let Some(end) = content_start.find("```") else {
            break;
        };
        let content = content_start[..end].trim().to_string();

        if let Some(safe_name) = sanitize_document_name(raw_name)
            && !content.is_empty()
        {
            docs.push((safe_name, content));
        }

        search_from += start + 12 + newline + 1 + end + 3;
    }

    docs
}

/// Returns (filename, content) pairs for all existing document files.
pub fn list_documents(dir: &Path) -> Vec<(String, String)> {
    let mut docs = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return docs;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_some_and(|e| e == "md")
            && let Some(name) = path.file_name().and_then(|n| n.to_str())
        {
            let content = std::fs::read_to_string(&path).unwrap_or_default();
            if !content.is_empty() {
                docs.push((name.to_string(), content));
            }
        }
    }
    docs.sort_by(|a, b| a.0.cmp(&b.0));
    docs
}

/// Generate/update living documents from memory facts.
/// Returns (documents_updated, facts_promoted).
pub async fn generate_documents(
    store: &MemoryStore,
    knowledge_dir: &Path,
    backend: &dyn ModelBackend,
    log: &EventLog,
) -> Result<(u32, u32), Box<dyn Error + Send + Sync>> {
    let memories = store.list()?;
    if memories.is_empty() {
        return Ok((0, 0));
    }

    let dir = knowledge_dir;

    // Build documents section from existing files (dynamic discovery)
    let existing_docs = list_documents(dir);
    let mut documents_section = String::new();
    if existing_docs.is_empty() {
        documents_section.push_str(
            "(no documents yet — create documents as needed to organize the facts below)\n\n",
        );
    } else {
        for (name, content) in &existing_docs {
            documents_section.push_str(&format!("## {name}\nCurrent content:\n{content}\n\n"));
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
        .replace("{facts}", &facts)
        .replace("{max_documents}", &MAX_DOCUMENTS.to_string());

    let response = backend.complete(prompt).await?;

    let mut docs_updated: u32 = 0;
    let mut docs_deleted: u32 = 0;
    let mut facts_promoted: u32 = 0;

    // Parse the JSON block first so we can process deletions before writes
    let json_str = extract_json_block(&response);
    let parsed = serde_json::from_str::<serde_json::Value>(&json_str).ok();

    // Process document deletions first (frees slots for cap enforcement)
    let mut deleted_names: HashSet<String> = HashSet::new();
    if let Some(ref parsed) = parsed
        && let Some(to_delete) = parsed.get("delete_documents").and_then(|v| v.as_array())
    {
        for name_val in to_delete {
            if let Some(raw_name) = name_val.as_str()
                && let Some(safe_name) = sanitize_document_name(raw_name)
            {
                let path = dir.join(&safe_name);
                if path.exists() {
                    if let Err(e) = std::fs::remove_file(&path) {
                        eprintln!("[janitor] document delete error: {e}");
                    } else {
                        deleted_names.insert(safe_name);
                        docs_deleted += 1;
                    }
                }
            }
        }
    }

    // Extract and write document blocks (dynamic filenames)
    let doc_blocks = parse_document_blocks(&response);
    let existing_names: HashSet<String> = existing_docs.iter().map(|(n, _)| n.clone()).collect();
    let mut total_count = existing_names.len() - deleted_names.len();

    for (name, content) in &doc_blocks {
        let is_update = existing_names.contains(name) && !deleted_names.contains(name);
        if !is_update && total_count >= MAX_DOCUMENTS {
            eprintln!("[janitor] document cap reached, skipping new document: {name}");
            continue;
        }
        std::fs::create_dir_all(dir)?;
        std::fs::write(dir.join(name), content)?;
        if !is_update {
            total_count += 1;
        }
        docs_updated += 1;
    }

    // Extract and delete promoted facts
    if let Some(ref parsed) = parsed
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

    if docs_updated > 0 || facts_promoted > 0 || docs_deleted > 0 {
        log.emit(Event::DocumentsUpdated {
            documents: docs_updated,
            promoted: facts_promoted,
            deleted: docs_deleted,
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
    fn list_documents_nonexistent_dir() {
        let docs = list_documents(Path::new("/tmp/marrow_nonexistent_dir_test"));
        assert!(docs.is_empty());
    }

    #[test]
    fn list_documents_discovers_all_md_files() {
        let dir = tempfile::Builder::new()
            .prefix("marrow_docs")
            .tempdir()
            .unwrap();
        std::fs::write(dir.path().join("profile.md"), "# Profile\n- Name: Alice").unwrap();
        std::fs::write(
            dir.path().join("infrastructure.md"),
            "# Infra\n- Server: prod",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("workflows.md"),
            "# Workflows\n- Deploy: manual",
        )
        .unwrap();
        let docs = list_documents(dir.path());
        assert_eq!(docs.len(), 3);
        // Should be sorted by filename
        assert_eq!(docs[0].0, "infrastructure.md");
        assert_eq!(docs[1].0, "profile.md");
        assert_eq!(docs[2].0, "workflows.md");
    }

    #[test]
    fn list_documents_ignores_non_md() {
        let dir = tempfile::Builder::new()
            .prefix("marrow_docs")
            .tempdir()
            .unwrap();
        std::fs::write(dir.path().join("notes.txt"), "some text").unwrap();
        std::fs::write(dir.path().join("data.json"), "{}").unwrap();
        std::fs::write(dir.path().join("profile.md"), "# Profile").unwrap();
        let docs = list_documents(dir.path());
        assert_eq!(docs.len(), 1);
        assert_eq!(docs[0].0, "profile.md");
    }

    #[test]
    fn list_documents_skips_empty() {
        let dir = tempfile::Builder::new()
            .prefix("marrow_docs")
            .tempdir()
            .unwrap();
        std::fs::write(dir.path().join("empty.md"), "").unwrap();
        std::fs::write(dir.path().join("real.md"), "# Content").unwrap();
        let docs = list_documents(dir.path());
        assert_eq!(docs.len(), 1);
        assert_eq!(docs[0].0, "real.md");
    }

    #[test]
    fn sanitize_document_name_basic() {
        assert_eq!(
            sanitize_document_name("user-profile.md"),
            Some("user-profile.md".to_string())
        );
    }

    #[test]
    fn sanitize_document_name_adds_extension() {
        assert_eq!(
            sanitize_document_name("user-profile"),
            Some("user-profile.md".to_string())
        );
    }

    #[test]
    fn sanitize_document_name_normalizes() {
        assert_eq!(
            sanitize_document_name("My User Profile.md"),
            Some("my-user-profile.md".to_string())
        );
    }

    #[test]
    fn sanitize_document_name_strips_traversal() {
        assert_eq!(
            sanitize_document_name("../../etc/passwd.md"),
            Some("passwd.md".to_string())
        );
        assert_eq!(
            sanitize_document_name("..\\..\\windows\\system.md"),
            Some("system.md".to_string())
        );
    }

    #[test]
    fn sanitize_document_name_empty() {
        assert_eq!(sanitize_document_name(""), None);
        assert_eq!(sanitize_document_name("..."), None);
        assert_eq!(sanitize_document_name(".md"), None);
    }

    #[test]
    fn sanitize_document_name_collapses_hyphens() {
        assert_eq!(
            sanitize_document_name("a---b---c.md"),
            Some("a-b-c.md".to_string())
        );
    }

    #[test]
    fn parse_document_blocks_basic() {
        let response = r#"Here are the updated documents:

```document:user-profile.md
# Profile
- Name: Alice
```

```document:infrastructure.md
# Infrastructure
- Server: prod-1
```

```json
{"promoted": ["abc-123"], "delete_documents": []}
```
"#;
        let blocks = parse_document_blocks(response);
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0].0, "user-profile.md");
        assert!(blocks[0].1.contains("Alice"));
        assert_eq!(blocks[1].0, "infrastructure.md");
        assert!(blocks[1].1.contains("prod-1"));
    }

    #[test]
    fn parse_document_blocks_empty() {
        let response = "No documents to create.\n```json\n{\"promoted\": []}\n```";
        let blocks = parse_document_blocks(response);
        assert!(blocks.is_empty());
    }

    #[test]
    fn parse_document_blocks_sanitizes_names() {
        let response = "```document:../../etc/passwd.md\n# Hacked\n```\n";
        let blocks = parse_document_blocks(response);
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].0, "passwd.md");
    }

    #[test]
    fn parse_document_blocks_skips_empty_content() {
        let response = "```document:empty.md\n\n```\n";
        let blocks = parse_document_blocks(response);
        assert!(blocks.is_empty());
    }
}
