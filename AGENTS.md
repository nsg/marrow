# Marrow — Project Spec

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

## Design Decisions

Context that isn't obvious from the code:

- **Single agent loop over staged orchestration** — one planner owns the decision to answer,
  run inline Lua, reuse a tool, or generate a new one. That replaced the earlier staged
  design because it was simpler to share across frontends and drifted less in practice
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

## Key Constraints

- **Use existing infrastructure** — before writing any parsing, formatting, or utility
  code, check what already exists in the codebase. XML parsing uses `xml.rs` (quick_xml
  with namespace resolution). Never hand-roll parsers when a proper one is available.
- Built-in tools cover both standard protocols and vendor-specific services

---

## CLI Usage (for agents)

Use the CLI to test the agent loop, verify tools, and run the janitor.
Build with `cargo build --bin marrow`, run `target/debug/marrow --help` for options.
stdout is the response, stderr is progress and diagnostics.

`--list-tools` shows all available tools (both built-in Rust tools and Lua toolbox tools).

---

## Release Flow

For tagged releases, follow this sequence:

1. Bump the workspace crate version in the root `Cargo.toml`
2. Run `cargo fmt`
3. Run `cargo clippy -- -D warnings`
4. Run `cargo build` to refresh `Cargo.lock` and verify the workspace builds
5. Run `cargo test`
6. Commit the release prep
7. Create an annotated git tag named `v<version>`

Keep crate versions inherited from `workspace.package` so future releases only
need one manifest version change.
