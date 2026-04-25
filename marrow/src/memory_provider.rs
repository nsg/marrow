use std::error::Error;

use crate::memory::{Memory, MemoryStore};
use crate::model::{EmbedBackend, ModelBackend};

const SELECTION_PROMPT_TEMPLATE: &str = r#"You are a memory retrieval system. Given a task description and a list of stored facts, decide which facts are relevant context for the task.

Stored facts:
{facts}

Task: {task}

Respond with ONLY a JSON array of fact IDs that are relevant. If no facts are relevant, respond with [].
Example: ["a1b2c3d4-...", "e5f6g7h8-..."]

Your response (JSON array only):"#;

pub async fn select_memories(
    task_description: &str,
    store: &MemoryStore,
    embed_backend: Option<&dyn EmbedBackend>,
    fast_backend: &dyn ModelBackend,
) -> Result<Vec<Memory>, Box<dyn Error + Send + Sync>> {
    // Primary path: vector similarity search
    if let Some(embed) = embed_backend {
        match embed.embed(vec![task_description.to_string()]).await {
            Ok(embeddings) if !embeddings.is_empty() => {
                let query_vec = &embeddings[0];
                let results = store.nearest(query_vec, 10)?;
                let memories: Vec<Memory> = results.into_iter().map(|(m, _)| m).collect();
                if !memories.is_empty() {
                    return Ok(memories);
                }
                // Fall through to model-driven selection if no embeddings in DB yet
            }
            Ok(_) => {}
            Err(e) => {
                eprintln!("[marrow] embedding search failed, falling back to model selection: {e}");
            }
        }
    }

    // Fallback: model-driven selection
    let all_memories = store.list()?;
    if all_memories.is_empty() {
        return Ok(Vec::new());
    }

    let facts_list = all_memories
        .iter()
        .map(|m| format!("- [{}] {}", m.id, m.fact))
        .collect::<Vec<_>>()
        .join("\n");

    let prompt = SELECTION_PROMPT_TEMPLATE
        .replace("{facts}", &facts_list)
        .replace("{task}", task_description);

    let response = fast_backend.complete(prompt).await?;
    let selected_ids = parse_ids(&response);

    let selected: Vec<Memory> = all_memories
        .into_iter()
        .filter(|m| selected_ids.contains(&m.id.to_string()))
        .collect();

    Ok(selected)
}

fn parse_ids(response: &str) -> Vec<String> {
    let trimmed = response.trim();
    let start = trimmed.find('[');
    let end = trimmed.rfind(']');

    match (start, end) {
        (Some(s), Some(e)) if s < e => {
            let json_str = &trimmed[s..=e];
            serde_json::from_str::<Vec<String>>(json_str).unwrap_or_default()
        }
        _ => Vec::new(),
    }
}
