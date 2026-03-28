use std::collections::HashMap;
use std::sync::Arc;

use marrow::context::{ContextAssembler, LuaProvider};
use marrow::events::{Event, EventLog};
use marrow::executor::Context;
use marrow::memory::{Memory, MemorySource, MemoryStore};
use marrow::memory_provider;
use marrow::memory_writer;
use marrow::model::{CompletionResult, ModelBackend};
use marrow::registry::TaskRegistry;
use marrow::session::{ChatSession, Message};
use marrow::task::Task;
use marrow::tool_selection;
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
    assert!(result.tools.is_empty());
}

#[tokio::test]
async fn tool_selection_picks_matching_tool() {
    let tools = vec![ToolMeta {
        name: "weather".to_string(),
        description: "Get weather for a location".to_string(),
        provides: vec!["weather".to_string()],
        validated: true,
    }];

    let backend = MockBackend::new(vec![
        r#"{"tools": ["weather"], "params": {"LOCATION": "Tokyo"}}"#,
    ]);

    let result = tool_selection::select_tools("weather in Tokyo", &tools, &backend, None)
        .await
        .unwrap();
    assert_eq!(result.tools, vec!["weather"]);
    assert_eq!(result.params.get("LOCATION").unwrap(), "Tokyo");
}

#[tokio::test]
async fn tool_selection_returns_empty_when_no_match() {
    let tools = vec![ToolMeta {
        name: "weather".to_string(),
        description: "Get weather".to_string(),
        provides: vec!["weather".to_string()],
        validated: true,
    }];

    let backend = MockBackend::new(vec![r#"{"tools": [], "params": {}}"#]);

    let result = tool_selection::select_tools("tell me a joke", &tools, &backend, None)
        .await
        .unwrap();
    assert!(result.tools.is_empty());
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
async fn lua_provider_receives_params() {
    let provider = LuaProvider::new("test", "return { city = LOCATION }");
    let client = Arc::new(Client::new());
    let mut params = HashMap::new();
    params.insert("LOCATION".to_string(), "Paris".to_string());
    let result = provider
        .execute_with_params("test", client, &params)
        .await
        .unwrap();
    assert_eq!(result["city"], "Paris");
}

#[tokio::test]
async fn lua_provider_receives_task_description() {
    let provider = LuaProvider::new("test", "return { desc = TASK_DESCRIPTION }");
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

    let names = vec!["a".to_string(), "b".to_string()];
    let ctx = assembler.assemble("test", &names).await.unwrap();

    assert_eq!(ctx.data["a"]["x"], 1);
    assert_eq!(ctx.data["b"]["y"], 2);
}

#[tokio::test]
async fn context_assembler_handles_failing_provider() {
    let client = Arc::new(Client::new());
    let mut assembler = ContextAssembler::new(client);
    assembler.add_provider(LuaProvider::new("good", "return { ok = true }"));
    assembler.add_provider(LuaProvider::new("bad", "error('boom')"));

    let names = vec!["good".to_string(), "bad".to_string()];
    let ctx = assembler.assemble("test", &names).await.unwrap();

    assert_eq!(ctx.data["good"]["ok"], true);
    assert!(ctx.data["bad"]["error"].as_str().unwrap().contains("boom"));
}

#[tokio::test]
async fn context_assembler_passes_params_to_providers() {
    let client = Arc::new(Client::new());
    let mut assembler = ContextAssembler::new(client);
    assembler.add_provider(LuaProvider::new("loc", "return { city = LOCATION }"));

    let mut params = HashMap::new();
    params.insert("LOCATION".to_string(), "Berlin".to_string());
    assembler.set_params(params);

    let names = vec!["loc".to_string()];
    let ctx = assembler.assemble("test", &names).await.unwrap();
    assert_eq!(ctx.data["loc"]["city"], "Berlin");
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
    // Add enough messages to trigger summarization (threshold is 20)
    for i in 0..22 {
        session.append(Message::user(format!("message {i}")));
    }

    assert!(session.needs_summarization());
    session.summarize(&backend).await.unwrap();

    // After summarization, recent messages are kept, old ones summarized
    let msgs = session.build_messages(None);
    // Should have a system message with summary + recent messages
    assert!(msgs.len() < 22);
    assert_eq!(msgs[0].role, "system");
    assert!(msgs[0].content.contains("Previous conversation summary"));
}

// ---------------------------------------------------------------------------
// Janitor tests (process_tool via toolbox + mock)
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

    // Mock returns PASS review
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

    // First call: FAIL review, second: regeneration, third: PASS review
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

    // 3 rounds of FAIL + regenerate, final FAIL triggers delete
    // Attempt 1: fail review → regenerate
    // Attempt 2: fail review → regenerate
    // Attempt 3: fail review → escalate + delete
    let backend = MockBackend::new(vec![
        // Attempt 1: review fails
        "```verdict\nFAIL\n```\n```issues\n- broken\n```\n```suggestions\n- fix it\n```",
        // Attempt 1: regenerate
        "```name\nhopeless\n```\n```description\nStill broken\n```\n```lua\nreturn {}\n```",
        // Attempt 2: review fails
        "```verdict\nFAIL\n```\n```issues\n- still broken\n```\n```suggestions\n- try again\n```",
        // Attempt 2: regenerate
        "```name\nhopeless\n```\n```description\nStill broken\n```\n```lua\nreturn {}\n```",
        // Attempt 3: review fails → escalate + delete
        "```verdict\nFAIL\n```\n```issues\n- hopelessly broken\n```\n```suggestions\n- give up\n```",
    ]);

    marrow::janitor::review_and_fix(&toolbox, "hopeless", &backend, &log)
        .await
        .unwrap();

    // Tool should be deleted
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

    // emit() awaits the write, but tokio file I/O may buffer. Sync via a second emit.
    log.emit(Event::TaskExecuted {
        task_id: "test-123".to_string(),
        status: "succeeded".to_string(),
    })
    .await;

    // Read the file — both events should be flushed since we awaited them
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
    // Just verify no panic
}

// ---------------------------------------------------------------------------
// Codegen parsing tests (via the public interface)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn codegen_generates_and_saves_tool() {
    let dir = temp_dir("marrow_tb");
    let toolbox = Toolbox::new(dir.path());
    let client = Arc::new(Client::new());

    // Mock returns well-formed codegen response with a Lua script that doesn't need HTTP
    let backend = MockBackend::new(vec![
        "```name\ngreeter\n```\n```description\nReturns a greeting\n```\n```lua\nreturn { greeting = \"hello world\" }\n```",
    ]);

    let name = marrow::codegen::generate_provider("say hello", &backend, &toolbox, client)
        .await
        .unwrap();

    assert_eq!(name, "greeter");

    let meta = toolbox.load_meta("greeter").unwrap();
    assert_eq!(meta.description, "Returns a greeting");
    assert!(!meta.validated); // Janitor hasn't reviewed yet

    let source = toolbox.load_source("greeter").unwrap();
    assert!(source.contains("hello world"));
}

#[tokio::test]
async fn codegen_rejects_broken_lua() {
    let dir = temp_dir("marrow_tb");
    let toolbox = Toolbox::new(dir.path());
    let client = Arc::new(Client::new());

    // Mock returns Lua that will error at runtime
    let backend = MockBackend::new(vec![
        "```name\nbroken\n```\n```description\nBroken tool\n```\n```lua\nerror('intentional failure')\n```",
    ]);

    let result = marrow::codegen::generate_provider("test", &backend, &toolbox, client).await;
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("failed test run"));

    // Tool should NOT be saved
    assert!(toolbox.list_tools().unwrap().is_empty());
}

// ---------------------------------------------------------------------------
// Full pipeline: conversational (no tools needed)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn pipeline_conversational_no_tools() {
    let tb_dir = temp_dir("marrow_tb");
    let mem_dir = temp_dir("marrow_mem");
    let _toolbox = Toolbox::new(tb_dir.path());
    let memory_store = MemoryStore::new(mem_dir.path());
    let _log = noop_log().await;
    let registry = TaskRegistry::new();
    let client = Arc::new(Client::new());

    // Pipeline calls:
    // 1. memory_provider::select_memories → no memories, skips backend call
    // 2. triage::needs_external_data → "NO"
    // 3. (skip tool selection/codegen)
    // 4. registry.run → executor.execute (complete_chat) → "Hello! How are you?"
    // 5. memory_writer::process_interaction → complete → empty actions
    let backend = MockBackend::new(vec![
        "NO",                                                // triage
        "Hello! How are you?",                               // task execution
        r#"{"save": [], "update": {}, "delete": []}"#,      // memory writer
    ]);

    // Simulate the pipeline manually (since run_task is in main.rs binary)
    let fast = &backend as &dyn ModelBackend;
    let memories = memory_provider::select_memories("hi there", &memory_store, fast)
        .await
        .unwrap();
    assert!(memories.is_empty());

    let needs_tools = triage::needs_external_data("hi there", fast, None, &memories)
        .await
        .unwrap();
    assert!(!needs_tools);

    // No tools → empty context
    let assembler = ContextAssembler::new(client);
    let mut context = assembler.assemble("hi there", &[]).await.unwrap();

    let memory_context = memory_provider::memories_to_context(&memories);
    if let Some(obj) = context.data.as_object_mut() {
        obj.insert("memories".to_string(), memory_context);
    }

    let mut task = Task::new("hi there");
    task.model_role = "default".to_string();
    let id = registry.create(task).await;

    // Use a separate mock for execution since the backend trait doesn't know roles
    // In real code, ModelRouter dispatches. Here we directly test with the mock
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

    // Memory writer
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

    // Pre-populate toolbox with a tool that returns static data
    let meta = ToolMeta {
        name: "time_lookup".to_string(),
        description: "Get current time for a timezone".to_string(),
        provides: vec!["time_lookup".to_string()],
        validated: true,
    };
    toolbox
        .save_tool(&meta, r#"return { time = "15:30 UTC" }"#)
        .unwrap();

    // Pipeline calls:
    // 1. memory selection → empty store, no backend call
    // 2. triage → YES
    // 3. tool selection → picks time_lookup with TIMEZONE param
    // 4. context assembly → executes Lua
    // 5. execution → model answer
    // 6. memory writer → save timezone preference
    let backend = MockBackend::new(vec![
        "YES",                                                            // triage
        r#"{"tools": ["time_lookup"], "params": {"TIMEZONE": "UTC"}}"#,   // tool selection
        "The time is 15:30 UTC.",                                         // execution
        r#"{"save": ["User uses UTC timezone"], "update": {}, "delete": []}"#, // memory writer
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
    assert_eq!(selection.tools, vec!["time_lookup"]);

    // Context assembly
    let mut assembler = ContextAssembler::new(client);
    assembler.set_params(selection.params);
    let provider = toolbox.load_provider("time_lookup").unwrap();
    assembler.add_provider(provider);

    let mut context = assembler
        .assemble("what time is it?", &selection.tools)
        .await
        .unwrap();
    assert_eq!(context.data["time_lookup"]["time"], "15:30 UTC");

    let memory_context = memory_provider::memories_to_context(&memories);
    if let Some(obj) = context.data.as_object_mut() {
        obj.insert("memories".to_string(), memory_context);
    }

    // Execute (using mock as direct backend)
    let response = fast
        .complete(format!("Context: {}\n\nTask: what time is it?", context.data))
        .await
        .unwrap();
    assert_eq!(response, "The time is 15:30 UTC.");

    // Memory writer saves preference
    memory_writer::process_interaction("what time is it?", &response, &memory_store, fast)
        .await
        .unwrap();

    let saved = memory_store.list().unwrap();
    assert_eq!(saved.len(), 1);
    assert_eq!(saved[0].fact, "User uses UTC timezone");
}
