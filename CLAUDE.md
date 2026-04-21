# CLAUDE.md — Project Spec

## Project Overview

Marrow — a lean, open source agent framework for personal and small business
workflow automation. Model-agnostic, provider-agnostic. Two-tier tool system:
built-in Rust tools for reliable integrations, runtime Lua generation for
everything else. No black boxes. Everything observable.

---

## Core Philosophy

- **Built-in tools for reliability, Lua generation for flexibility.** Core and
  vendor-specific integrations ship as built-in Rust tools. The model can still
  generate Lua tools at runtime for anything not covered.
- **Lean, task-specific context.** No passive accumulation. No "context rot."
- **No black boxes.** Progressive detail — summary by default, full on request.
- **Self-healing.** Fix it silently, or stop and tell the user clearly.
- **Model-agnostic.** No lock-in to any single provider.

---

## Current State

Working prototype. Core architecture implemented and validated end-to-end
against Ollama Cloud. Zero clippy warnings, zero dead code.
Model research in `docs/model-benchmarks-2026-03.md` — GLM-5
(`default`/`code`) and Kimi K2.5 (`fast`) selected based on speed,
intelligence index, and tool calling benchmarks.

### What's built
- Cargo workspace: `marrow` (library), `marrow-cli` (CLI), `marrow-discord` (Discord bot)
- Agentic loop: model decides actions (answer, inline Lua, call tool, create tool) iteratively
- **Two-tier tool system**: built-in Rust tools + runtime-generated Lua tools, unified behind
  `ToolRegistry`. Agent sees one list, doesn't know which is which
- Built-in tools in `marrow/src/tools/` — implement the `Tool` trait, compiled into the binary.
  Currently ships: `rss_feed` (RSS/Atom reader with topic filtering)
- Runtime Lua tool generation — model creates tools on demand when none exist for the task
- Inline Lua execution — model writes code blocks that run directly in the sandbox
- Lua sandbox with Rust host functions for all external access
- `run_tool(name, params)` host function — tools can call other tools (built-in or Lua)
- Described secrets store (`secrets.toml`) — agent sees names + descriptions, passes
  them as `secret:name` tool params; execution layer resolves to actual values
- `secret(name)` host function — Lua tools can also access secrets directly
- Janitor (async background review, regeneration, escalation after 3 attempts) — Lua tools only
- Working memory (JSON files, model-selected per task, auto-saved post-interaction)
- Conversation history with automatic summarization
- Structured JSONL event logging with `--verbose` progressive detail
- Ollama and OpenAI-compatible backends
- Discord gateway (serenity) — responds to @mentions, DMs, and configured channels

### What's not built yet
- **Background/scheduled tasks** — no scheduler, triggers, or cron. Currently prompt-driven only
- **Long-term memory** — vector-backed store for deeper patterns. Ollama Cloud doesn't
  support embeddings yet, so deferred. Working memory covers short-term facts

---

## Design Decisions

Context that isn't obvious from the code:

- **Two-tier tools: built-in Rust + generated Lua** — current models aren't reliable enough
  to generate tools on the fly for critical workflows. Built-in Rust tools (`marrow/src/tools/`)
  provide reliable baselines; Lua generation remains for the future when models improve.
  Both tiers are unified behind `ToolRegistry` — the agent sees one merged list and the
  dispatch is transparent. Built-in tools shadow Lua tools with the same name. Lua `run_tool()`
  can call built-in tools and vice versa. The janitor only manages Lua tools (built-ins
  don't need healing)
- **Lua for generated tools** — chosen for easy sandboxing, low token cost for generation,
  and the "code toolbox" pattern where the janitor maintains quality
- **Single agent loop over staged orchestration** — one planner owns the decision to answer,
  run inline Lua, reuse a tool, or generate a new one. That replaced the earlier staged
  design because it was simpler to share across frontends and drifted less in practice
- **Working memory vs long-term** — two planned tiers. Working memory (now) is JSON files
  with model-based selection. Long-term memory (future) is vector DB with embeddings,
  deferred until Ollama Cloud supports embedding models or a local model is added
- **Janitor uses code role** — reviews and regenerates using the code model, not analytical.
  It's reviewing code, so the code model is the right fit
- **Test before save** — generated Lua is test-run in the sandbox before persisting to toolbox.
  Prevents broken tools from polluting the toolbox
- **Janitor deletes unfixable tools** — after 3 failed fix attempts, the janitor escalates
  (logged as event) and deletes the tool from the toolbox. Auto-generated tools are disposable;
  they'll be regenerated fresh if needed again
- **run_tool for composition** — tools compose via `run_tool()` in Lua, not via orchestrator
  staging. Keeps composition logic in testable code, avoids multi-model-call orchestration
  that hallucinated in practice. Data tools do one thing; glue tools compose them.
  Recursion depth capped at 5. New sandbox per nested call for isolation
- **Two frontends, different audiences** — Discord is the human-facing frontend (the author
  uses it daily). The CLI is primarily for agentic usage — Claude Code and other agents use
  it to test tools, verify behavior, and validate changes. The human may also use the CLI
  occasionally, but optimize CLI design decisions for automated/agentic workflows first

---

## CLI Usage (for agents)

Use the CLI to test the agent loop, verify tools, and run the janitor.
Build with `cargo build --bin marrow`, run `target/debug/marrow --help` for options.
stdout is the response, stderr is progress and diagnostics.

`--list-tools` shows all available tools (both built-in Rust tools and Lua toolbox tools).

---

## Dogfood Use Case

Build around the author's daily workflow:

- Morning schedule summary from calendar
- Auto-generate todos and reminders from meetings and messages
- Cross-match across services (calendar, Slack, task managers, etc.)
- Surface what matters, create actions, stay out of the way

Not started yet — infrastructure supports it, needs real-world API testing.

---

## Key Constraints

- **Use existing infrastructure** — before writing any parsing, formatting, or utility
  code, check what already exists in the codebase. XML parsing uses `xml.rs` (quick_xml
  with namespace resolution). Never hand-roll parsers when a proper one is available.
- Built-in tools cover both standard protocols and vendor-specific services
- Every failure must be visible to the user at some level
- Every model routing decision must be inspectable
- Context scope must be auditable per task
- Daily usage must not require technical knowledge

---

## Open Source Strategy

- Publish early, solve the author's own problem first
- Invite early users to test alongside development
- Use real usage to validate before any commercialization
- Never compromise the core open source model for commercial reasons
