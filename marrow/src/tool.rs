use std::collections::HashMap;
use std::error::Error;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;

use reqwest::Client;

use crate::schedule::ScheduleStore;
use crate::secrets::Secrets;
use crate::toolbox::{ToolMeta, Toolbox};

type BoxError = Box<dyn Error + Send + Sync>;

pub type ExecuteResult<'a> =
    Pin<Box<dyn Future<Output = Result<serde_json::Value, BoxError>> + Send + 'a>>;

#[derive(Debug, Clone)]
pub struct ParamDef {
    pub name: String,
    pub required: bool,
}

impl ParamDef {
    pub fn required(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            required: true,
        }
    }

    pub fn optional(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            required: false,
        }
    }
}

#[derive(Debug, Clone)]
pub struct FrontendContext {
    pub frontend: String,
    pub channel_id: Option<u64>,
}

#[derive(Clone)]
pub struct ToolContext {
    pub client: Arc<Client>,
    pub secrets: Arc<Secrets>,
    pub task_description: String,
    pub schedule_store: Option<Arc<ScheduleStore>>,
    pub frontend_context: Option<FrontendContext>,
}

pub trait Tool: Send + Sync {
    fn meta(&self) -> ToolMeta;
    fn params(&self) -> Vec<ParamDef>;
    fn returns(&self) -> Vec<String>;
    fn execute(&self, params: HashMap<String, String>, ctx: ToolContext) -> ExecuteResult<'_>;
}

pub struct ToolInfo {
    pub name: String,
    pub description: String,
    pub params: Vec<String>,
    pub returns: Vec<String>,
    pub builtin: bool,
}

impl ToolInfo {
    pub fn usage_line(&self) -> String {
        let marker = if self.builtin { " [built-in]" } else { "" };
        let mut line = format!("- {}: {}{marker}", self.name, self.description);
        if !self.params.is_empty() {
            line.push_str(&format!(" (params: {})", self.params.join(", ")));
        }
        if !self.returns.is_empty() {
            line.push_str(&format!(" (returns: {})", self.returns.join(", ")));
        }
        line
    }
}

/// Unified view of all tools: built-in Rust tools + Lua toolbox.
pub struct ToolRegistry {
    builtins: HashMap<String, Arc<dyn Tool>>,
    toolbox: Toolbox,
    toolbox_path: PathBuf,
}

impl ToolRegistry {
    pub fn new(toolbox: Toolbox, toolbox_path: impl Into<PathBuf>) -> Self {
        Self {
            builtins: HashMap::new(),
            toolbox,
            toolbox_path: toolbox_path.into(),
        }
    }

    pub fn register(&mut self, tool: impl Tool + 'static) {
        let name = tool.meta().name;
        self.builtins.insert(name, Arc::new(tool));
    }

    pub fn list_all(&self) -> Vec<ToolInfo> {
        let mut tools = Vec::new();

        for tool in self.builtins.values() {
            let meta = tool.meta();
            tools.push(ToolInfo {
                name: meta.name,
                description: meta.description,
                params: tool.params().iter().map(|p| p.name.clone()).collect(),
                returns: tool.returns(),
                builtin: true,
            });
        }

        if let Ok(lua_tools) = self.toolbox.list_tools() {
            for meta in &lua_tools {
                if self.builtins.contains_key(&meta.name) {
                    continue;
                }
                tools.push(ToolInfo {
                    name: meta.name.clone(),
                    description: meta.description.clone(),
                    params: self.toolbox.extract_params(&meta.name),
                    returns: self.toolbox.extract_return_fields(&meta.name),
                    builtin: false,
                });
            }
        }

        tools
    }

    pub async fn execute_tool(
        &self,
        name: &str,
        params: &HashMap<String, String>,
        ctx: &ToolContext,
    ) -> Result<serde_json::Value, BoxError> {
        // Resolve secret: prefixed param values before dispatching
        let resolved = ctx.secrets.resolve_params(params);

        if let Some(tool) = self.builtins.get(name) {
            return tool.execute(resolved, ctx.clone()).await;
        }

        let provider = self.toolbox.load_provider(name)?;
        provider
            .execute_with_params(
                &ctx.task_description,
                ctx.client.clone(),
                &resolved,
                Some(self.toolbox_path.clone()),
                Some(ctx.secrets.as_ref()),
                self.builtins_arc(),
            )
            .await
    }

    pub fn get_builtin(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.builtins.get(name).cloned()
    }

    pub fn builtins_arc(&self) -> Arc<HashMap<String, Arc<dyn Tool>>> {
        Arc::new(self.builtins.clone())
    }

    pub fn toolbox(&self) -> &Toolbox {
        &self.toolbox
    }

    pub fn toolbox_path(&self) -> &Path {
        &self.toolbox_path
    }

    pub fn builtin_info(&self) -> Vec<crate::janitor::BuiltinInfo> {
        self.builtins
            .values()
            .map(|t| {
                let meta = t.meta();
                crate::janitor::BuiltinInfo {
                    name: meta.name,
                    description: meta.description,
                }
            })
            .collect()
    }

    pub fn extract_params(&self, name: &str) -> Vec<String> {
        if let Some(tool) = self.builtins.get(name) {
            tool.params().iter().map(|p| p.name.clone()).collect()
        } else {
            self.toolbox.extract_params(name)
        }
    }
}

/// Execute a built-in tool by name (used by sandbox_host's run_tool).
pub async fn execute_builtin(
    tool: &dyn Tool,
    params: Option<&HashMap<String, String>>,
    client: Arc<Client>,
    secrets: Arc<Secrets>,
    task_description: &str,
) -> Result<serde_json::Value, BoxError> {
    let ctx = ToolContext {
        client,
        secrets,
        task_description: task_description.to_string(),
        schedule_store: None,
        frontend_context: None,
    };
    let params = params.cloned().unwrap_or_default();
    tool.execute(params, ctx).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::toolbox::ToolMeta;

    struct DummyTool;

    impl Tool for DummyTool {
        fn meta(&self) -> ToolMeta {
            ToolMeta {
                name: "dummy".to_string(),
                description: "A dummy test tool".to_string(),
                provides: vec!["dummy".to_string()],
                validated: true,
            }
        }

        fn params(&self) -> Vec<ParamDef> {
            vec![ParamDef::required("input")]
        }

        fn returns(&self) -> Vec<String> {
            vec!["output".to_string()]
        }

        fn execute(&self, params: HashMap<String, String>, _ctx: ToolContext) -> ExecuteResult<'_> {
            Box::pin(async move {
                let input = params.get("input").cloned().unwrap_or_default();
                Ok(serde_json::json!({ "output": input }))
            })
        }
    }

    #[test]
    fn registry_lists_builtin_tools() {
        let dir = tempfile::Builder::new()
            .prefix("marrow_reg")
            .tempdir()
            .unwrap();
        let toolbox = Toolbox::new(dir.path());
        let mut registry = ToolRegistry::new(toolbox, dir.path());
        registry.register(DummyTool);

        let tools = registry.list_all();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "dummy");
        assert!(tools[0].builtin);
    }

    #[test]
    fn registry_merges_builtin_and_lua() {
        let dir = tempfile::Builder::new()
            .prefix("marrow_reg")
            .tempdir()
            .unwrap();
        let toolbox = Toolbox::new(dir.path());
        toolbox
            .save_tool(
                &ToolMeta {
                    name: "lua_tool".to_string(),
                    description: "A lua tool".to_string(),
                    provides: vec![],
                    validated: true,
                },
                r#"return { ok = true }"#,
            )
            .unwrap();

        let mut registry = ToolRegistry::new(toolbox, dir.path());
        registry.register(DummyTool);

        let tools = registry.list_all();
        assert_eq!(tools.len(), 2);
        let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
        assert!(names.contains(&"dummy"));
        assert!(names.contains(&"lua_tool"));
    }

    #[test]
    fn builtin_shadows_lua_with_same_name() {
        let dir = tempfile::Builder::new()
            .prefix("marrow_reg")
            .tempdir()
            .unwrap();
        let toolbox = Toolbox::new(dir.path());
        toolbox
            .save_tool(
                &ToolMeta {
                    name: "dummy".to_string(),
                    description: "Lua version".to_string(),
                    provides: vec![],
                    validated: true,
                },
                r#"return { ok = true }"#,
            )
            .unwrap();

        let mut registry = ToolRegistry::new(toolbox, dir.path());
        registry.register(DummyTool);

        let tools = registry.list_all();
        assert_eq!(tools.len(), 1);
        assert!(tools[0].builtin);
        assert_eq!(tools[0].description, "A dummy test tool");
    }

    #[test]
    fn tool_info_usage_line_formatting() {
        let info = ToolInfo {
            name: "rss".to_string(),
            description: "Fetch RSS feeds".to_string(),
            params: vec!["url".to_string(), "topic".to_string()],
            returns: vec!["items".to_string()],
            builtin: true,
        };
        let line = info.usage_line();
        assert!(line.contains("[built-in]"));
        assert!(line.contains("params: url, topic"));
        assert!(line.contains("returns: items"));
    }

    #[tokio::test]
    async fn execute_builtin_tool() {
        let dir = tempfile::Builder::new()
            .prefix("marrow_reg")
            .tempdir()
            .unwrap();
        let toolbox = Toolbox::new(dir.path());
        let mut registry = ToolRegistry::new(toolbox, dir.path());
        registry.register(DummyTool);

        let ctx = ToolContext {
            client: Arc::new(Client::new()),
            secrets: Arc::new(Secrets::default()),
            task_description: "test".to_string(),
            schedule_store: None,
            frontend_context: None,
        };
        let mut params = HashMap::new();
        params.insert("input".to_string(), "hello".to_string());

        let result = registry.execute_tool("dummy", &params, &ctx).await.unwrap();
        assert_eq!(result["output"], "hello");
    }
}
