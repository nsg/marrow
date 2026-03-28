use std::collections::HashMap;
use std::sync::Arc;

use marrow::context::{ContextAssembler, LuaProvider, Stage};
use marrow::events::{Event, EventLog};
use marrow::executor::Context;
use marrow::memory::{Memory, MemorySource, MemoryStore};
use marrow::memory_provider;
use marrow::memory_writer;
use marrow::model::{CompletionResult, ModelBackend};
use marrow::registry::TaskRegistry;
use marrow::session::{ChatSession, Message};
use marrow::task::Task;
use marrow::answer_check;
use marrow::tool_selection::{self, PriorAttempt};
use marrow::toolbox::{ToolMeta, Toolbox};
use marrow::triage;

use reqwest::Client;
use tokio::sync::Mutex;

// ---------------------------------------------------------------------------
// MockBackend — returns canned responses in order
// ---------------------------------------------------------------------------

struct MockBackend {
    responses: Mutex<Vec<String>>,
}

impl MockBackend {
    fn new(responses: Vec<&str>) -> Self {
        Self {
            responses: Mutex::new(responses.into_iter().map(String::from).collect()),
        }
    }
}

impl ModelBackend for MockBackend {
    fn complete(&self, _prompt: String) -> CompletionResult<'_> {
        Box::pin(async {
            let mut queue = self.responses.lock().await;
            if queue.is_empty() {
                panic!("MockBackend: no more responses queued");
            }
            Ok(queue.remove(0))
        })
    }

    fn complete_chat(&self, _messages: Vec<Message>) -> CompletionResult<'_> {
        Box::pin(async {
            let mut queue = self.responses.lock().await;
            if queue.is_empty() {
                panic!("MockBackend: no more responses queued");
            }
            Ok(queue.remove(0))
        })
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn temp_dir(name: &str) -> tempfile::TempDir {
    tempfile::Builder::new().prefix(name).tempdir().unwrap()
}

async fn noop_log() -> EventLog {
    EventLog::new(None, false).await.unwrap()
}

fn single_stage(tools: Vec<(&str, Vec<(&str, &str)>)>) -> Vec<Stage> {
    let mut map = HashMap::new();
    for (name, params) in tools {
        let p: HashMap<String, String> = params
            .into_iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        map.insert(name.to_string(), p);
    }
    vec![Stage { tools: map }]
}

// ---------------------------------------------------------------------------
// Triage tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn triage_says_no_for_greeting() {
    let backend = MockBackend::new(vec!["NO"]);
    let result = triage::needs_external_data("hello", &backend, None, &[]).await.unwrap();
    assert!(!result);
}

#[tokio::test]
async fn triage_says_yes_for_weather() {
    let backend = MockBackend::new(vec!["YES"]);
    let result = triage::needs_external_data("what's the weather?", &backend, None, &[])
        .await
        .unwrap();
    assert!(result);
}

#[tokio::test]
async fn triage_considers_memories() {
    let mem = Memory::new("User lives in Tokyo", MemorySource::User);
    let backend = MockBackend::new(vec!["NO"]);
    let result = triage::needs_external_data("where do I live?", &backend, None, &[mem])
        .await
        .unwrap();
    assert!(!result);
}

#[tokio::test]
async fn triage_considers_history() {
    let history = vec![
        Message::user("the capital of France is Paris"),
        Message::assistant("noted"),
    ];
    let backend = MockBackend::new(vec!["NO"]);
    let result = triage::needs_external_data(
        "what did I just say?",
        &backend,
        Some(&history),
        &[],
    )
    .await
    .unwrap();
    assert!(!result);
}

// ---------------------------------------------------------------------------
// Tool selection tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn tool_selection_empty_toolbox_returns_empty() {
    let backend = MockBackend::new(vec![]);
    let result = tool_selection::select_tools("anything", &[], &backend, None)
        .await
        .unwrap();
    assert!(result.is_empty());
}

#[tokio::test]
async fn tool_selection_picks_matching_tool_staged() {
    let tools = vec![ToolMeta {
        name: "weather".to_string(),
        description: "Get weather for a location".to_string(),
        provides: vec!["weather".to_string()],
        validated: true,
    }];

    let backend = MockBackend::new(vec![
        r#"{"stages": [{"tools": {"weather": {"LOCATION": "Tokyo"}}}]}"#,
    ]);

    let result = tool_selection::select_tools("weather in Tokyo", &tools, &backend, None)
        .await
        .unwrap();
    assert_eq!(result.stages.len(), 1);
    assert!(result.stages[0].tools.contains_key("weather"));
    assert_eq!(result.stages[0].tools["weather"]["LOCATION"], "Tokyo");
}

#[tokio::test]
async fn tool_selection_multi_stage() {
    let tools = vec![
        ToolMeta {
            name: "weather".to_string(),
            description: "Get weather".to_string(),
            provides: vec!["weather".to_string()],
            validated: true,
        },
        ToolMeta {
            name: "planner".to_string(),
            description: "Plan activities".to_string(),
            provides: vec!["planner".to_string()],
            validated: true,
        },
    ];

    let backend = MockBackend::new(vec![
        r#"{"stages": [{"tools": {"weather": {"LOCATION": "Portland"}}}, {"tools": {"planner": {}}}]}"#,
    ]);

    let result = tool_selection::select_tools("plan my weekend", &tools, &backend, None)
        .await
        .unwrap();
    assert_eq!(result.stages.len(), 2);
    assert!(result.stages[0].tools.contains_key("weather"));
    assert!(result.stages[1].tools.contains_key("planner"));
}

#[tokio::test]
async fn tool_selection_legacy_format_compat() {
    let tools = vec![ToolMeta {
        name: "weather".to_string(),
        description: "Get weather".to_string(),
        provides: vec!["weather".to_string()],
        validated: true,
    }];

    let backend = MockBackend::new(vec![
        r#"{"tools": ["weather"], "params": {"LOCATION": "Tokyo"}}"#,
    ]);

    let result = tool_selection::select_tools("weather", &tools, &backend, None)
        .await
        .unwrap();
    assert_eq!(result.stages.len(), 1);
    assert_eq!(result.stages[0].tools["weather"]["LOCATION"], "Tokyo");
}

#[tokio::test]
async fn tool_selection_returns_empty_when_no_match() {
    let tools = vec![ToolMeta {
        name: "weather".to_string(),
        description: "Get weather".to_string(),
        provides: vec!["weather".to_string()],
        validated: true,
    }];

    let backend = MockBackend::new(vec![r#"{"stages": []}"#]);

    let result = tool_selection::select_tools("tell me a joke", &tools, &backend, None)
        .await
        .unwrap();
    assert!(result.is_empty());
}

#[tokio::test]
async fn tool_selection_all_tool_names() {
    let tools = vec![
        ToolMeta {
            name: "a".to_string(),
            description: "A".to_string(),
            provides: vec![],
            validated: true,
        },
        ToolMeta {
            name: "b".to_string(),
            description: "B".to_string(),
            provides: vec![],
            validated: true,
        },
    ];

    let backend = MockBackend::new(vec![
        r#"{"stages": [{"tools": {"a": {}}}, {"tools": {"b": {}}}]}"#,
    ]);

    let result = tool_selection::select_tools("test", &tools, &backend, None)
        .await
        .unwrap();
    let mut names = result.all_tool_names();
    names.sort();
    assert_eq!(names, vec!["a", "b"]);
}

// ---------------------------------------------------------------------------
// Lua sandbox + context assembly tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn lua_provider_returns_table() {
    let provider = LuaProvider::new("test", r#"return { greeting = "hello" }"#);
    let client = Arc::new(Client::new());
    let result = provider.execute("test task", client).await.unwrap();
    assert_eq!(result["greeting"], "hello");
}

#[tokio::test]
async fn lua_provider_receives_params_table() {
    let provider = LuaProvider::new("test", r#"return { city = PARAMS["LOCATION"] }"#);
    let client = Arc::new(Client::new());
    let mut params = HashMap::new();
    params.insert("LOCATION".to_string(), "Paris".to_string());
    let result = provider
        .execute_with_params("test", client, &params, &HashMap::new())
        .await
        .unwrap();
    assert_eq!(result["city"], "Paris");
}

#[tokio::test]
async fn lua_provider_receives_task_table() {
    let provider = LuaProvider::new("test", "return { desc = TASK.description }");
    let client = Arc::new(Client::new());
    let result = provider.execute("my task", client).await.unwrap();
    assert_eq!(result["desc"], "my task");
}

#[tokio::test]
async fn lua_provider_receives_results_table() {
    let provider = LuaProvider::new(
        "test",
        r#"local w = json_parse(RESULTS["weather"]); return { temp = w.temp }"#,
    );
    let client = Arc::new(Client::new());
    let mut results = HashMap::new();
    results.insert("weather".to_string(), r#"{"temp": 22}"#.to_string());
    let result = provider
        .execute_with_params("test", client, &HashMap::new(), &results)
        .await
        .unwrap();
    assert_eq!(result["temp"], 22);
}

#[tokio::test]
async fn lua_provider_empty_results_table() {
    // RESULTS should be an empty table when no prior stages
    let provider = LuaProvider::new("test", "return { count = #RESULTS }");
    let client = Arc::new(Client::new());
    let result = provider.execute("test", client).await.unwrap();
    // # on a table with string keys is 0
    assert_eq!(result["count"], 0);
}

#[tokio::test]
async fn lua_sandbox_blocks_unsafe_globals() {
    let provider = LuaProvider::new("test", "return { has_os = (os ~= nil) }");
    let client = Arc::new(Client::new());
    let result = provider.execute("test", client).await.unwrap();
    assert_eq!(result["has_os"], false);
}

#[tokio::test]
async fn lua_sandbox_blocks_io() {
    let provider = LuaProvider::new("test", "return { has_io = (io ~= nil) }");
    let client = Arc::new(Client::new());
    let result = provider.execute("test", client).await.unwrap();
    assert_eq!(result["has_io"], false);
}

#[tokio::test]
async fn lua_sandbox_blocks_require() {
    let provider = LuaProvider::new("test", "return { has_require = (require ~= nil) }");
    let client = Arc::new(Client::new());
    let result = provider.execute("test", client).await.unwrap();
    assert_eq!(result["has_require"], false);
}

#[tokio::test]
async fn context_assembler_collects_providers() {
    let client = Arc::new(Client::new());
    let mut assembler = ContextAssembler::new(client);
    assembler.add_provider(LuaProvider::new("a", "return { x = 1 }"));
    assembler.add_provider(LuaProvider::new("b", "return { y = 2 }"));

    let stages = single_stage(vec![("a", vec![]), ("b", vec![])]);
    let ctx = assembler.assemble("test", &stages).await.unwrap();

    assert_eq!(ctx.data["a"]["x"], 1);
    assert_eq!(ctx.data["b"]["y"], 2);
}

#[tokio::test]
async fn context_assembler_handles_failing_provider() {
    let client = Arc::new(Client::new());
    let mut assembler = ContextAssembler::new(client);
    assembler.add_provider(LuaProvider::new("good", "return { ok = true }"));
    assembler.add_provider(LuaProvider::new("bad", "error('boom')"));

    let stages = single_stage(vec![("good", vec![]), ("bad", vec![])]);
    let ctx = assembler.assemble("test", &stages).await.unwrap();

    assert_eq!(ctx.data["good"]["ok"], true);
    assert!(ctx.data["bad"]["error"].as_str().unwrap().contains("boom"));
}

#[tokio::test]
async fn context_assembler_per_tool_params() {
    let client = Arc::new(Client::new());
    let mut assembler = ContextAssembler::new(client);
    assembler.add_provider(LuaProvider::new("a", r#"return { loc = PARAMS["LOCATION"] }"#));
    assembler.add_provider(LuaProvider::new("b", r#"return { tz = PARAMS["TIMEZONE"] }"#));

    let mut tools = HashMap::new();
    let mut a_params = HashMap::new();
    a_params.insert("LOCATION".to_string(), "Berlin".to_string());
    tools.insert("a".to_string(), a_params);

    let mut b_params = HashMap::new();
    b_params.insert("TIMEZONE".to_string(), "CET".to_string());
    tools.insert("b".to_string(), b_params);

    let stages = vec![Stage { tools }];
    let ctx = assembler.assemble("test", &stages).await.unwrap();

    assert_eq!(ctx.data["a"]["loc"], "Berlin");
    assert_eq!(ctx.data["b"]["tz"], "CET");
}

#[tokio::test]
async fn context_assembler_staged_results_passed() {
    let client = Arc::new(Client::new());
    let mut assembler = ContextAssembler::new(client);

    // Stage 1: weather returns data
    assembler.add_provider(LuaProvider::new(
        "weather",
        r#"return { temp = 22, condition = "sunny" }"#,
    ));

    // Stage 2: planner reads weather result
    assembler.add_provider(LuaProvider::new(
        "planner",
        r#"local w = json_parse(RESULTS["weather"]); return { plan = "outdoor, " .. w.condition }"#,
    ));

    let stage1 = Stage {
        tools: {
            let mut m = HashMap::new();
            m.insert("weather".to_string(), HashMap::new());
            m
        },
    };
    let stage2 = Stage {
        tools: {
            let mut m = HashMap::new();
            m.insert("planner".to_string(), HashMap::new());
            m
        },
    };

    let ctx = assembler.assemble("plan my weekend", &[stage1, stage2]).await.unwrap();

    assert_eq!(ctx.data["weather"]["temp"], 22);
    assert_eq!(ctx.data["planner"]["plan"], "outdoor, sunny");
}

// ---------------------------------------------------------------------------
// Memory store round-trip tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn memory_store_save_load_list() {
    let dir = temp_dir("marrow_mem");
    let store = MemoryStore::new(dir.path());

    let mem = Memory::new("user likes Rust", MemorySource::User);
    let id = mem.id;
    store.save(&mem).unwrap();

    let loaded = store.load(id).unwrap();
    assert_eq!(loaded.fact, "user likes Rust");

    let all = store.list().unwrap();
    assert_eq!(all.len(), 1);
}

#[tokio::test]
async fn memory_store_update_and_delete() {
    let dir = temp_dir("marrow_mem");
    let store = MemoryStore::new(dir.path());

    let mem = Memory::new("old fact", MemorySource::Auto);
    let id = mem.id;
    store.save(&mem).unwrap();

    store.update(id, "new fact".to_string()).unwrap();
    let updated = store.load(id).unwrap();
    assert_eq!(updated.fact, "new fact");

    store.delete(id).unwrap();
    assert!(store.load(id).is_err());
    assert!(store.list().unwrap().is_empty());
}

#[tokio::test]
async fn memory_store_list_empty_dir() {
    let dir = temp_dir("marrow_mem");
    let store = MemoryStore::new(dir.path().join("nonexistent"));
    let list = store.list().unwrap();
    assert!(list.is_empty());
}

// ---------------------------------------------------------------------------
// Memory provider (selection) tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn memory_provider_empty_store_returns_empty() {
    let dir = temp_dir("marrow_mem");
    let store = MemoryStore::new(dir.path());
    let backend = MockBackend::new(vec![]);

    let result = memory_provider::select_memories("anything", &store, &backend)
        .await
        .unwrap();
    assert!(result.is_empty());
}

#[tokio::test]
async fn memory_provider_selects_relevant_memories() {
    let dir = temp_dir("marrow_mem");
    let store = MemoryStore::new(dir.path());

    let mem = Memory::new("user prefers dark mode", MemorySource::User);
    let id = mem.id;
    store.save(&mem).unwrap();

    let backend = MockBackend::new(vec![&format!(r#"["{id}"]"#)]);

    let result = memory_provider::select_memories("what theme do I like?", &store, &backend)
        .await
        .unwrap();
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].fact, "user prefers dark mode");
}

#[tokio::test]
async fn memories_to_context_format() {
    let mems = vec![Memory::new("fact one", MemorySource::Auto)];
    let ctx = memory_provider::memories_to_context(&mems);
    assert_eq!(ctx["memories"][0]["fact"], "fact one");
}

// ---------------------------------------------------------------------------
// Memory writer tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn memory_writer_saves_new_facts() {
    let dir = temp_dir("marrow_mem");
    let store = MemoryStore::new(dir.path());

    let backend = MockBackend::new(vec![
        r#"{"save": ["User prefers UTC timezone"], "update": {}, "delete": []}"#,
    ]);

    memory_writer::process_interaction(
        "what time is it?",
        "It's 3pm UTC",
        &store,
        &backend,
    )
    .await
    .unwrap();

    let all = store.list().unwrap();
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].fact, "User prefers UTC timezone");
}

#[tokio::test]
async fn memory_writer_does_nothing_when_empty() {
    let dir = temp_dir("marrow_mem");
    let store = MemoryStore::new(dir.path());

    let backend = MockBackend::new(vec![
        r#"{"save": [], "update": {}, "delete": []}"#,
    ]);

    memory_writer::process_interaction("hello", "hi", &store, &backend)
        .await
        .unwrap();

    assert!(store.list().unwrap().is_empty());
}

#[tokio::test]
async fn memory_writer_updates_existing() {
    let dir = temp_dir("marrow_mem");
    let store = MemoryStore::new(dir.path());

    let mem = Memory::new("old fact", MemorySource::Auto);
    let id = mem.id;
    store.save(&mem).unwrap();

    let backend = MockBackend::new(vec![&format!(
        r#"{{"save": [], "update": {{"{id}": "updated fact"}}, "delete": []}}"#
    )]);

    memory_writer::process_interaction("update", "done", &store, &backend)
        .await
        .unwrap();

    let loaded = store.load(id).unwrap();
    assert_eq!(loaded.fact, "updated fact");
}

#[tokio::test]
async fn memory_writer_deletes_outdated() {
    let dir = temp_dir("marrow_mem");
    let store = MemoryStore::new(dir.path());

    let mem = Memory::new("stale fact", MemorySource::Auto);
    let id = mem.id;
    store.save(&mem).unwrap();

    let backend = MockBackend::new(vec![&format!(
        r#"{{"save": [], "update": {{}}, "delete": ["{id}"]}}"#
    )]);

    memory_writer::process_interaction("cleanup", "done", &store, &backend)
        .await
        .unwrap();

    assert!(store.list().unwrap().is_empty());
}

// ---------------------------------------------------------------------------
// Toolbox tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn toolbox_save_load_list() {
    let dir = temp_dir("marrow_tb");
    let toolbox = Toolbox::new(dir.path());

    let meta = ToolMeta {
        name: "test_tool".to_string(),
        description: "A test tool".to_string(),
        provides: vec!["test_tool".to_string()],
        validated: false,
    };

    toolbox.save_tool(&meta, "return { ok = true }").unwrap();

    let loaded = toolbox.load_meta("test_tool").unwrap();
    assert_eq!(loaded.name, "test_tool");
    assert!(!loaded.validated);

    let source = toolbox.load_source("test_tool").unwrap();
    assert_eq!(source, "return { ok = true }");

    let all = toolbox.list_tools().unwrap();
    assert_eq!(all.len(), 1);

    let unvalidated = toolbox.list_unvalidated().unwrap();
    assert_eq!(unvalidated.len(), 1);
}

#[tokio::test]
async fn toolbox_delete_tool() {
    let dir = temp_dir("marrow_tb");
    let toolbox = Toolbox::new(dir.path());

    let meta = ToolMeta {
        name: "to_delete".to_string(),
        description: "will be deleted".to_string(),
        provides: vec!["to_delete".to_string()],
        validated: false,
    };

    toolbox.save_tool(&meta, "return {}").unwrap();
    assert_eq!(toolbox.list_tools().unwrap().len(), 1);

    toolbox.delete_tool("to_delete").unwrap();
    assert!(toolbox.list_tools().unwrap().is_empty());
}

#[tokio::test]
async fn toolbox_load_provider_executes() {
    let dir = temp_dir("marrow_tb");
    let toolbox = Toolbox::new(dir.path());

    let meta = ToolMeta {
        name: "greet".to_string(),
        description: "Returns greeting".to_string(),
        provides: vec!["greet".to_string()],
        validated: true,
    };
    toolbox.save_tool(&meta, r#"return { msg = "hi" }"#).unwrap();

    let provider = toolbox.load_provider("greet").unwrap();
    let client = Arc::new(Client::new());
    let result = provider.execute("test", client).await.unwrap();
    assert_eq!(result["msg"], "hi");
}

// ---------------------------------------------------------------------------
// Task registry tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn registry_create_and_get() {
    let registry = TaskRegistry::new();
    let task = Task::new("do something");
    let id = task.id;

    let created_id = registry.create(task).await;
    assert_eq!(created_id, id);

    let retrieved = registry.get(id).await.unwrap();
    assert_eq!(retrieved.description, "do something");
}

#[tokio::test]
async fn registry_run_succeeds_with_mock_executor() {
    use marrow::executor::Executor;

    struct MockExecutor;
    impl Executor for MockExecutor {
        async fn execute(
            &self,
            _task: &Task,
            _context: &Context,
            _history: Option<&[Message]>,
        ) -> Result<serde_json::Value, Box<dyn std::error::Error + Send + Sync>> {
            Ok(serde_json::json!("task done"))
        }
    }

    let registry = TaskRegistry::new();
    let task = Task::new("test task");
    let id = registry.create(task).await;

    let ctx = Context::new(serde_json::json!({}));
    let result = registry.run(id, &MockExecutor, &ctx, None).await.unwrap();
    assert_eq!(result, "task done");

    let task = registry.get(id).await.unwrap();
    assert_eq!(task.status, marrow::task::TaskStatus::Succeeded);
}

#[tokio::test]
async fn registry_run_records_failure() {
    use marrow::executor::Executor;

    struct FailingExecutor;
    impl Executor for FailingExecutor {
        async fn execute(
            &self,
            _task: &Task,
            _context: &Context,
            _history: Option<&[Message]>,
        ) -> Result<serde_json::Value, Box<dyn std::error::Error + Send + Sync>> {
            Err("something broke".into())
        }
    }

    let registry = TaskRegistry::new();
    let task = Task::new("failing task");
    let id = registry.create(task).await;

    let ctx = Context::new(serde_json::json!({}));
    let result = registry.run(id, &FailingExecutor, &ctx, None).await;
    assert!(result.is_err());

    let task = registry.get(id).await.unwrap();
    assert_eq!(task.status, marrow::task::TaskStatus::Failed);
    assert!(task.error.unwrap().contains("something broke"));
}

// ---------------------------------------------------------------------------
// Session tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn session_summarization() {
    let backend = MockBackend::new(vec!["User discussed various topics about Rust."]);

    let mut session = ChatSession::new();
    for i in 0..22 {
        session.append(Message::user(format!("message {i}")));
    }

    assert!(session.needs_summarization());
    session.summarize(&backend).await.unwrap();

    let msgs = session.build_messages(None);
    assert!(msgs.len() < 22);
    assert_eq!(msgs[0].role, "system");
    assert!(msgs[0].content.contains("Previous conversation summary"));
}

// ---------------------------------------------------------------------------
// Janitor tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn janitor_validates_passing_tool() {
    let dir = temp_dir("marrow_tb");
    let toolbox = Toolbox::new(dir.path());
    let log = noop_log().await;

    let meta = ToolMeta {
        name: "good_tool".to_string(),
        description: "Does good things".to_string(),
        provides: vec!["good_tool".to_string()],
        validated: false,
    };
    toolbox.save_tool(&meta, "return { ok = true }").unwrap();

    let backend = MockBackend::new(vec![
        "```verdict\nPASS\n```\n```issues\nnone\n```\n```suggestions\nnone\n```",
    ]);

    marrow::janitor::review_and_fix(&toolbox, "good_tool", &backend, &log)
        .await
        .unwrap();

    let updated = toolbox.load_meta("good_tool").unwrap();
    assert!(updated.validated);
}

#[tokio::test]
async fn janitor_regenerates_failing_tool() {
    let dir = temp_dir("marrow_tb");
    let toolbox = Toolbox::new(dir.path());
    let log = noop_log().await;

    let meta = ToolMeta {
        name: "bad_tool".to_string(),
        description: "Broken tool".to_string(),
        provides: vec!["bad_tool".to_string()],
        validated: false,
    };
    toolbox.save_tool(&meta, "return {}").unwrap();

    let backend = MockBackend::new(vec![
        "```verdict\nFAIL\n```\n```issues\n- missing error handling\n```\n```suggestions\n- add checks\n```",
        "```name\nbad_tool\n```\n```description\nFixed tool\n```\n```lua\nreturn { ok = true }\n```",
        "```verdict\nPASS\n```\n```issues\nnone\n```\n```suggestions\nnone\n```",
    ]);

    marrow::janitor::review_and_fix(&toolbox, "bad_tool", &backend, &log)
        .await
        .unwrap();

    let updated = toolbox.load_meta("bad_tool").unwrap();
    assert!(updated.validated);
}

#[tokio::test]
async fn janitor_deletes_after_max_failures() {
    let dir = temp_dir("marrow_tb");
    let toolbox = Toolbox::new(dir.path());
    let log = noop_log().await;

    let meta = ToolMeta {
        name: "hopeless".to_string(),
        description: "Unfixable tool".to_string(),
        provides: vec!["hopeless".to_string()],
        validated: false,
    };
    toolbox.save_tool(&meta, "return {}").unwrap();

    let backend = MockBackend::new(vec![
        "```verdict\nFAIL\n```\n```issues\n- broken\n```\n```suggestions\n- fix it\n```",
        "```name\nhopeless\n```\n```description\nStill broken\n```\n```lua\nreturn {}\n```",
        "```verdict\nFAIL\n```\n```issues\n- still broken\n```\n```suggestions\n- try again\n```",
        "```name\nhopeless\n```\n```description\nStill broken\n```\n```lua\nreturn {}\n```",
        "```verdict\nFAIL\n```\n```issues\n- hopelessly broken\n```\n```suggestions\n- give up\n```",
    ]);

    marrow::janitor::review_and_fix(&toolbox, "hopeless", &backend, &log)
        .await
        .unwrap();

    assert!(toolbox.list_tools().unwrap().is_empty());
}

// ---------------------------------------------------------------------------
// Event log tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn event_log_writes_to_file() {
    let dir = temp_dir("marrow_log");
    let log_path = dir.path().join("test.jsonl");
    let log = EventLog::new(Some(log_path.clone()), false).await.unwrap();

    log.emit(Event::TaskCreated {
        task_id: "test-123".to_string(),
        description: "test task".to_string(),
        role: "default".to_string(),
    })
    .await;

    log.emit(Event::TaskExecuted {
        task_id: "test-123".to_string(),
        status: "succeeded".to_string(),
    })
    .await;

    let content = tokio::fs::read_to_string(&log_path).await.unwrap();
    assert!(content.contains("test-123"));
    assert!(content.contains("task_created"));
}

#[tokio::test]
async fn event_log_no_file_doesnt_panic() {
    let log = EventLog::new(None, false).await.unwrap();
    log.emit(Event::TaskExecuted {
        task_id: "x".to_string(),
        status: "succeeded".to_string(),
    })
    .await;
}

// ---------------------------------------------------------------------------
// Codegen tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn codegen_generates_and_saves_tool() {
    let dir = temp_dir("marrow_tb");
    let toolbox = Toolbox::new(dir.path());
    let client = Arc::new(Client::new());

    let backend = MockBackend::new(vec![
        "```name\ngreeter\n```\n```description\nReturns a greeting\n```\n```lua\nreturn { greeting = \"hello world\" }\n```",
    ]);

    let name = marrow::codegen::generate_provider("say hello", &backend, &toolbox, client)
        .await
        .unwrap();

    assert_eq!(name, "greeter");

    let meta = toolbox.load_meta("greeter").unwrap();
    assert_eq!(meta.description, "Returns a greeting");
    assert!(!meta.validated);

    let source = toolbox.load_source("greeter").unwrap();
    assert!(source.contains("hello world"));
}

#[tokio::test]
async fn codegen_rejects_broken_lua() {
    let dir = temp_dir("marrow_tb");
    let toolbox = Toolbox::new(dir.path());
    let client = Arc::new(Client::new());

    let backend = MockBackend::new(vec![
        "```name\nbroken\n```\n```description\nBroken tool\n```\n```lua\nerror('intentional failure')\n```",
    ]);

    let result = marrow::codegen::generate_provider("test", &backend, &toolbox, client).await;
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("failed test run"));

    assert!(toolbox.list_tools().unwrap().is_empty());
}

// ---------------------------------------------------------------------------
// Full pipeline: conversational (no tools needed)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn pipeline_conversational_no_tools() {
    let mem_dir = temp_dir("marrow_mem");
    let memory_store = MemoryStore::new(mem_dir.path());
    let registry = TaskRegistry::new();
    let client = Arc::new(Client::new());

    let backend = MockBackend::new(vec![
        "NO",                                                // triage
        "Hello! How are you?",                               // task execution
        r#"{"save": [], "update": {}, "delete": []}"#,      // memory writer
    ]);

    let fast = &backend as &dyn ModelBackend;
    let memories = memory_provider::select_memories("hi there", &memory_store, fast)
        .await
        .unwrap();
    assert!(memories.is_empty());

    let needs_tools = triage::needs_external_data("hi there", fast, None, &memories)
        .await
        .unwrap();
    assert!(!needs_tools);

    // No tools → empty stages
    let assembler = ContextAssembler::new(client);
    let mut context = assembler.assemble("hi there", &[]).await.unwrap();

    let memory_context = memory_provider::memories_to_context(&memories);
    if let Some(obj) = context.data.as_object_mut() {
        obj.insert("memories".to_string(), memory_context);
    }

    let mut task = Task::new("hi there");
    task.model_role = "default".to_string();
    let id = registry.create(task).await;

    use marrow::executor::Executor;
    struct DirectExecutor<'a>(&'a dyn ModelBackend);
    impl Executor for DirectExecutor<'_> {
        async fn execute(
            &self,
            task: &Task,
            context: &Context,
            history: Option<&[Message]>,
        ) -> Result<serde_json::Value, Box<dyn std::error::Error + Send + Sync>> {
            let system_context = format!("Context: {}", context.data);
            let response = if let Some(msgs) = history {
                let mut messages = vec![Message::system(system_context)];
                messages.extend(msgs.iter().cloned());
                messages.push(Message::user(&task.description));
                self.0.complete_chat(messages).await?
            } else {
                self.0.complete(format!("{system_context}\n\nTask: {}", task.description)).await?
            };
            Ok(serde_json::Value::String(response))
        }
    }

    let executor = DirectExecutor(fast);
    let result = registry.run(id, &executor, &context, None).await.unwrap();
    assert_eq!(result.as_str().unwrap(), "Hello! How are you?");

    memory_writer::process_interaction("hi there", "Hello! How are you?", &memory_store, fast)
        .await
        .unwrap();

    assert!(memory_store.list().unwrap().is_empty());
}

// ---------------------------------------------------------------------------
// Full pipeline: tool-needing prompt with existing tool
// ---------------------------------------------------------------------------

#[tokio::test]
async fn pipeline_with_existing_tool() {
    let tb_dir = temp_dir("marrow_tb");
    let mem_dir = temp_dir("marrow_mem");
    let toolbox = Toolbox::new(tb_dir.path());
    let memory_store = MemoryStore::new(mem_dir.path());
    let client = Arc::new(Client::new());

    let meta = ToolMeta {
        name: "time_lookup".to_string(),
        description: "Get current time for a timezone".to_string(),
        provides: vec!["time_lookup".to_string()],
        validated: true,
    };
    toolbox
        .save_tool(&meta, r#"return { time = "15:30 UTC" }"#)
        .unwrap();

    let backend = MockBackend::new(vec![
        "YES",                                                                            // triage
        r#"{"stages": [{"tools": {"time_lookup": {"TIMEZONE": "UTC"}}}]}"#,               // tool selection
        "The time is 15:30 UTC.",                                                         // execution
        r#"{"save": ["User uses UTC timezone"], "update": {}, "delete": []}"#,            // memory writer
    ]);

    let fast = &backend as &dyn ModelBackend;

    let memories = memory_provider::select_memories("what time is it?", &memory_store, fast)
        .await
        .unwrap();

    let needs_tools = triage::needs_external_data("what time is it?", fast, None, &memories)
        .await
        .unwrap();
    assert!(needs_tools);

    let available = toolbox.list_tools().unwrap();
    let selection = tool_selection::select_tools("what time is it?", &available, fast, None)
        .await
        .unwrap();
    assert_eq!(selection.stages.len(), 1);
    assert!(selection.stages[0].tools.contains_key("time_lookup"));

    // Context assembly with stages
    let mut assembler = ContextAssembler::new(client);
    for name in selection.all_tool_names() {
        let provider = toolbox.load_provider(&name).unwrap();
        assembler.add_provider(provider);
    }

    let mut context = assembler
        .assemble("what time is it?", &selection.stages)
        .await
        .unwrap();
    assert_eq!(context.data["time_lookup"]["time"], "15:30 UTC");

    let memory_context = memory_provider::memories_to_context(&memories);
    if let Some(obj) = context.data.as_object_mut() {
        obj.insert("memories".to_string(), memory_context);
    }

    let response = fast
        .complete(format!("Context: {}\n\nTask: what time is it?", context.data))
        .await
        .unwrap();
    assert_eq!(response, "The time is 15:30 UTC.");

    memory_writer::process_interaction("what time is it?", &response, &memory_store, fast)
        .await
        .unwrap();

    let saved = memory_store.list().unwrap();
    assert_eq!(saved.len(), 1);
    assert_eq!(saved[0].fact, "User uses UTC timezone");
}

// ---------------------------------------------------------------------------
// Full pipeline: multi-stage tool composition
// ---------------------------------------------------------------------------

#[tokio::test]
async fn pipeline_multi_stage_composition() {
    let tb_dir = temp_dir("marrow_tb");
    let toolbox = Toolbox::new(tb_dir.path());
    let client = Arc::new(Client::new());

    // Stage 1 tool: returns weather data
    let weather_meta = ToolMeta {
        name: "weather".to_string(),
        description: "Get weather for a location".to_string(),
        provides: vec!["weather".to_string()],
        validated: true,
    };
    toolbox
        .save_tool(
            &weather_meta,
            r#"return { temp = 25, condition = "sunny", location = PARAMS["LOCATION"] }"#,
        )
        .unwrap();

    // Stage 2 tool: reads weather result to plan activities
    let planner_meta = ToolMeta {
        name: "planner".to_string(),
        description: "Plan activities based on context".to_string(),
        provides: vec!["planner".to_string()],
        validated: true,
    };
    toolbox
        .save_tool(
            &planner_meta,
            r#"
            local w = json_parse(RESULTS["weather"])
            if w.condition == "sunny" then
                return { activity = "go to the park in " .. w.location }
            else
                return { activity = "stay indoors" }
            end
            "#,
        )
        .unwrap();

    // Build stages
    let mut stage1_tools = HashMap::new();
    let mut weather_params = HashMap::new();
    weather_params.insert("LOCATION".to_string(), "Portland".to_string());
    stage1_tools.insert("weather".to_string(), weather_params);

    let mut stage2_tools = HashMap::new();
    stage2_tools.insert("planner".to_string(), HashMap::new());

    let stages = vec![
        Stage { tools: stage1_tools },
        Stage { tools: stage2_tools },
    ];

    let mut assembler = ContextAssembler::new(client);
    for name in ["weather", "planner"] {
        assembler.add_provider(toolbox.load_provider(name).unwrap());
    }

    let ctx = assembler.assemble("plan my weekend", &stages).await.unwrap();

    // Stage 1 output
    assert_eq!(ctx.data["weather"]["temp"], 25);
    assert_eq!(ctx.data["weather"]["location"], "Portland");

    // Stage 2 used stage 1's output
    assert_eq!(ctx.data["planner"]["activity"], "go to the park in Portland");
}

// ---------------------------------------------------------------------------
// Answer check tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn answer_check_detects_successful_answer() {
    let backend = MockBackend::new(vec![
        "```verdict\nYES\n```\n```reason\nThe response contains the weather information.\n```",
    ]);

    let result = answer_check::check_answer(
        "what's the weather?",
        r#"{"temp": 22, "condition": "sunny"}"#,
        "The weather is 22°C and sunny.",
        &backend,
    )
    .await
    .unwrap();

    assert!(result.answered);
}

#[tokio::test]
async fn answer_check_detects_insufficient_answer() {
    let backend = MockBackend::new(vec![
        "```verdict\nNO\n```\n```reason\nOnly HTML headers were returned, no blog content visible.\n```",
    ]);

    let result = answer_check::check_answer(
        "what is my latest blog post about?",
        r#"{"body_preview": "<head>...</head>"}"#,
        "I cannot determine what your latest blog post is about.",
        &backend,
    )
    .await
    .unwrap();

    assert!(!result.answered);
    assert!(result.reason.contains("HTML headers"));
}

// ---------------------------------------------------------------------------
// Tool selection with prior attempt (re-planning) tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn tool_selection_replan_with_prior_attempt() {
    let tools = vec![
        ToolMeta {
            name: "blog_access".to_string(),
            description: "Fetch a web page".to_string(),
            provides: vec!["blog_access".to_string()],
            validated: true,
        },
        ToolMeta {
            name: "rss_reader".to_string(),
            description: "Parse an RSS feed".to_string(),
            provides: vec!["rss_reader".to_string()],
            validated: true,
        },
    ];

    let prior = PriorAttempt {
        tool_outputs: vec![
            ("blog_access".to_string(), r#"{"body_preview": "<head>...</head>"}"#.to_string()),
        ],
        reason: "Only HTML headers returned, no blog content visible".to_string(),
    };

    let backend = MockBackend::new(vec![
        r#"{"stages": [{"tools": {"rss_reader": {"URL": "https://nsg.cc/index.xml"}}}]}"#,
    ]);

    let result = tool_selection::select_tools_with_retry_context(
        "what is my latest blog post about?",
        &tools,
        &backend,
        None,
        Some(&prior),
    )
    .await
    .unwrap();

    assert_eq!(result.stages.len(), 1);
    assert!(result.stages[0].tools.contains_key("rss_reader"));
}

#[tokio::test]
async fn tool_selection_replan_allows_empty_toolbox() {
    // When re-planning, empty toolbox should still call the model
    // (new tools can be generated)
    let prior = PriorAttempt {
        tool_outputs: vec![],
        reason: "no tools were available".to_string(),
    };

    let backend = MockBackend::new(vec![
        r#"{"stages": [{"tools": {"new_tool": {"PARAM": "value"}}}]}"#,
    ]);

    let result = tool_selection::select_tools_with_retry_context(
        "do something",
        &[],
        &backend,
        None,
        Some(&prior),
    )
    .await
    .unwrap();

    assert!(!result.is_empty());
    assert!(result.stages[0].tools.contains_key("new_tool"));
}

// ---------------------------------------------------------------------------
// Full pipeline: re-planning after insufficient answer
// ---------------------------------------------------------------------------

#[tokio::test]
async fn pipeline_replan_after_insufficient_answer() {
    let tb_dir = temp_dir("marrow_tb");
    let mem_dir = temp_dir("marrow_mem");
    let toolbox = Toolbox::new(tb_dir.path());
    let _memory_store = MemoryStore::new(mem_dir.path());
    let client = Arc::new(Client::new());

    // First tool: returns insufficient data (HTML headers only)
    let blog_meta = ToolMeta {
        name: "blog_access".to_string(),
        description: "Fetch blog homepage".to_string(),
        provides: vec!["blog_access".to_string()],
        validated: true,
    };
    toolbox
        .save_tool(
            &blog_meta,
            r#"return { body_preview = "<head>scripts and meta tags</head>" }"#,
        )
        .unwrap();

    // Second tool: returns actual content (RSS feed)
    let rss_meta = ToolMeta {
        name: "rss_reader".to_string(),
        description: "Parse RSS feed".to_string(),
        provides: vec!["rss_reader".to_string()],
        validated: true,
    };
    toolbox
        .save_tool(
            &rss_meta,
            r#"return { latest_post = { title = "Building Marrow", summary = "A lean agent framework" } }"#,
        )
        .unwrap();

    // Simulate the re-planning pipeline manually:
    // Attempt 1: select blog_access → insufficient → answer check says NO
    // Re-plan: select rss_reader → sufficient → answer check says YES

    let backend = MockBackend::new(vec![
        // Attempt 1: tool selection picks blog_access
        r#"{"stages": [{"tools": {"blog_access": {}}}]}"#,
        // Attempt 1: model can't answer with HTML headers
        "I cannot determine what your latest blog post is about from the HTML headers.",
        // Answer check: NO
        "```verdict\nNO\n```\n```reason\nOnly HTML head section returned, no actual content.\n```",
        // Re-plan: tool selection picks rss_reader
        r#"{"stages": [{"tools": {"rss_reader": {"URL": "https://nsg.cc/index.xml"}}}]}"#,
        // Attempt 2: model answers from RSS data
        "Your latest blog post is 'Building Marrow' — about a lean agent framework.",
        // Answer check: YES
        "```verdict\nYES\n```\n```reason\nThe response answers the question with specific post details.\n```",
    ]);

    let fast = &backend as &dyn ModelBackend;

    // Attempt 1
    let available = toolbox.list_tools().unwrap();
    let selection1 = tool_selection::select_tools("latest blog post?", &available, fast, None)
        .await
        .unwrap();
    assert!(selection1.stages[0].tools.contains_key("blog_access"));

    let mut assembler1 = ContextAssembler::new(client.clone());
    for name in selection1.all_tool_names() {
        assembler1.add_provider(toolbox.load_provider(&name).unwrap());
    }
    let context1 = assembler1.assemble("latest blog post?", &selection1.stages).await.unwrap();

    // Execute attempt 1
    let response1 = fast
        .complete(format!("Context: {}\n\nTask: latest blog post?", context1.data))
        .await
        .unwrap();
    assert!(response1.contains("cannot determine"));

    // Answer check says NO
    let check1 = answer_check::check_answer(
        "latest blog post?",
        &context1.data.to_string(),
        &response1,
        fast,
    )
    .await
    .unwrap();
    assert!(!check1.answered);

    // Build prior attempt for re-planning
    let prior = PriorAttempt {
        tool_outputs: vec![(
            "blog_access".to_string(),
            context1.data["blog_access"].to_string(),
        )],
        reason: check1.reason,
    };

    // Re-plan: tool selection with prior attempt context
    let selection2 = tool_selection::select_tools_with_retry_context(
        "latest blog post?",
        &available,
        fast,
        None,
        Some(&prior),
    )
    .await
    .unwrap();
    assert!(selection2.stages[0].tools.contains_key("rss_reader"));

    // Execute attempt 2
    let mut assembler2 = ContextAssembler::new(client);
    for name in selection2.all_tool_names() {
        assembler2.add_provider(toolbox.load_provider(&name).unwrap());
    }
    let context2 = assembler2.assemble("latest blog post?", &selection2.stages).await.unwrap();

    let response2 = fast
        .complete(format!("Context: {}\n\nTask: latest blog post?", context2.data))
        .await
        .unwrap();
    assert!(response2.contains("Building Marrow"));

    // Answer check says YES
    let check2 = answer_check::check_answer(
        "latest blog post?",
        &context2.data.to_string(),
        &response2,
        fast,
    )
    .await
    .unwrap();
    assert!(check2.answered);
}
