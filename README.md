<div align="center">
  <h1>Marrow</h1>
  <p>A lean, open source agent framework for personal and small business workflow automation.</p>
</div>

## About

Marrow is a model-agnostic, provider-agnostic agent framework that monitors your inputs (calendar, communications, tasks), understands intent through conversation, dynamically generates the tools it needs to act, and self-heals when things break.

No hardcoded integrations. No black boxes. Everything observable.

## Features

- **Zero hardcoded integrations** — all connectors and tools are generated dynamically by the model at runtime
- **Lean, task-specific context** — context is assembled per task from only what that task requires, no passive accumulation
- **Self-healing** — a background janitor process monitors tools, catches failures, attempts automatic repair, and escalates clearly when it can't
- **Model-agnostic routing** — different models handle different responsibilities (chat, tool calling, code generation, diagnostics), all configurable and inspectable
- **Full transparency** — every action, correction, and decision is visible with progressive detail

## Architecture

| Layer | Responsibility |
|---|---|
| **Orchestration** | Task lifecycle, routing, isolation, and registry |
| **Context Assembly** | Per-task ephemeral and persistent context construction |
| **Tool Generation** | Model-written, sandboxed, versioned, reusable tools |
| **Janitor** | Continuous self-healing monitor — validate, repair, or escalate |
| **Transparency** | Status visibility and progressive detail at every level |
| **Model Routing** | Explicit, user-adjustable task-to-model assignment |

## Quick Start

```sh
cargo build
cargo run
```

## License

MIT
