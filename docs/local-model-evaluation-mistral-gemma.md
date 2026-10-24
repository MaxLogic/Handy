# Handy evaluation: Mistral and Gemma 4, 2026-10-16

Keep Qwen 9B and S1-mini as the production defaults. Gemma 4 26B-A4B is the most useful new candidate for translation and friendly rewrites at near-9B latency, but it was weaker on technical rewrites and summaries. Mistral did well on professional developer rewrites, with slower responses and poor summary length control. Gemma 4 31B scored highest overall in this review but took about three times as long as 9B and used 22.2 GiB of whole-card VRAM with Whisper closed. S1 remains the fastest English-cleanup option. None is a clear all-purpose replacement on this small synthetic corpus.

| Model | Score /120 | Usable /30 | First median | First p95 | Card peak | Peak minus preload | Errors / truncations |
|---|---:|---:|---:|---:|---:|---:|---:|
| Qwen3.5 9B Q4_K_M | 95 | 20 | 644 ms | 2013 ms | 9.19 GiB | 5.38 GiB | 0 / 0 |
| S1-mini Q4_K_M | 52 | 7 | 204 ms | 1978 ms | 5.02 GiB | 1.12 GiB | 3 / 3 |
| Mistral Small 3.2 24B Q4_K_M | 98 | 23 | 1161 ms | 5853 ms | 18.15 GiB | 14.27 GiB | 0 / 0 |
| Gemma 4 31B QAT Q4_0 | 104 | 25 | 1967 ms | 6324 ms | 22.17 GiB | 18.25 GiB | 0 / 0 |
| Gemma 4 26B-A4B QAT Q4_0 | 99 | 22 | 589 ms | 1934 ms | 18.05 GiB | 14.26 GiB | 0 / 0 |

Thirty frozen synthetic post-Whisper text tasks, three repetitions each, one unrelated warmup. RTX 3090, CUDA llama.cpp b11560, 4096 context, one slot, thinking off, temperature 0, seed 42, maximum output 768 tokens. No prompt tuning or corpus changes. First request per case can reuse warmed prefixes; repeated requests reuse additional cached input. Times exclude loading, speech recognition and paste. p95 uses nearest rank over 30 cases. Card memory is a sampled inference-phase maximum, including desktop but excluding Whisper and other LLMs; loading peaks are not captured. No fixed VRAM reserve was imposed. These runs do not establish production 16k context fit with Whisper loaded. 9B and S1 timing receipts are earlier CUDA baselines, re-reviewed here by the same reviewer as the new models. The initial S1/Mistral/9B packet hid candidate identities. Gemma variant names were exposed by packet filenames, so their review was only partially blinded. Baseline outputs were re-scored for this comparison; subjective thresholds can differ from previous reports without any inference change. Quality scores are subjective; >=3/4 means at most minor edits. Small score differences are not evidence of a broad model ranking. S1 is intended for English cleanup; its aggregate score includes out-of-domain tasks. All three outputs per case were reviewed; materially different variants use the lowest corresponding quality score. New runs explicitly set speculation to none; the earlier baselines use the same default. Whisper remains unchanged on Vulkan. Production 9B and S1 assignments remain unchanged pending the user decision. Cases with textual variation across repeats: Qwen3.5 9B Q4_K_M: 0/30; S1-mini Q4_K_M: 2/30; Mistral Small 3.2 24B Q4_K_M: 4/30; Gemma 4 31B QAT Q4_0: 10/30; Gemma 4 26B-A4B QAT Q4_0: 9/30.

## Tasks

| Task | Qwen3.5 9B Q4_K_M | S1-mini Q4_K_M | Mistral Small 3.2 24B Q4_K_M | Gemma 4 31B QAT Q4_0 | Gemma 4 26B-A4B QAT Q4_0 |
|---|---:|---:|---:|---:|---:|
| cleanup_de | 1/2 | 1/2 | 2/2 | 2/2 | 1/2 |
| cleanup_en | 3/6 | 5/6 | 5/6 | 5/6 | 4/6 |
| cleanup_pl | 3/3 | 1/3 | 3/3 | 3/3 | 3/3 |
| rewrite_friendly | 2/3 | 0/3 | 1/3 | 2/3 | 3/3 |
| rewrite_senior | 1/3 | 0/3 | 3/3 | 2/3 | 1/3 |
| summary | 1/3 | 0/3 | 0/3 | 1/3 | 0/3 |
| translate_de_en | 3/3 | 0/3 | 3/3 | 3/3 | 3/3 |
| translate_pl_de | 2/3 | 0/3 | 2/3 | 3/3 | 3/3 |
| translate_pl_en | 4/4 | 0/4 | 4/4 | 4/4 | 4/4 |

## Evidence

Run root: `F:\dev-artifacts\runs\handy-local-llm-eval\20261016-mistral-gemma-i6r0laoj`. Raw request/response receipts, GPU samples, server manifests, pinned download hashes, and all semantic judgments are retained. See `scripts/handy-eval/README.md` for reuse.

Google Gemma 4 weights are the official QAT Q4_0 GGUFs: [31B](https://huggingface.co/google/gemma-4-31B-it-qat-q4_0-gguf/tree/59dde24573e7e61570dba08b18a2e1fe246955ed) and [26B-A4B](https://huggingface.co/google/gemma-4-26B-A4B-it-qat-q4_0-gguf/tree/d1c082be9cf3c8a514acf63b8761f4b41935842e). Vision projectors were not needed for these text tasks. Mistral is [Unsloth Q4_K_M at the pinned revision](https://huggingface.co/unsloth/Mistral-Small-3.2-24B-Instruct-2506-GGUF/tree/b750ec2299225e492f1bd27cab88a0a595fa848f). Downloaded GGUFs total 46,424,288,000 bytes, or 43.2 GiB, retained under `C:/models/llama.cpp`.
