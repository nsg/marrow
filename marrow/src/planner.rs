use std::error::Error;

use crate::agent::{self, LoopConfig, LoopResult, Outcome, ProgressUpdate};
use crate::events::Event;
use crate::memory::Memory;
use crate::model::ModelBackend;
use crate::session::Message;

#[derive(Debug, Clone, serde::Deserialize)]
pub struct PlanItem {
    pub objective: String,
    pub criteria: String,
}

#[derive(Debug, Clone)]
pub struct Plan {
    pub items: Vec<PlanItem>,
}

#[derive(Debug, Clone)]
struct ItemResult {
    pub objective: String,
    pub summary: String,
    pub outcome_text: String,
    pub success: bool,
    pub loop_result: LoopResult,
}

#[derive(Debug, Clone, PartialEq)]
enum TriageDecision {
    Direct,
    NeedsPlan,
}

const TRIAGE_PROMPT: &str = r#"Classify this task as SIMPLE or COMPLEX.

SIMPLE: Can be done in 1-2 tool calls or answered from memory. Examples: "what time is it?", "what's the weather?", "remember my name", "check the RSS feed"
COMPLEX: Needs multiple independent data sources, multi-step work, or sequential operations. Examples: "compare the weather in 3 cities", "check my calendar and plan my commute", "fetch RSS feeds and summarize the top stories then email me"

Task: {task}
{conversation}
Reply with exactly one word: SIMPLE or COMPLEX"#;

const PLAN_PROMPT: &str = r#"Break this task into 2-5 sequential steps. Each step should accomplish one clear objective.

Task: {task}
{memories}{conversation}
Respond with ONLY a JSON array, no other text:
[
  {{"objective": "what to accomplish", "criteria": "how to know it's done"}},
  {{"objective": "...", "criteria": "..."}}
]

Rules:
- 2-5 items only. Prefer fewer.
- Each item must be independently completable.
- Later items can use results from earlier ones.
- Focus on WHAT, not HOW. The executor decides which tools to use.
- The final item should deliver the answer to the user."#;

const EVALUATE_PROMPT: &str = r#"Evaluate whether this step was completed.

Objective: {objective}
Criteria: {criteria}

Result:
{outcome}

Respond with ONLY a JSON object, no other text:
{{"success": true, "summary": "one-sentence summary of what was accomplished"}}"#;

pub async fn run_planned(
    config: LoopConfig<'_>,
) -> Result<LoopResult, Box<dyn Error + Send + Sync>> {
    let decision = triage(config.task, config.conversation, config.fast_backend).await;

    config
        .log
        .emit(Event::PlanTriageResult {
            task_id: config.task_id.to_string(),
            decision: match decision {
                TriageDecision::Direct => "direct".to_string(),
                TriageDecision::NeedsPlan => "needs_plan".to_string(),
            },
        })
        .await;

    if decision == TriageDecision::Direct {
        return agent::run_loop(config).await;
    }

    let plan = generate_plan(
        config.task,
        config.conversation,
        config.memories,
        config.fast_backend,
    )
    .await;

    if plan.items.len() <= 1 {
        return agent::run_loop(config).await;
    }

    config
        .log
        .emit(Event::PlanCreated {
            task_id: config.task_id.to_string(),
            item_count: plan.items.len() as u32,
        })
        .await;

    if config.log.is_verbose() {
        eprintln!("[planner] generated plan with {} items:", plan.items.len());
        for (i, item) in plan.items.iter().enumerate() {
            eprintln!("[planner]   {}: {}", i + 1, item.objective);
        }
    }

    let item_count = plan.items.len();
    let mut item_results: Vec<ItemResult> = Vec::new();
    let mut total_steps: u32 = 0;
    let mut total_lua_runs: u32 = 0;
    let mut all_timings = Vec::new();

    for (i, item) in plan.items.iter().enumerate() {
        let is_last = i == item_count - 1;

        if let Some(tx) = config.progress {
            let _ = tx.send(ProgressUpdate::Notification(format!(
                "Step {}/{}: {}",
                i + 1,
                item_count,
                item.objective
            )));
        }

        config
            .log
            .emit(Event::PlanItemStarted {
                task_id: config.task_id.to_string(),
                item_index: i as u32,
                objective: item.objective.clone(),
            })
            .await;

        let prior_context = if item_results.is_empty() {
            None
        } else {
            Some(build_prior_context(&item_results))
        };

        let item_task = if is_last {
            format!(
                "{}\n\nThis is the final step. Deliver your complete answer to the user.",
                item.objective
            )
        } else {
            item.objective.clone()
        };

        let item_config = LoopConfig {
            task: &item_task,
            task_id: config.task_id,
            backend: config.backend,
            fast_backend: config.fast_backend,
            registry: config.registry.clone(),
            client: config.client.clone(),
            memories: config.memories,
            skill_store: config.skill_store,
            log: config.log,
            secrets: config.secrets,
            progress: config.progress,
            conversation: config.conversation,
            incoming: None,
            formatting_hint: config.formatting_hint,
            schedule_store: config.schedule_store.clone(),
            memory_store: config.memory_store.clone(),
            frontend_context: None,
            frontend: config.frontend,
            max_steps: None,
            prior_context,
        };

        let loop_result = agent::run_loop(item_config).await?;

        total_steps += loop_result.steps;
        total_lua_runs += loop_result.lua_runs;
        all_timings.extend(loop_result.step_timings.clone());

        let outcome_text = match &loop_result.outcome {
            Outcome::Answer(text) => text.clone(),
            Outcome::Dismissed => "(no output)".to_string(),
        };

        let (success, summary) = evaluate_item(item, &outcome_text, config.fast_backend).await;

        if config.log.is_verbose() {
            eprintln!(
                "[planner] item {}/{} ({}): success={}, summary={}",
                i + 1,
                item_count,
                item.objective,
                success,
                summary
            );
        }

        config
            .log
            .emit(Event::PlanItemCompleted {
                task_id: config.task_id.to_string(),
                item_index: i as u32,
                success,
                steps_used: loop_result.steps,
            })
            .await;

        let item_result = ItemResult {
            objective: item.objective.clone(),
            summary: summary.clone(),
            outcome_text: outcome_text.clone(),
            success,
            loop_result,
        };

        if is_last {
            let final_outcome = item_result.loop_result.outcome.clone();
            item_results.push(item_result);
            return Ok(LoopResult {
                outcome: final_outcome,
                steps: total_steps,
                lua_runs: total_lua_runs,
                hit_step_limit: false,
                step_timings: all_timings,
            });
        }

        if !success {
            if config.log.is_verbose() {
                eprintln!(
                    "[planner] item {}/{} failed, retrying once with failure context",
                    i + 1,
                    item_count,
                );
            }

            let retry_prior = {
                let mut ctx = build_prior_context(&item_results);
                ctx.push_str(&format!(
                    "\nFailed attempt at \"{}\": {}\nTry a different approach.",
                    item.objective, summary
                ));
                Some(ctx)
            };

            let retry_config = LoopConfig {
                task: &item.objective,
                task_id: config.task_id,
                backend: config.backend,
                fast_backend: config.fast_backend,
                registry: config.registry.clone(),
                client: config.client.clone(),
                memories: config.memories,
                skill_store: config.skill_store,
                log: config.log,
                secrets: config.secrets,
                progress: config.progress,
                conversation: config.conversation,
                incoming: None,
                formatting_hint: config.formatting_hint,
                schedule_store: config.schedule_store.clone(),
                memory_store: config.memory_store.clone(),
                frontend_context: None,
                frontend: config.frontend,
                max_steps: None,
                prior_context: retry_prior,
            };

            let retry_result = agent::run_loop(retry_config).await?;
            total_steps += retry_result.steps;
            total_lua_runs += retry_result.lua_runs;
            all_timings.extend(retry_result.step_timings.clone());

            let retry_outcome = match &retry_result.outcome {
                Outcome::Answer(text) => text.clone(),
                Outcome::Dismissed => "(no output)".to_string(),
            };

            let (retry_success, retry_summary) =
                evaluate_item(item, &retry_outcome, config.fast_backend).await;

            item_results.push(ItemResult {
                objective: item.objective.clone(),
                summary: if retry_success {
                    retry_summary
                } else {
                    format!("Failed: {}", summary)
                },
                outcome_text: retry_outcome,
                success: retry_success,
                loop_result: retry_result,
            });
        } else {
            item_results.push(item_result);
        }
    }

    let final_answer = item_results
        .last()
        .and_then(|r| match &r.loop_result.outcome {
            Outcome::Answer(text) => Some(text.clone()),
            Outcome::Dismissed => None,
        })
        .unwrap_or_else(|| {
            item_results
                .iter()
                .map(|r| format!("- {}: {}", r.objective, r.summary))
                .collect::<Vec<_>>()
                .join("\n")
        });

    Ok(LoopResult {
        outcome: Outcome::Answer(final_answer),
        steps: total_steps,
        lua_runs: total_lua_runs,
        hit_step_limit: false,
        step_timings: all_timings,
    })
}

async fn triage(
    task: &str,
    conversation: &[Message],
    fast_backend: &dyn ModelBackend,
) -> TriageDecision {
    let conv_section = if conversation.is_empty() {
        String::new()
    } else {
        let lines = conversation
            .iter()
            .map(|m| format!("{}: {}", m.role, m.content))
            .collect::<Vec<_>>()
            .join("\n");
        format!("\nConversation context:\n{lines}\n")
    };

    let prompt = TRIAGE_PROMPT
        .replace("{task}", task)
        .replace("{conversation}", &conv_section);

    let response = match fast_backend.complete(prompt).await {
        Ok(r) => r,
        Err(_) => return TriageDecision::Direct,
    };

    if response.to_uppercase().contains("COMPLEX") {
        TriageDecision::NeedsPlan
    } else {
        TriageDecision::Direct
    }
}

async fn generate_plan(
    task: &str,
    conversation: &[Message],
    memories: &[Memory],
    fast_backend: &dyn ModelBackend,
) -> Plan {
    let memory_section = if memories.is_empty() {
        String::new()
    } else {
        let facts = memories
            .iter()
            .map(|m| format!("- {}", m.fact))
            .collect::<Vec<_>>()
            .join("\n");
        format!("Known facts:\n{facts}\n")
    };

    let conv_section = if conversation.is_empty() {
        String::new()
    } else {
        let lines = conversation
            .iter()
            .map(|m| format!("{}: {}", m.role, m.content))
            .collect::<Vec<_>>()
            .join("\n");
        format!("Conversation context:\n{lines}\n")
    };

    let prompt = PLAN_PROMPT
        .replace("{task}", task)
        .replace("{memories}", &memory_section)
        .replace("{conversation}", &conv_section);

    let response = match fast_backend.complete(prompt).await {
        Ok(r) => r,
        Err(_) => return fallback_plan(task),
    };

    match parse_plan_json(&response) {
        Some(items) if items.len() >= 2 => Plan { items },
        _ => fallback_plan(task),
    }
}

fn parse_plan_json(response: &str) -> Option<Vec<PlanItem>> {
    let text = response.trim();

    if let Ok(items) = serde_json::from_str::<Vec<PlanItem>>(text) {
        return Some(items);
    }

    // Try extracting JSON from markdown fences or surrounding text
    let start = text.find('[')?;
    let end = text.rfind(']')?;
    if end <= start {
        return None;
    }
    serde_json::from_str::<Vec<PlanItem>>(&text[start..=end]).ok()
}

fn fallback_plan(task: &str) -> Plan {
    Plan {
        items: vec![PlanItem {
            objective: task.to_string(),
            criteria: "Task completed and answer delivered".to_string(),
        }],
    }
}

async fn evaluate_item(
    item: &PlanItem,
    outcome_text: &str,
    fast_backend: &dyn ModelBackend,
) -> (bool, String) {
    let truncated = if outcome_text.len() > 2000 {
        &outcome_text[..2000]
    } else {
        outcome_text
    };

    let prompt = EVALUATE_PROMPT
        .replace("{objective}", &item.objective)
        .replace("{criteria}", &item.criteria)
        .replace("{outcome}", truncated);

    let response = match fast_backend.complete(prompt).await {
        Ok(r) => r,
        Err(_) => return (true, truncated.chars().take(200).collect()),
    };

    parse_evaluation(&response).unwrap_or_else(|| (true, truncated.chars().take(200).collect()))
}

fn parse_evaluation(response: &str) -> Option<(bool, String)> {
    let text = response.trim();

    #[derive(serde::Deserialize)]
    struct Eval {
        success: bool,
        summary: String,
    }

    if let Ok(eval) = serde_json::from_str::<Eval>(text) {
        return Some((eval.success, eval.summary));
    }

    let start = text.find('{')?;
    let end = text.rfind('}')?;
    if end <= start {
        return None;
    }
    let eval = serde_json::from_str::<Eval>(&text[start..=end]).ok()?;
    Some((eval.success, eval.summary))
}

const PRIOR_CONTEXT_ITEM_LIMIT: usize = 2000;

fn build_prior_context(results: &[ItemResult]) -> String {
    results
        .iter()
        .enumerate()
        .map(|(i, r)| {
            let status = if r.success { "done" } else { "failed" };
            let data = if r.outcome_text.len() > PRIOR_CONTEXT_ITEM_LIMIT {
                format!(
                    "{}… (truncated)",
                    &r.outcome_text[..PRIOR_CONTEXT_ITEM_LIMIT]
                )
            } else {
                r.outcome_text.clone()
            };
            format!(
                "Step {} [{}]: {}\nResult: {}",
                i + 1,
                status,
                r.objective,
                data
            )
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}
