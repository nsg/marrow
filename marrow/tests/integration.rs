use std::collections::HashMap;
use std::sync::Arc;

use marrow::agent;
use marrow::context::LuaProvider;
use marrow::events::{Event, EventLog};
use marrow::memory::{Memory, MemorySource, MemoryStore};
use marrow::memory_provider;
use marrow::memory_writer;
use marrow::model::{CompletionResult, ModelBackend};
use marrow::session::{ChatSession, Message};
use marrow::tool::ToolRegistry;
use marrow::toolbox::{ToolMeta, Toolbox};
use reqwest::Client;
use tokio::sync::Mutex;

// ---------------------------------------------------------------------------
// MockBackend
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

// ---------------------------------------------------------------------------
// Agent action parsing tests
// ---------------------------------------------------------------------------

#[test]
fn agent_parse_call_tool() {
    let input = r#"{"action": "call_tool", "tool": "weather", "params": {"LOCATION": "Tokyo"}}"#;
    match agent::parse_action(input) {
        agent::Action::CallTool { tool, params } => {
            assert_eq!(tool, "weather");
            assert_eq!(params.get("LOCATION").unwrap(), "Tokyo");
        }
        other => panic!("expected CallTool, got {other:?}"),
    }
}

#[test]
fn agent_parse_answer() {
    let input = r#"{"action": "answer", "text": "The answer is 42."}"#;
    match agent::parse_action(input) {
        agent::Action::Answer { text, fallback } => {
            assert_eq!(text, "The answer is 42.");
            assert!(!fallback);
        }
        other => panic!("expected Answer, got {other:?}"),
    }
}

#[test]
fn agent_parse_malformed_defaults_to_answer() {
    match agent::parse_action("I don't know") {
        agent::Action::Answer { text, fallback } => {
            assert_eq!(text, "I don't know");
            assert!(fallback);
        }
        other => panic!("expected Answer, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Agent loop integration tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn agent_loop_call_tool_then_answer() {
    let dir = temp_dir("marrow_tb");
    let toolbox = Toolbox::new(dir.path());
    let log = noop_log().await;
    let client = Arc::new(Client::new());

    toolbox
        .save_tool(
            &ToolMeta {
                name: "greeter".to_string(),
                description: "Returns greeting".to_string(),
                provides: vec![],
                validated: true,
            },
            r#"return { msg = "hello world" }"#,
        )
        .unwrap();

    let registry = ToolRegistry::new(Toolbox::new(dir.path()), dir.path());

    // Step 1: agent calls greeter tool
    // Step 2: agent says answer
    // Step 3: answer_backend formulates final response
    let agent_backend = MockBackend::new(vec![
        r#"{"action": "call_tool", "tool": "greeter", "params": {}}"#,
        r#"{"action": "answer", "text": ""}"#,
    ]);
    let answer_backend = MockBackend::new(vec!["The greeting is: hello world"]);
    let fast_backend = MockBackend::new(vec![]);

    let result = agent::run_loop(
        "say hello",
        "test-task",
        &agent_backend,
        &answer_backend,
        &fast_backend,
        &registry,
        client,
        &[],
        &log,
        None,
        None,
        &[],
        None,
        None,
        None,
        None,
        None,
        "test",
    )
    .await
    .unwrap();

    assert!(result.contains("hello world"));
}

#[tokio::test]
async fn agent_loop_save_tool_then_call_then_answer() {
    let dir = temp_dir("marrow_tb");
    let toolbox = Toolbox::new(dir.path());
    let log = noop_log().await;
    let client = Arc::new(Client::new());

    let registry = ToolRegistry::new(Toolbox::new(dir.path()), dir.path());

    // Step 1: model runs inline Lua
    // Step 2: model saves it as a tool
    // Step 3: model calls the saved tool
    // Step 4: model answers from result
    let agent_backend = MockBackend::new(vec![
        "```lua\nreturn { echo = \"hi\" }\n```",
        r#"{"action": "save_tool", "name": "echo_tool", "description": "Echoes a message"}"#,
        r#"{"action": "call_tool", "tool": "echo_tool", "params": {"MSG": "hi"}}"#,
        r#"{"action": "answer", "text": ""}"#,
    ]);

    let answer_backend = MockBackend::new(vec!["Echo says: hi"]);
    let fast_backend = MockBackend::new(vec![]);

    let result = agent::run_loop(
        "echo something",
        "test-task",
        &agent_backend,
        &answer_backend,
        &fast_backend,
        &registry,
        client,
        &[],
        &log,
        None,
        None,
        &[],
        None,
        None,
        None,
        None,
        None,
        "test",
    )
    .await
    .unwrap();

    assert!(result.contains("Echo says"));
    assert!(toolbox.load_meta("echo_tool").is_ok());
}

#[tokio::test]
async fn agent_loop_direct_answer() {
    let dir = temp_dir("marrow_tb");
    let log = noop_log().await;
    let client = Arc::new(Client::new());

    let registry = ToolRegistry::new(Toolbox::new(dir.path()), dir.path());

    let agent_backend = MockBackend::new(vec![r#"{"action": "answer", "text": ""}"#]);
    let answer_backend = MockBackend::new(vec!["2 + 2 = 4"]);
    let fast_backend = MockBackend::new(vec![]);

    let result = agent::run_loop(
        "what is 2+2?",
        "test-task",
        &agent_backend,
        &answer_backend,
        &fast_backend,
        &registry,
        client,
        &[],
        &log,
        None,
        None,
        &[],
        None,
        None,
        None,
        None,
        None,
        "test",
    )
    .await
    .unwrap();

    assert_eq!(result, "2 + 2 = 4");
}

#[tokio::test]
async fn agent_loop_tool_failure_recovery() {
    let dir = temp_dir("marrow_tb");
    let log = noop_log().await;
    let client = Arc::new(Client::new());

    let registry = ToolRegistry::new(Toolbox::new(dir.path()), dir.path());

    // Step 1: agent tries nonexistent tool → gets error
    // Step 2: agent says answer
    // Step 3: answer_backend formulates response
    let agent_backend = MockBackend::new(vec![
        r#"{"action": "call_tool", "tool": "missing_tool", "params": {}}"#,
        r#"{"action": "answer", "text": ""}"#,
    ]);
    let answer_backend = MockBackend::new(vec!["Tool was not available, but I can tell you..."]);
    let fast_backend = MockBackend::new(vec![]);

    let result = agent::run_loop(
        "do something",
        "test-task",
        &agent_backend,
        &answer_backend,
        &fast_backend,
        &registry,
        client,
        &[],
        &log,
        None,
        None,
        &[],
        None,
        None,
        None,
        None,
        None,
        "test",
    )
    .await
    .unwrap();

    assert!(result.contains("not available"));
}

// ---------------------------------------------------------------------------
// Lua sandbox + provider tests
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
        .execute_with_params(
            "test",
            client,
            &params,
            None,
            None,
            Arc::new(HashMap::new()),
        )
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
async fn lua_sandbox_blocks_unsafe_globals() {
    let provider = LuaProvider::new("test", "return { has_os = (os ~= nil) }");
    let client = Arc::new(Client::new());
    let result = provider.execute("test", client).await.unwrap();
    assert_eq!(result["has_os"], false);
}

// ---------------------------------------------------------------------------
// run_tool tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn run_tool_calls_another_tool() {
    let dir = temp_dir("marrow_tb");
    let toolbox = Toolbox::new(dir.path());

    toolbox
        .save_tool(
            &ToolMeta {
                name: "greeter".to_string(),
                description: "Returns greeting".to_string(),
                provides: vec![],
                validated: true,
            },
            r#"return { msg = "hello from greeter" }"#,
        )
        .unwrap();

    let caller = LuaProvider::new(
        "caller",
        r#"local result = run_tool("greeter", {}); return { got = result.msg }"#,
    );
    let client = Arc::new(Client::new());
    let result = caller
        .execute_with_params(
            "test",
            client,
            &HashMap::new(),
            Some(dir.path().to_path_buf()),
            None,
            Arc::new(HashMap::new()),
        )
        .await
        .unwrap();
    assert_eq!(result["got"], "hello from greeter");
}

#[tokio::test]
async fn run_tool_passes_params() {
    let dir = temp_dir("marrow_tb");
    let toolbox = Toolbox::new(dir.path());

    toolbox
        .save_tool(
            &ToolMeta {
                name: "echo".to_string(),
                description: "Echoes".to_string(),
                provides: vec![],
                validated: true,
            },
            r#"return { city = PARAMS["CITY"] }"#,
        )
        .unwrap();

    let caller = LuaProvider::new("caller", r#"return run_tool("echo", {CITY = "Tokyo"})"#);
    let client = Arc::new(Client::new());
    let result = caller
        .execute_with_params(
            "test",
            client,
            &HashMap::new(),
            Some(dir.path().to_path_buf()),
            None,
            Arc::new(HashMap::new()),
        )
        .await
        .unwrap();
    assert_eq!(result["city"], "Tokyo");
}

#[tokio::test]
async fn run_tool_recursion_guard() {
    let dir = temp_dir("marrow_tb");
    let toolbox = Toolbox::new(dir.path());

    toolbox
        .save_tool(
            &ToolMeta {
                name: "infinite".to_string(),
                description: "Loop".to_string(),
                provides: vec![],
                validated: true,
            },
            r#"return run_tool("infinite", {})"#,
        )
        .unwrap();

    let caller = LuaProvider::new("caller", r#"return run_tool("infinite", {})"#);
    let client = Arc::new(Client::new());
    let result = caller
        .execute_with_params(
            "test",
            client,
            &HashMap::new(),
            Some(dir.path().to_path_buf()),
            None,
            Arc::new(HashMap::new()),
        )
        .await;
    assert!(result.is_err());
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("max recursion depth")
    );
}

#[tokio::test]
async fn run_tool_not_available_without_toolbox() {
    let provider = LuaProvider::new("test", r#"return { has_run_tool = (run_tool ~= nil) }"#);
    let client = Arc::new(Client::new());
    let result = provider.execute("test", client).await.unwrap();
    assert_eq!(result["has_run_tool"], false);
}

#[tokio::test]
async fn run_tool_glue_composition() {
    let dir = temp_dir("marrow_tb");
    let toolbox = Toolbox::new(dir.path());

    toolbox
        .save_tool(
            &ToolMeta {
                name: "weather".to_string(),
                description: "Get weather".to_string(),
                provides: vec![],
                validated: true,
            },
            r#"return { temp = 22, condition = "sunny", location = PARAMS["LOCATION"] }"#,
        )
        .unwrap();

    toolbox
        .save_tool(
            &ToolMeta {
                name: "calendar".to_string(),
                description: "Get events".to_string(),
                provides: vec![],
                validated: true,
            },
            r#"return { events = {"hiking", "lunch"} }"#,
        )
        .unwrap();

    let glue = LuaProvider::new(
        "planner",
        r#"
        local weather = run_tool("weather", {LOCATION = PARAMS["LOCATION"]})
        local cal = run_tool("calendar", {})
        return { weather = weather, events = cal.events, recommendation = weather.condition .. " in " .. weather.location }
    "#,
    );

    let client = Arc::new(Client::new());
    let mut params = HashMap::new();
    params.insert("LOCATION".to_string(), "Portland".to_string());
    let result = glue
        .execute_with_params(
            "plan weekend",
            client,
            &params,
            Some(dir.path().to_path_buf()),
            None,
            Arc::new(HashMap::new()),
        )
        .await
        .unwrap();
    assert_eq!(result["weather"]["temp"], 22);
    assert_eq!(result["recommendation"], "sunny in Portland");
}

// ---------------------------------------------------------------------------
// Memory tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn memory_store_save_load_list() {
    let dir = temp_dir("marrow_mem");
    let store = MemoryStore::new(dir.path());
    let mem = Memory::new("user likes Rust", MemorySource::User);
    let id = mem.id;
    store.save(&mem).unwrap();
    assert_eq!(store.load(id).unwrap().fact, "user likes Rust");
    assert_eq!(store.list().unwrap().len(), 1);
}

#[tokio::test]
async fn memory_store_update_and_delete() {
    let dir = temp_dir("marrow_mem");
    let store = MemoryStore::new(dir.path());
    let mem = Memory::new("old fact", MemorySource::Auto);
    let id = mem.id;
    store.save(&mem).unwrap();
    store.update(id, "new fact".to_string()).unwrap();
    assert_eq!(store.load(id).unwrap().fact, "new fact");
    store.delete(id).unwrap();
    assert!(store.list().unwrap().is_empty());
}

#[tokio::test]
async fn memory_provider_selects_relevant() {
    let dir = temp_dir("marrow_mem");
    let store = MemoryStore::new(dir.path());
    let mem = Memory::new("user prefers dark mode", MemorySource::User);
    let id = mem.id;
    store.save(&mem).unwrap();
    let backend = MockBackend::new(vec![&format!(r#"["{id}"]"#)]);
    let result = memory_provider::select_memories("theme?", &store, &backend)
        .await
        .unwrap();
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].fact, "user prefers dark mode");
}

#[tokio::test]
async fn memory_writer_saves_new_facts() {
    let dir = temp_dir("marrow_mem");
    let store = MemoryStore::new(dir.path());
    let backend = MockBackend::new(vec![
        r#"{"save": ["User prefers UTC"], "update": {}, "delete": []}"#,
    ]);
    memory_writer::process_interaction("time?", "3pm UTC", &store, &backend)
        .await
        .unwrap();
    let all = store.list().unwrap();
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].fact, "User prefers UTC");
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
        description: "A test".to_string(),
        provides: vec![],
        validated: false,
    };
    toolbox.save_tool(&meta, "return { ok = true }").unwrap();
    assert_eq!(toolbox.load_meta("test_tool").unwrap().name, "test_tool");
    assert_eq!(toolbox.list_tools().unwrap().len(), 1);
    assert_eq!(toolbox.list_unvalidated().unwrap().len(), 1);
}

#[tokio::test]
async fn toolbox_delete_tool() {
    let dir = temp_dir("marrow_tb");
    let toolbox = Toolbox::new(dir.path());
    let meta = ToolMeta {
        name: "x".to_string(),
        description: "x".to_string(),
        provides: vec![],
        validated: false,
    };
    toolbox.save_tool(&meta, "return {}").unwrap();
    toolbox.delete_tool("x").unwrap();
    assert!(toolbox.list_tools().unwrap().is_empty());
}

// ---------------------------------------------------------------------------
// Knowledge file tests
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Session tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn session_summarization() {
    let backend = MockBackend::new(vec!["Summary of discussion."]);
    let mut session = ChatSession::new();
    for i in 0..22 {
        session.append(Message::user(format!("msg {i}")));
    }
    assert!(session.needs_summarization());
    session.summarize(&backend).await.unwrap();
    let msgs = session.build_messages(None);
    assert!(msgs.len() < 22);
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
        name: "good".to_string(),
        description: "Good".to_string(),
        provides: vec![],
        validated: false,
    };
    toolbox.save_tool(&meta, "return { ok = true }").unwrap();
    let backend = MockBackend::new(vec![
        "```verdict\nPASS\n```\n```issues\nnone\n```\n```suggestions\nnone\n```",
    ]);
    marrow::janitor::review_and_fix(&toolbox, "good", &backend, &log, &[])
        .await
        .unwrap();
    assert!(toolbox.load_meta("good").unwrap().validated);
}

#[tokio::test]
async fn janitor_deletes_after_max_failures() {
    let dir = temp_dir("marrow_tb");
    let toolbox = Toolbox::new(dir.path());
    let log = noop_log().await;
    let meta = ToolMeta {
        name: "bad".to_string(),
        description: "Bad".to_string(),
        provides: vec![],
        validated: false,
    };
    toolbox.save_tool(&meta, "return {}").unwrap();
    let backend = MockBackend::new(vec![
        "```verdict\nFAIL\n```\n```issues\n- broken\n```\n```suggestions\n- fix\n```",
        "```name\nbad\n```\n```description\nBad\n```\n```lua\nreturn {}\n```",
        "```verdict\nFAIL\n```\n```issues\n- broken\n```\n```suggestions\n- fix\n```",
        "```name\nbad\n```\n```description\nBad\n```\n```lua\nreturn {}\n```",
        "```verdict\nFAIL\n```\n```issues\n- broken\n```\n```suggestions\n- fix\n```",
    ]);
    marrow::janitor::review_and_fix(&toolbox, "bad", &backend, &log, &[])
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
        task_id: "t1".to_string(),
        description: "test".to_string(),
        role: "default".to_string(),
    })
    .await;
    log.emit(Event::TaskExecuted {
        task_id: "t1".to_string(),
        status: "succeeded".to_string(),
    })
    .await;
    let content = tokio::fs::read_to_string(&log_path).await.unwrap();
    assert!(content.contains("task_created"));
}
