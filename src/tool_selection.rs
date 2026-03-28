use std::collections::HashMap;
use std::error::Error;

use crate::context::Stage;
use crate::model::ModelBackend;
use crate::session::Message;
use crate::toolbox::ToolMeta;

const SELECTION_PROMPT_TEMPLATE: &str = r#"You are a tool selection system. Given a task description, conversation history, and a list of available tools, decide which tools are needed, what parameters each tool needs, and whether tools need to run in stages (where later tools depend on earlier tool outputs).

IMPORTANT: If the task can be answered from conversation history alone (follow-up questions, chitchat, references to earlier messages), respond with empty stages.

Available tools:
{tools}

{history}Task: {task}

Respond with ONLY a JSON object. Each stage runs its tools in parallel. Later stages receive earlier stage outputs via RESULTS["tool_name"]. Each tool gets its own params.

Example with dependencies (weather first, then planner uses weather output):
{{"stages": [{{"tools": {{"weather": {{"LOCATION": "Portland"}}}}}}, {{"tools": {{"weekend_planner": {{}}}}}}]}}

Example with parallel tools (no dependencies):
{{"stages": [{{"tools": {{"weather": {{"LOCATION": "Tokyo"}}, "calendar": {{"DATE": "2026-03-28"}}}}}}]}}

If no tools are needed:
{{"stages": []}}

Your response (JSON only):"#;

#[derive(Debug)]
pub struct SelectionResult {
    pub stages: Vec<Stage>,
}

impl SelectionResult {
    pub fn is_empty(&self) -> bool {
        self.stages.is_empty() || self.stages.iter().all(|s| s.tools.is_empty())
    }

    pub fn all_tool_names(&self) -> Vec<String> {
        self.stages
            .iter()
            .flat_map(|s| s.tools.keys().cloned())
            .collect()
    }
}

pub fn build_selection_prompt(
    task_description: &str,
    tools: &[ToolMeta],
    history: Option<&[Message]>,
) -> String {
    let tools_list = if tools.is_empty() {
        "(none available)".to_string()
    } else {
        tools
            .iter()
            .map(|t| format!("- {}: {}", t.name, t.description))
            .collect::<Vec<_>>()
            .join("\n")
    };

    let history_section = if let Some(msgs) = history {
        if msgs.is_empty() {
            String::new()
        } else {
            let conversation = msgs
                .iter()
                .map(|m| format!("{}: {}", m.role, m.content))
                .collect::<Vec<_>>()
                .join("\n");
            format!("Conversation history:\n{conversation}\n\n")
        }
    } else {
        String::new()
    };

    SELECTION_PROMPT_TEMPLATE
        .replace("{tools}", &tools_list)
        .replace("{history}", &history_section)
        .replace("{task}", task_description)
}

pub async fn select_tools(
    task_description: &str,
    tools: &[ToolMeta],
    backend: &dyn ModelBackend,
    history: Option<&[Message]>,
) -> Result<SelectionResult, Box<dyn Error + Send + Sync>> {
    if tools.is_empty() {
        return Ok(SelectionResult {
            stages: Vec::new(),
        });
    }

    let prompt = build_selection_prompt(task_description, tools, history);
    let response = backend.complete(prompt).await?;

    parse_selection(&response)
}

fn parse_selection(response: &str) -> Result<SelectionResult, Box<dyn Error + Send + Sync>> {
    let trimmed = response.trim();

    let start = trimmed.find('{');
    let end = trimmed.rfind('}');

    match (start, end) {
        (Some(s), Some(e)) if s < e => {
            let json_str = &trimmed[s..=e];

            // Try staged format first
            if let Ok(staged) = parse_staged(json_str) {
                return Ok(staged);
            }

            // Fall back to legacy flat format
            parse_legacy(json_str)
        }
        _ => Ok(SelectionResult {
            stages: Vec::new(),
        }),
    }
}

fn parse_staged(json_str: &str) -> Result<SelectionResult, Box<dyn Error + Send + Sync>> {
    #[derive(serde::Deserialize)]
    struct RawStaged {
        stages: Vec<RawStage>,
    }

    #[derive(serde::Deserialize)]
    struct RawStage {
        tools: HashMap<String, HashMap<String, serde_json::Value>>,
    }

    let raw: RawStaged = serde_json::from_str(json_str)?;

    let stages = raw
        .stages
        .into_iter()
        .map(|s| Stage {
            tools: s
                .tools
                .into_iter()
                .map(|(name, params)| {
                    let string_params = params
                        .into_iter()
                        .map(|(k, v)| {
                            let s = match v {
                                serde_json::Value::String(s) => s,
                                other => other.to_string(),
                            };
                            (k, s)
                        })
                        .collect();
                    (name, string_params)
                })
                .collect(),
        })
        .collect();

    Ok(SelectionResult { stages })
}

fn parse_legacy(json_str: &str) -> Result<SelectionResult, Box<dyn Error + Send + Sync>> {
    #[derive(serde::Deserialize)]
    struct RawSelection {
        #[serde(default)]
        tools: Vec<String>,
        #[serde(default)]
        params: HashMap<String, serde_json::Value>,
    }

    let raw: RawSelection = serde_json::from_str(json_str).unwrap_or(RawSelection {
        tools: Vec::new(),
        params: HashMap::new(),
    });

    if raw.tools.is_empty() {
        return Ok(SelectionResult {
            stages: Vec::new(),
        });
    }

    // Convert shared params to per-tool params (all tools get the same params)
    let shared_params: HashMap<String, String> = raw
        .params
        .into_iter()
        .map(|(k, v)| {
            let s = match v {
                serde_json::Value::String(s) => s,
                other => other.to_string(),
            };
            (k, s)
        })
        .collect();

    let tools = raw
        .tools
        .into_iter()
        .map(|name| (name, shared_params.clone()))
        .collect();

    Ok(SelectionResult {
        stages: vec![Stage { tools }],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_staged_single_stage() {
        let r = parse_selection(
            r#"{"stages": [{"tools": {"weather": {"LOCATION": "Tokyo"}}}]}"#,
        )
        .unwrap();
        assert_eq!(r.stages.len(), 1);
        assert!(r.stages[0].tools.contains_key("weather"));
        assert_eq!(r.stages[0].tools["weather"]["LOCATION"], "Tokyo");
    }

    #[test]
    fn parse_staged_multi_stage() {
        let r = parse_selection(
            r#"{"stages": [{"tools": {"weather": {"LOCATION": "Portland"}, "calendar": {"DATE": "today"}}}, {"tools": {"planner": {}}}]}"#,
        )
        .unwrap();
        assert_eq!(r.stages.len(), 2);
        assert_eq!(r.stages[0].tools.len(), 2);
        assert_eq!(r.stages[1].tools.len(), 1);
        assert!(r.stages[1].tools.contains_key("planner"));
    }

    #[test]
    fn parse_staged_empty() {
        let r = parse_selection(r#"{"stages": []}"#).unwrap();
        assert!(r.is_empty());
    }

    #[test]
    fn parse_legacy_flat_format() {
        let r = parse_selection(
            r#"{"tools": ["weather"], "params": {"LOCATION": "Tokyo"}}"#,
        )
        .unwrap();
        assert_eq!(r.stages.len(), 1);
        assert!(r.stages[0].tools.contains_key("weather"));
        assert_eq!(r.stages[0].tools["weather"]["LOCATION"], "Tokyo");
    }

    #[test]
    fn parse_legacy_empty_tools() {
        let r = parse_selection(r#"{"tools": [], "params": {}}"#).unwrap();
        assert!(r.is_empty());
    }

    #[test]
    fn parse_legacy_shared_params_applied_to_all() {
        let r = parse_selection(
            r#"{"tools": ["weather", "time"], "params": {"LOCATION": "Paris"}}"#,
        )
        .unwrap();
        assert_eq!(r.stages.len(), 1);
        assert_eq!(r.stages[0].tools["weather"]["LOCATION"], "Paris");
        assert_eq!(r.stages[0].tools["time"]["LOCATION"], "Paris");
    }

    #[test]
    fn parse_no_json() {
        let r = parse_selection("I don't know what tools to use").unwrap();
        assert!(r.is_empty());
    }

    #[test]
    fn parse_json_with_surrounding_text() {
        let r = parse_selection(
            r#"Here: {"stages": [{"tools": {"time": {"TIMEZONE": "UTC"}}}]} done"#,
        )
        .unwrap();
        assert_eq!(r.stages.len(), 1);
        assert_eq!(r.stages[0].tools["time"]["TIMEZONE"], "UTC");
    }

    #[test]
    fn parse_numeric_param_converted_to_string() {
        let r = parse_selection(
            r#"{"stages": [{"tools": {"test": {"COUNT": 5}}}]}"#,
        )
        .unwrap();
        assert_eq!(r.stages[0].tools["test"]["COUNT"], "5");
    }

    #[test]
    fn all_tool_names_across_stages() {
        let r = parse_selection(
            r#"{"stages": [{"tools": {"a": {}, "b": {}}}, {"tools": {"c": {}}}]}"#,
        )
        .unwrap();
        let mut names = r.all_tool_names();
        names.sort();
        assert_eq!(names, vec!["a", "b", "c"]);
    }

    #[test]
    fn is_empty_checks_tools_not_just_stages() {
        let r = parse_selection(r#"{"stages": [{"tools": {}}]}"#).unwrap();
        assert!(r.is_empty());
    }
}
