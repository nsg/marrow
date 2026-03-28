# Ollama Cloud Model Benchmarks — March 2026

Tested against `https://ollama.com/api/chat` with a simple prompt ("What is 2+2? Answer in one word.") and 30s timeout.

## Speed + Capability Matrix

| Model | No Think | Think | Thinks? | General | Code | Tool Calling | Notes |
|---|---|---|---|---|---|---|---|
| **qwen3-next:80b-cloud** | 557ms | 2445ms | YES | 3-4 | 3-4 | High | Fastest smart model. Qwen tool-calling lineage |
| **nemotron-3-super:cloud** | 715ms | 1238ms | YES | 3? | 3? | Medium | Fast, NVIDIA — limited public data |
| **qwen3-coder-next:cloud** | 721ms | 967ms | NO | 3 | 4-5 | High | Best code model. Very fast |
| **ministral-3:3b-cloud** | 857ms | 1299ms | NO | 2 | 2 | Low | Too small for serious use |
| **nemotron-3-nano:30b-cloud** | 857ms | 1028ms | YES | 2-3? | 2-3? | Low-Med | Fast, limited data. 30B risky for strict JSON |
| **devstral-2:123b-cloud** | 973ms | 1013ms | NO | 3 | 4 | Med-High | Strong code, sub-1s. Mistral coding focus |
| **ministral-3:8b-cloud** | 1002ms | 931ms | NO | 2-3 | 2 | Low | Lightweight efficiency model |
| **devstral-small-2:24b-cloud** | 1022ms | 1126ms | NO | 2-3 | 3 | Medium | Decent code for size |
| **kimi-k2.5:cloud** | 1084ms | 1942ms | YES | 3-4 | 3-4 | High | Purpose-built for tool calling. Moonshot AI |
| **cogito-2.1:671b-cloud** | 1165ms | 2363ms | YES | 3-4 | 3-4 | Medium | Reasoning-focused 671B MoE. New entrant |
| **ministral-3:14b-cloud** | 1163ms | 1227ms | NO | 3 | 2-3 | Low-Med | Best ministral variant |
| **glm-5:cloud** | 1277ms | 8595ms | YES | 3-4 | 3 | Medium | Zhipu AI. Slow with thinking enabled |
| **rnj-1:8b-cloud** | 1393ms | 1010ms | NO | ? | ? | ? | Unknown model, unknown origin |
| **deepseek-v3.2:cloud** | 1778ms | 13828ms | YES | 4 | 4 | High | Frontier quality. Very slow thinking |
| **gemini-3-flash-preview:cloud** | 2406ms | 1956ms | NO | 4 | 3-4 | Very High | Best native tool calling API. Google |
| **minimax-m2.5:cloud** | 2754ms | 2460ms | YES | 3 | 3 | Medium | MiniMax. Mid-tier |
| **minimax-m2.7:cloud** | 5157ms | 6147ms | YES | 3-4 | 3 | Medium | MiniMax. Slow |
| **qwen3-vl:235b-cloud** | 5386ms | 5627ms | YES | 4 | 3-4 | Medium | Vision-language. Overkill for text only |
| **glm-4.7:cloud** | 23906ms | — | — | 3 | 3 | Medium | Very slow |
| **glm-4.6:cloud** | 17760ms | — | — | 3 | 3 | Medium | Very slow |
| **minimax-m2:cloud** | 561ms | — | — | ? | ? | ? | Returned ERROR |
| **qwen3.5:397b-cloud** | TIMEOUT | — | — | 4 | 4 | High | Too slow for cloud |

## Rating Scale

- **General/Code intelligence** (1-5): 1=basic, 2=capable, 3=strong, 4=frontier, 5=SOTA
- **Tool calling**: How reliably the model returns structured JSON and follows output formats
- **Thinks?**: Whether the model supports thinking/reasoning mode via `"think": true`

## Recommended Config for Marrow

| Role | Model | Rationale |
|---|---|---|
| **fast** | `qwen3-next:80b-cloud` (no think) | 557ms, smart (3-4), strong tool calling |
| **default** | `kimi-k2.5:cloud` | 1.1s, built for tool calling, thinking capable |
| **code** | `qwen3-coder-next:cloud` | 721ms, best code rating (4-5), Qwen tool-calling lineage |

## Tool Calling Research

Models ranked by tool calling / structured output reliability:

1. **Gemini 3 Flash Preview** — native JSON mode, best API support. But 2.4s is slow for fast role
2. **Kimi K2.5** — purpose-built for agentic tool calling. Near top of BFCL
3. **Qwen3-Next / Qwen3-Coder-Next** — proven Qwen tool-calling lineage, explicit `<tool_call>` tags
4. **DeepSeek V3.2** — battle-tested in agent frameworks. But slow
5. **Devstral 2** — code-focused helps with JSON adherence
6. **Cogito 2.1** — large but unproven on tool calling specifically
7. **GLM-5** — decent but less tested in Western frameworks
8. **Nemotron models** — limited public benchmark data

## Notes

- Ollama Cloud does NOT support embedding models (no `/api/embed` endpoint, no roadmap)
- GLM models are notably slow (14-24s), avoid for latency-sensitive roles
- `qwen3.5:397b-cloud` consistently times out — too large for cloud inference
- `minimax-m2:cloud` returned errors during testing
- Thinking mode adds significant latency: deepseek-v3.2 goes from 1.8s to 13.8s
- All tests run from this workspace on 2026-03-28
