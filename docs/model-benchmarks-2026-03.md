# Ollama Cloud Model Research — March 2026

Model selection research for Marrow. All speed tests run against `https://ollama.com/api/chat`
with a simple prompt and 30s timeout on 2026-03-28.

## Models Evaluated

### GLM-5 (Zhipu AI) — Selected as default

| Metric | Score |
|---|---|
| Intelligence Index | **50** (highest of all tested) |
| Chatbot Arena ELO | **1451** |
| SWE-bench Verified | **77.8%** |
| AIME 2025 | 92.7% |
| GPQA-Diamond | 86.0% |
| Terminal-Bench (agentic) | **81.0** |
| Speed (no think) | 1277ms |
| Speed (think) | 8595ms |
| Active params | 40B (744B total MoE) |

**Strengths:** Smartest model available on Ollama Cloud. Best agentic performance (Terminal-Bench 81).
Best SWE-bench score. Strong disciplined step-by-step reasoning. Near-perfect math scores.

**Weaknesses:** Slow with thinking enabled (8.6s). Not the fastest without thinking either (1.3s).
Less tested in Western agent frameworks compared to Qwen/DeepSeek.

**Verdict:** Best choice for default and code roles where quality matters most.

---

### Kimi K2.5 (Moonshot AI) — Selected as fast

| Metric | Score |
|---|---|
| Intelligence Index | **47** |
| SWE-bench Verified | 76.8% |
| LiveCodeBench | **85%** (highest of all tested) |
| AIME 2025 | **96.1%** |
| GPQA-Diamond | 87.6% |
| Terminal-Bench (agentic) | 50.8 |
| Speed (no think) | 1084ms |
| Speed (think) | 1942ms |
| Active params | 32B (1T total MoE) |

**Strengths:** Purpose-built for agentic tool calling (near top of BFCL). Best LiveCodeBench (85%).
Best math (AIME 96.1%). Multimodal (vision). 262K context window. Can spawn up to 100 sub-agents.
Faster than GLM-5.

**Weaknesses:** Weaker on agentic tasks than GLM-5 (Terminal-Bench 50.8 vs 81). Slightly lower
Intelligence Index (47 vs 50).

**Verdict:** Best choice for fast role — smart enough for memory retrieval, summarization, and
lightweight structured tasks, with ~1s response time.

---

### Qwen3-Coder-Next (Alibaba/Qwen) — Considered, not selected

| Metric | Score |
|---|---|
| Intelligence Index | **27** |
| SWE-bench Verified | 70.6% |
| SWE-bench Pass@5 | 64.6% (#1 overall) |
| Speed (no think) | 721ms |
| Speed (think) | 967ms |
| Active params | 3B (80B total MoE) |

**Strengths:** Very fast (721ms). #1 on SWE-bench Pass@5 beating Claude Opus 4.6 and GPT-5.2.
Impressive efficiency — competes with models 10-20x larger. Code-specialized with strong
tool-calling support from Qwen lineage.

**Not selected because:** Intelligence Index of 27 is roughly half of GLM-5 (50). Single-pass
SWE-bench (70.6%) is lower than both GLM-5 (77.8%) and Kimi (76.8%). The 3B active params
make it fast but limit general intelligence. For Marrow's code generation (Lua tools that must
work correctly), quality matters more than speed.

---

### Qwen3-Next-80B (Alibaba/Qwen) — Considered, not selected

| Metric | Score |
|---|---|
| Intelligence Index | **27** |
| AIME 2025 | 69.5% |
| GPQA-Diamond | 72.9% |
| LiveCodeBench | 56.6% |
| Arena-Hard v2 | 82.7% |
| Speed (no think) | 557ms |
| Speed (think) | 2445ms |
| Active params | 3B (80B total MoE) |

**Strengths:** Fastest model tested (557ms). Novel hybrid architecture (Gated DeltaNet + MoE).
262K context, extensible to 1M tokens. Beats models 10x its active size on Arena-Hard.

**Not selected because:** Intelligence Index of 27 — same limitation as Coder-Next. Significantly
behind GLM-5 and Kimi on AIME (69.5% vs 92.7%/96.1%), GPQA (72.9% vs 86.0%/87.6%), and
overall intelligence. Too weak for reliable memory selection and other lightweight structured
runtime tasks despite being fast.

---

### Devstral-2 123B (Mistral) — Considered, not selected

| Metric | Score |
|---|---|
| Intelligence Index | **22** |
| SWE-bench Verified | 72.2% |
| Speed (no think) | 973ms |
| Speed (think) | 1013ms |
| Active params | ~123B dense |

**Strengths:** Fast (sub-1s). Code-focused. From Mistral, a reputable lab. Good SWE-bench for
an open model.

**Not selected because:** Intelligence Index of 22 — less than half of GLM-5. A coding-focused
model that's actually weaker at coding than GLM-5 (72.2% vs 77.8%) and Kimi (76.8%). Not smart
enough for reliable memory selection or other lightweight structured tasks.

---

### Cogito-2.1 671B (Deep Cogito) — Considered, not selected

| Metric | Score |
|---|---|
| Intelligence Index | N/A (not indexed) |
| MATH-500 | 98.57% |
| GPQA-Diamond | 77.72% |
| MMLU Pro | 84.69% |
| Speed (no think) | 1165ms |
| Speed (think) | 2363ms |
| Active params | ~37B (671B total MoE) |

**Strengths:** Excellent math (MATH-500 98.57%). Reasonable speed for a 671B model. Claims to
rival Claude 4 Opus on reasoning with 60% shorter chains.

**Not selected because:** No Intelligence Index score. "Notably weak agentic and coding composite
scores." New entrant with limited real-world validation. Unproven for tool calling and structured
output.

---

### DeepSeek-V3.2 — Considered, not selected

| Metric | Score |
|---|---|
| Intelligence Index | ~frontier |
| Speed (no think) | 1778ms |
| Speed (think) | 13828ms |

**Strengths:** Frontier-quality model. Battle-tested in agent frameworks. Strong tool calling.

**Not selected because:** Too slow, especially with thinking (13.8s). Otherwise would be a
strong contender.

---

### Qwen3.5 397B (Alibaba/Qwen) — Considered, not selected

| Metric | Score |
|---|---|
| Intelligence Index | **45** |
| SWE-bench Verified | 60% |
| Chatbot Arena ELO | 1450 |

**Not selected because:** Consistently times out on Ollama Cloud (>30s). Surprisingly weak on
SWE-bench (60%) despite being the largest Qwen model. Would be competitive if it were faster.

## Selected Configuration

| Role | Model | Speed | Why |
|---|---|---|---|
| **fast** | `kimi-k2.5:cloud` | 1084ms | Smart (47), best tool calling, good speed |
| **default** | `glm-5:cloud` | 1277ms | Smartest (50), best agentic, best SWE-bench |
| **code** | `glm-5:cloud` | 1277ms | Highest coding quality (77.8% SWE-bench) |

## Key Findings

- **GLM-5 and Kimi K2.5 are in a tier of their own** on Ollama Cloud. Everything else scores
  roughly half their Intelligence Index
- **Speed vs intelligence tradeoff is steep** — sub-600ms models (qwen3-next) score 27 on
  Intelligence Index vs 50 for GLM-5 at 1.3s. The ~700ms savings isn't worth halving intelligence
- **Thinking mode is expensive** — GLM-5 goes from 1.3s to 8.6s. Use thinking only when needed
- **Ollama Cloud does NOT support embeddings** — no `/api/embed` endpoint, no roadmap
- **Qwen3.5 and DeepSeek-V3.2** would be strong picks if they were faster on cloud infrastructure
