use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser;
use reqwest::Client;

use marrow::answer_check;
use marrow::context::{ContextAssembler, Stage};
use marrow::events::{Event, EventLog};
use marrow::memory::MemoryStore;
use marrow::memory_provider;
use marrow::memory_writer;
use marrow::registry::TaskRegistry;
use marrow::router::{ModelRouter, RouterConfig};
use marrow::session::{ChatSession, Message};
use marrow::task::Task;
use marrow::tool_selection::{self, PriorAttempt, SelectionResult};
use marrow::toolbox::Toolbox;
use marrow::triage;
use marrow::{codegen, janitor};

const MAX_REPLAN_ATTEMPTS: u32 = 2;

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

/// Select tools (with optional re-plan context), generate missing tools, assemble context.
#[allow(clippy::too_many_arguments)]
async fn select_and_assemble(
    description: &str,
    task_id: &str,
    router: &ModelRouter,
    toolbox: &Toolbox,
    client: Arc<Client>,
    log: &EventLog,
    fast_backend: &dyn marrow::model::ModelBackend,
    history_ref: Option<&[Message]>,
    prior_attempt: Option<&PriorAttempt>,
) -> Result<
    (SelectionResult, marrow::executor::Context),
    Box<dyn std::error::Error + Send + Sync>,
> {
    let available_tools = toolbox.list_tools().unwrap_or_default();
    let mut selection = tool_selection::select_tools_with_retry_context(
        description,
        &available_tools,
        fast_backend,
        history_ref,
        prior_attempt,
    )
    .await?;

    let tool_names = selection.all_tool_names();

    log.emit(Event::ToolSelected {
        task_id: task_id.to_string(),
        tools: tool_names.clone(),
    })
    .await;

    // Generate any tools that don't exist yet
    let code_backend = router
        .backend("code")
        .or_else(|_| router.backend("default"))?;

    for name in &tool_names {
        if toolbox.load_meta(name).is_err() {
            match codegen::generate_provider(description, code_backend, toolbox, client.clone())
                .await
            {
                Ok(generated_name) => {
                    log.emit(Event::ToolGenerated {
                        name: generated_name.clone(),
                        description: description.to_string(),
                    })
                    .await;
                    // If the model named it differently, update the stage
                    if &generated_name != name {
                        for stage in &mut selection.stages {
                            if let Some(params) = stage.tools.remove(name) {
                                stage.tools.insert(generated_name.clone(), params);
                            }
                        }
                    }
                }
                Err(e) => {
                    eprintln!("[marrow] code generation for '{name}' failed: {e}");
                }
            }
        }
    }

    // If selection was empty and no prior attempt, try generating a single tool
    if selection.is_empty() && prior_attempt.is_none() && !description.trim().is_empty() {
        match codegen::generate_provider(description, code_backend, toolbox, client.clone()).await {
            Ok(name) => {
                log.emit(Event::ToolGenerated {
                    name: name.clone(),
                    description: description.to_string(),
                })
                .await;
                let mut tools = std::collections::HashMap::new();
                tools.insert(name, std::collections::HashMap::new());
                selection.stages = vec![Stage { tools }];
            }
            Err(e) => {
                eprintln!("[marrow] code generation failed: {e}");
            }
        }
    }

    // Load providers and assemble context
    let all_tools = selection.all_tool_names();
    let mut assembler = ContextAssembler::new(client);
    for name in &all_tools {
        match toolbox.load_provider(name) {
            Ok(provider) => assembler.add_provider(provider),
            Err(e) => eprintln!("[marrow] failed to load provider '{name}': {e}"),
        }
    }

    let context = assembler
        .assemble(description, &selection.stages)
        .await?;

    log.emit(Event::ContextAssembled {
        task_id: task_id.to_string(),
        providers: all_tools,
    })
    .await;

    Ok((selection, context))
}

#[allow(clippy::too_many_arguments)]
async fn run_task(
    description: &str,
    role: &str,
    registry: &TaskRegistry,
    router: &ModelRouter,
    toolbox: &Toolbox,
    memory_store: &MemoryStore,
    client: Arc<Client>,
    log: &EventLog,
    session: Option<&ChatSession>,
) -> Result<serde_json::Value, Box<dyn std::error::Error + Send + Sync>> {
    let task_id = uuid::Uuid::new_v4().to_string();

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

    // Step 2: Triage
    let needs_tools =
        triage::needs_external_data(description, fast_backend, history_ref, &memories).await?;

    // Step 3-6: Select tools, assemble context, execute — with retry loop
    let mut prior_attempt: Option<PriorAttempt> = None;
    let mut final_result: Option<Result<serde_json::Value, Box<dyn std::error::Error + Send + Sync>>> = None;

    let max_attempts = if needs_tools {
        MAX_REPLAN_ATTEMPTS + 1
    } else {
        1
    };

    for attempt in 0..max_attempts {
        // Tool selection + context assembly
        let (selection, mut context) = if needs_tools {
            select_and_assemble(
                description,
                &task_id,
                router,
                toolbox,
                client.clone(),
                log,
                fast_backend,
                history_ref,
                prior_attempt.as_ref(),
            )
            .await?
        } else {
            let assembler = ContextAssembler::new(client.clone());
            let context = assembler.assemble(description, &[]).await?;
            (SelectionResult { stages: Vec::new() }, context)
        };

        // Inject memories
        if let Some(obj) = context.data.as_object_mut() {
            obj.insert("memories".to_string(), memory_context.clone());
        }

        // Execute
        let history = session.map(|s| s.build_messages(None));
        let history_slice = history.as_deref();

        let mut task = Task::new(description);
        task.model_role = role.to_string();
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

        // If execution failed, don't retry — that's a system error
        let output = match result {
            Ok(output) => output,
            Err(e) => {
                final_result = Some(Err(e.into()));
                break;
            }
        };

        // Check if we should retry (only if tools were used and we have retries left)
        let response_text = output.as_str().unwrap_or("");
        let is_last_attempt = attempt >= MAX_REPLAN_ATTEMPTS;

        if needs_tools && !selection.is_empty() && !is_last_attempt {
            let tool_output_summary = format!("{}", context.data);

            let check = answer_check::check_answer(
                description,
                &tool_output_summary,
                response_text,
                fast_backend,
            )
            .await;

            if let Ok(check_result) = check
                && !check_result.answered
            {
                let tool_outputs: Vec<(String, String)> = selection
                    .all_tool_names()
                    .into_iter()
                    .map(|name| {
                        let output = context
                            .data
                            .get(&name)
                            .map(|v| v.to_string())
                            .unwrap_or_default();
                        (name, output)
                    })
                    .collect();

                prior_attempt = Some(PriorAttempt {
                    tool_outputs,
                    reason: check_result.reason.clone(),
                });

                log.emit(Event::Replanning {
                    task_id: task_id.clone(),
                    attempt: attempt + 1,
                    reason: check_result.reason,
                })
                .await;

                continue;
            }
        }

        final_result = Some(Ok(output));
        break;
    }

    let result = final_result.unwrap_or_else(|| {
        Err("max re-plan attempts exceeded".into())
    });

    // Post-task memory writer
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

    result
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

                    // Append to session history
                    session.append(Message::user(input));
                    session.append(Message::assistant(&text));

                    // Summarize if needed
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
