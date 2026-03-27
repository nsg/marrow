# CLAUDE.md — Project Spec

## Project Overview

A lean, open source agent framework for personal and small business workflow automation.
Model-agnostic, provider-agnostic, and integration-agnostic by design. No hardcoded
integrations. No black boxes. Everything observable.

The system monitors inputs (calendar, communications, tasks, etc.), understands user intent
through conversation, dynamically generates the tools it needs to act, and self-heals when
things break.

---

## Core Philosophy

- **No hardcoded integrations — ever.** All connectors and tools are generated dynamically
  by the model at runtime. The system ships with zero provider-specific code.
- **Lean, task-specific context.** Context is assembled per task from only what that task
  requires. No passive context accumulation. No "context rot."
- **No black boxes.** Every action, correction, and decision is visible to the user.
  Detail is progressive — summary by default, full detail on request.
- **Self-healing.** A background janitor process monitors running tools, catches failures,
  attempts automatic repair, and escalates clearly when it cannot fix something.
- **Model-agnostic.** The system routes tasks to the most appropriate model. Different
  models handle different responsibilities. No lock-in to any single provider.

---

## Architecture

### 1. Orchestration Layer
- Manages task lifecycle: creation, context assembly, execution, monitoring
- Routes tasks to the appropriate model and tool
- Keeps tasks isolated — each task only sees what it needs
- Maintains a task registry with status, context definition, and tool references

### 2. Context Assembly
- Context is constructed dynamically per task, not accumulated passively
- Two types of context:
  - **Ephemeral:** assembled fresh for each task execution
  - **Persistent:** structured, queryable memory (user preferences, learned patterns)
- The model determines what context a task needs based on the task description
- Users can review and adjust context scope via plain language

### 3. Tool Generation Layer *(already prototyped)*
- When a workflow requires an integration or capability, the model writes the tool
- Tools are sandboxed with strict limits on IO, system calls, and execution time
- Generated tools are stored, versioned, and reusable across tasks
- No tool is trusted until the janitor has validated it

### 4. Janitor (Self-Healing Monitor)
- Background process that runs continuously
- Responsibilities:
  - Validate newly generated tools before they run in production
  - Monitor running tools for errors, unexpected outputs, and failures
  - Attempt automatic repair when a tool breaks
  - Escalate to the user in plain language when repair is not possible
- **Two modes only:** fix it silently, or stop and tell the user clearly
- All janitor activity is logged and visible — never silent beyond a status indicator

### 5. Transparency & Observability Layer
- Every action the system takes is recorded
- UI surfaces a simple status per workflow: healthy / fixed / needs attention
- Users can tap/click for progressive detail at any level:
  - What ran
  - What the tool did
  - What the janitor found and did
  - Full logs if desired
- Developers get full log access

### 6. Model Routing
- Different models handle different responsibilities:
  - **Chat / intent layer:** fast, cheap model for understanding user requests
  - **Tool calling layer:** reliable instruction-following model
  - **Code generation layer:** strong coding model for tool generation
  - **Janitor layer:** analytical model for error diagnosis and repair
- Target open/flexible models (e.g. Mistral, LLaMA, Qwen, DeepSeek families)
  for control, cost, and stability
- Model configuration is explicit and user-adjustable — no hidden routing

---

## First Use Case (Dogfood)

Build this system around the author's own daily workflow needs:

- Morning schedule summary from calendar
- Auto-generate todos and reminders from meetings and messages
- Cross-match across services (calendar, Slack, task managers, etc.)
- Surface what matters, create actions, stay out of the way

This is the validation harness. If the system can handle this reliably and
transparently for a technical user, it is ready to be tested by others.

---

## What This Is Not

- Not a chatbot platform
- Not a general-purpose AI assistant
- Not a plugin marketplace
- Not cloud-dependent
- Not tied to any specific LLM provider, calendar provider, or task manager

---

## Build Priorities

1. **Janitor** — the self-healing monitor is the most critical component.
   Without it, the no-hardcoding philosophy is not safe enough to trust.
2. **Context assembly** — lean, task-scoped context construction per workflow
3. **Transparency layer** — status visibility and progressive detail UI
4. **Model routing** — explicit, observable task-to-model assignment
5. **Tool generation layer** — already prototyped, integrate and harden

---

## Open Source Strategy

- Publish early, solve the author's own problem first
- Invite early users to test alongside development
- Use real usage to validate before any commercialization
- Potential future monetization: managed installs, consulting, hosted SaaS
- Never compromise the core open source model for commercial reasons

---

## Key Design Constraints

- Every integration must be generated, never hardcoded
- Every failure must be visible to the user at some level
- Every model routing decision must be inspectable
- Context scope must be auditable per task
- The system must be runnable by a non-technical user
  (setup complexity is acceptable; daily usage must not require technical knowledge)
