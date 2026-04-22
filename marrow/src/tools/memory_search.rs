use std::collections::HashMap;

use crate::tool::{ExecuteResult, ParamDef, Tool, ToolContext};
use crate::toolbox::ToolMeta;

pub struct MemorySearchTool;

impl Tool for MemorySearchTool {
    fn meta(&self) -> ToolMeta {
        ToolMeta {
            name: "memory_search".to_string(),
            description: "Searches stored facts by keyword".to_string(),
            provides: vec!["memory_search".to_string()],
            validated: true,
        }
    }

    fn params(&self) -> Vec<ParamDef> {
        vec![ParamDef::required("QUERY"), ParamDef::optional("OFFSET")]
    }

    fn returns(&self) -> Vec<String> {
        vec![
            "results".to_string(),
            "total".to_string(),
            "offset".to_string(),
        ]
    }

    fn execute(&self, params: HashMap<String, String>, ctx: ToolContext) -> ExecuteResult<'_> {
        Box::pin(async move {
            let store = match &ctx.memory_store {
                Some(s) => s,
                None => {
                    return Ok(serde_json::json!({"error": "memory store not available"}));
                }
            };

            let query = match params.get("QUERY") {
                Some(q) if !q.is_empty() => q,
                _ => {
                    return Ok(serde_json::json!({"error": "missing required parameter: QUERY"}));
                }
            };

            let offset: usize = match params.get("OFFSET") {
                Some(s) if !s.is_empty() => match s.parse() {
                    Ok(n) => n,
                    Err(_) => {
                        return Ok(serde_json::json!({"error": format!("invalid OFFSET: {s}")}));
                    }
                },
                _ => 0,
            };

            let all_memories = match store.list() {
                Ok(mems) => mems,
                Err(e) => {
                    return Ok(
                        serde_json::json!({"error": format!("failed to list memories: {e}")}),
                    );
                }
            };

            let query_lower = query.to_lowercase();
            let matches: Vec<_> = all_memories
                .into_iter()
                .filter(|m| m.fact.to_lowercase().contains(&query_lower))
                .collect();

            let total = matches.len();
            let results: Vec<serde_json::Value> = matches
                .into_iter()
                .skip(offset)
                .take(20)
                .map(|m| {
                    serde_json::json!({
                        "id": m.id.to_string(),
                        "fact": m.fact,
                        "created": m.created,
                    })
                })
                .collect();

            Ok(serde_json::json!({
                "results": results,
                "total": total,
                "offset": offset,
            }))
        })
    }
}
