use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser;
use reqwest::Client;

use marrow::events::{Event, EventLog};
use marrow::executor::Context;
use marrow::memory::MemoryStore;
use marrow::memory_provider;
use marrow::memory_writer;
use marrow::registry::TaskRegistry;
use marrow::router::{ModelRouter, RouterConfig};
use marrow::session::{ChatSession, Message};
use marrow::task::Task;
use marrow::tool_selection;
use marrow::toolbox::Toolbox;
use marrow::triage;
use marrow::{codegen, janitor};

#[derive(Parser)]
#[command(
    name = "marrow",
    about = "A lean agent framework for workflow automation"
)]
struct Cli {
    #[arg(short, long, default_value = "config.toml")]
    config: String,

    #[arg(short, long, default_value = "default")]
    role: String,

    /// Run a single prompt and exit
    #[arg(short = 'p', long)]
    prompt: Option<String>,

    /// Path to the toolbox directory
    #[arg(short, long, default_value = "toolbox")]
    toolbox: String,

    /// Path to the memory directory
    #[arg(short, long, default_value = "memory")]
    memory: String,

    /// Show full event stream
    #[arg(short, long)]
    verbose: bool,

    /// Path to event log file
    #[arg(long, default_value = "events.jsonl")]
    log: String,
}

#[allow(clippy::too_many_arguments)]
async fn run_task(
    description: &str,
    role: &str,
    registry: &TaskRegistry,
    router: &ModelRouter,
    toolbox: &Toolbox,
    toolbox_path: &str,
    memory_store: &MemoryStore,
    client: Arc<Client>,
    log: &EventLog,
    session: Option<&ChatSession>,
) -> Result<serde_json::Value, Box<dyn std::error::Error + Send + Sync>> {
    let mut task = Task::new(description);
    task.model_role = role.to_string();
    let task_id = task.id.to_string();

    log.emit(Event::TaskCreated {
        task_id: task_id.clone(),
        description: description.to_string(),
        role: role.to_string(),
    })
    .await;

    let fast_backend = router
        .backend("fast")
        .or_else(|_| router.backend("default"))?;
    let history_msgs = session.map(|s| s.build_messages(None));
    let history_ref = history_msgs.as_deref();

    // Step 1: Retrieve relevant memories
    let memories =
        memory_provider::select_memories(description, memory_store, fast_backend).await?;
    let memory_context = memory_provider::memories_to_context(&memories);

    // Step 2: Triage — does this task need external data?
    let needs_tools =
        triage::needs_external_data(description, fast_backend, history_ref, &memories).await?;

    // Step 3: Tool selection + generation + execution
    let tool_context = if needs_tools {
        let available_tools = toolbox.list_tools().unwrap_or_default();
        let selection =
            tool_selection::select_tools(description, &available_tools, fast_backend, history_ref, &memories)
                .await?;

        let tool_name = if let Some(name) = &selection.tool {
            log.emit(Event::ToolSelected {
                task_id: task_id.clone(),
                tools: vec![name.clone()],
            })
            .await;

            // Generate if missing
            if toolbox.load_meta(name).is_err() {
                let code_backend = router
                    .backend("code")
                    .or_else(|_| router.backend("default"))?;
                let request = codegen::ToolRequest {
                    name: name.clone(),
                    expected_params: selection.params.clone(),
                };
                match codegen::generate_provider_with_hint(
                    description,
                    code_backend,
                    toolbox,
                    client.clone(),
                    Some(&request),
                    &available_tools,
                )
                .await
                {
                    Ok(generated) => {
                        log.emit(Event::ToolGenerated {
                            name: generated,
                            description: description.to_string(),
                        })
                        .await;
                    }
                    Err(e) => {
                        eprintln!("[marrow] code generation for '{name}' failed: {e}");
                    }
                }
            }

            Some(name.clone())
        } else if !description.trim().is_empty() {
            // No tool selected — try generating one
            log.emit(Event::ToolSelected {
                task_id: task_id.clone(),
                tools: vec![],
            })
            .await;

            let code_backend = router
                .backend("code")
                .or_else(|_| router.backend("default"))?;
            match codegen::generate_provider(description, code_backend, toolbox, client.clone())
                .await
            {
                Ok(name) => {
                    log.emit(Event::ToolGenerated {
                        name: name.clone(),
                        description: description.to_string(),
                    })
                    .await;
                    Some(name)
                }
                Err(e) => {
                    eprintln!("[marrow] code generation failed: {e}");
                    None
                }
            }
        } else {
            None
        };

        // Execute the tool
        if let Some(ref name) = tool_name {
            match toolbox.load_provider(name) {
                Ok(provider) => {
                    let toolbox_dir = Some(PathBuf::from(toolbox_path));
                    match provider
                        .execute_with_params(description, client.clone(), &selection.params, toolbox_dir)
                        .await
                    {
                        Ok(value) => {
                            log.emit(Event::ContextAssembled {
                                task_id: task_id.clone(),
                                providers: vec![name.clone()],
                            })
                            .await;
                            Some(value)
                        }
                        Err(e) => {
                            eprintln!("[marrow] tool '{name}' failed: {e}");
                            Some(serde_json::json!({ "error": e.to_string() }))
                        }
                    }
                }
                Err(e) => {
                    eprintln!("[marrow] failed to load tool '{name}': {e}");
                    None
                }
            }
        } else {
            None
        }
    } else {
        None
    };

    // Step 4: Build context
    let mut context_data = serde_json::Map::new();
    if let Some(tool_output) = tool_context {
        context_data.insert("tool_output".to_string(), tool_output);
    }
    context_data.insert("memories".to_string(), memory_context);
    let context = Context::new(serde_json::Value::Object(context_data));

    // Step 5: Execute task
    let history = session.map(|s| s.build_messages(None));
    let history_slice = history.as_deref();

    let id = registry.create(task).await;
    let result = registry.run(id, router, &context, history_slice).await;

    let status = if result.is_ok() {
        "succeeded"
    } else {
        "failed"
    };

    log.emit(Event::TaskExecuted {
        task_id: task_id.clone(),
        status: status.to_string(),
    })
    .await;

    // Step 6: Post-task memory writer
    if let Ok(ref output) = result {
        let response_text = output.as_str().unwrap_or("");
        if let Err(e) = memory_writer::process_interaction(
            description,
            response_text,
            memory_store,
            fast_backend,
        )
        .await
        {
            eprintln!("[marrow] memory writer error: {e}");
        }
    }

    result.map_err(|e| e.into())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let cli = Cli::parse();

    let config = RouterConfig::from_file(&cli.config)?;
    let router = ModelRouter::from_config(&config)?;
    let registry = TaskRegistry::new();
    let client = Arc::new(Client::new());
    let toolbox = Toolbox::new(&cli.toolbox);
    let memory_store = MemoryStore::new(&cli.memory);
    let log = EventLog::new(Some(PathBuf::from(&cli.log)), cli.verbose).await?;

    // Spawn janitor in background
    let janitor_backend = config
        .build_backend("code")
        .or_else(|_| config.build_backend("default"))?;
    let janitor_toolbox = Toolbox::new(&cli.toolbox);
    let janitor_log = log.clone();
    tokio::spawn(async move {
        janitor::run(&janitor_toolbox, janitor_backend.as_ref(), &janitor_log).await;
    });

    if let Some(prompt) = cli.prompt {
        match run_task(
            &prompt,
            &cli.role,
            &registry,
            &router,
            &toolbox,
            &cli.toolbox,
            &memory_store,
            client,
            &log,
            None,
        )
        .await
        {
            Ok(output) => {
                if let Some(text) = output.as_str() {
                    println!("{text}");
                } else {
                    println!("{output}");
                }
            }
            Err(e) => {
                eprintln!("error: {e}");
                std::process::exit(1);
            }
        }
    } else {
        println!("marrow ready (role: {})", cli.role);
        println!("type 'quit' to exit\n");

        let mut session = ChatSession::new();
        let fast_backend = router
            .backend("fast")
            .or_else(|_| router.backend("default"))?;

        let stdin = io::stdin();
        loop {
            print!("> ");
            io::stdout().flush()?;

            let mut input = String::new();
            let bytes = stdin.read_line(&mut input)?;
            if bytes == 0 {
                break;
            }
            let input = input.trim();

            if input.is_empty() {
                continue;
            }
            if input == "quit" {
                break;
            }

            match run_task(
                input,
                &cli.role,
                &registry,
                &router,
                &toolbox,
                &cli.toolbox,
                &memory_store,
                client.clone(),
                &log,
                Some(&session),
            )
            .await
            {
                Ok(output) => {
                    let text = output.as_str().unwrap_or("").to_string();
                    println!("\n{text}\n");

                    session.append(Message::user(input));
                    session.append(Message::assistant(&text));

                    if session.needs_summarization()
                        && let Err(e) = session.summarize(fast_backend).await
                    {
                        eprintln!("[marrow] summarization error: {e}");
                    }
                }
                Err(e) => eprintln!("\nerror: {e}\n"),
            }
        }
    }

    Ok(())
}
