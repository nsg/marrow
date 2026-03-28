# CLAUDE.md — Project Spec

## Project Overview

Marrow — a lean, open source agent framework for personal and small business
workflow automation. Model-agnostic, provider-agnostic, integration-agnostic.
No hardcoded integrations. No black boxes. Everything observable.

---

## Core Philosophy

- **No hardcoded integrations — ever.** All connectors are generated at runtime.
- **Lean, task-specific context.** No passive accumulation. No "context rot."
- **No black boxes.** Progressive detail — summary by default, full on request.
- **Self-healing.** Fix it silently, or stop and tell the user clearly.
- **Model-agnostic.** No lock-in to any single provider.

---

## Current State

Working prototype. Core architecture implemented and validated end-to-end
against Ollama Cloud. Zero clippy warnings, zero dead code.
Model research in `docs/model-benchmarks-2026-03.md` — GLM-5 (default/code)
and Kimi K2.5 (fast) selected based on speed, intelligence index, and tool calling benchmarks.

### What's built
- Cargo workspace: `marrow` (library), `marrow-cli` (CLI), `marrow-discord` (Discord bot)
- Agentic loop: triage → model decides actions (call tool, create tool, answer) iteratively
- Lua sandbox with Rust host functions for all external access
- `run_tool(name, params)` host function — tools can call other tools directly in Lua
- `secret(name)` host function — tools access API keys from `secrets.toml`
- Runtime tool generation — model creates tools on demand when none exist for the task
- Janitor (async background review, regeneration, escalation after 3 attempts)
- Working memory (JSON files, model-selected per task, auto-saved post-interaction)
- Conversation history with automatic summarization
- Structured JSONL event logging with `--verbose` progressive detail
- Ollama backend (local + cloud)
- Discord gateway (serenity) — responds to @mentions, DMs, and configured channels

### What's not built yet
- **Background/scheduled tasks** — no scheduler, triggers, or cron. Currently prompt-driven only
- **Long-term memory** — vector-backed store for deeper patterns. Ollama Cloud doesn't
  support embeddings yet, so deferred. Working memory covers short-term facts
- **Multiple providers** — only Ollama. OpenAI-compatible backend is a quick add when needed
- **Tests** — unit tests for core logic and integration tests with MockBackend

---

## Design Decisions

Context that isn't obvious from the code:

- **Lua for generated tools** — chosen for easy sandboxing, low token cost for generation,
  and the "code toolbox" pattern where the janitor maintains quality
- **Triage before tool selection** — a separate "needs external data?" model call prevents
  the tool selection model from generating unnecessary tools for conversational prompts.
  Rule-based heuristics were rejected as brittle and against the no-hardcoding philosophy
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

- Every integration must be generated, never hardcoded
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
