# Larger local-model evaluation, 2026-10-16

Keep Qwen 9B for general tasks and S1-mini for English cleanup. The larger Qwens did not show a clear quality improvement on this corpus. The 35B MoE with MTP was faster, but the higher-precision IQ4 variant did not improve the reviewed outputs. The dense 27B was much slower. S1 English cleanup remains the fastest intended-domain option (164 ms median).

Frozen 30-case Handy corpus, three consecutive repetitions per case, 4,096 context, CUDA llama.cpp b11560. Thinking disabled. Production LLMs stopped and Handy closed during larger-model tests. Earlier 9B/S1 CUDA measurements are retained baselines.

| Model | Usable cases | Score /120 | First median | First p95 | Card peak | Peak minus preload | Errors / truncations |
|---|---:|---:|---:|---:|---:|---:|---:|
| Qwen 9B Q4_K_M | 24/30 | 97 | 644 ms | 2013 ms | 9.19 GiB | 5.38 GiB | 0 / 0 |
| S1-mini Q4_K_M | 7/30 | 49 | 204 ms | 1978 ms | 5.02 GiB | 1.12 GiB | 3 / 3 |
| Qwen 35B IQ3_XXS | 25/30 | 99 | 602 ms | 2244 ms | 16.79 GiB | 13.02 GiB | 0 / 0 |
| Qwen 35B IQ3_XXS + MTP | 25/30 | 102 | 476 ms | 1724 ms | 17.31 GiB | 13.55 GiB | 0 / 0 |
| Qwen 27B IQ4_XS | 26/30 | 102 | 1466 ms | 5178 ms | 17.91 GiB | 14.12 GiB | 0 / 0 |
| Qwen 35B IQ4_XS + MTP | 23/30 | 98 | 458 ms | 1569 ms | 21.04 GiB | 17.28 GiB | 0 / 0 |

Usable means a subjective rubric score of at least 3/4. Scores are not general model benchmarks. The saved 9B/S1 outputs were re-reviewed alongside the non-MTP 35B; this fresh review changes some usability thresholds from the earlier report. The same reviewer assessed 9B/S1/35B-base/27B/35B-IQ4. A separate reviewer assessed 35B-IQ3 MTP; small score differences need manual inspection. S1 is specialized for English cleanup; its whole-suite score includes tasks outside that domain.

Latency covers an entire postprocessing request, excluding model loading and audio transcription. The first request per case can reuse warmed prompt prefixes; later repetitions reuse additional cached input. p95 is nearest rank over 30 cases. Sampled inference-phase whole-card VRAM excludes loading transients and includes desktop applications; the delta is an approximation, not an exclusive allocation. No fixed VRAM reserve was imposed. Whisper was not loaded. These 4k runs do not establish fit at the production 16k Qwen context.

All four larger-model configurations completed 90 requests each without errors or truncation. Their outputs were identical across the three repetitions within each configuration. MTP used the same GGUF and drafted/accepted tokens recorded in response timing counters. MTP can change wording even with temperature zero, so its outputs were reviewed separately.

## Usable cases by task

| Task | Qwen 9B Q4_K_M | S1-mini Q4_K_M | Qwen 35B IQ3_XXS | Qwen 35B IQ3_XXS + MTP | Qwen 27B IQ4_XS | Qwen 35B IQ4_XS + MTP |
|---|---:|---:|---:|---:|---:|---:|
| cleanup_de | 1/2 | 1/2 | 1/2 | 1/2 | 2/2 | 2/2 |
| cleanup_en | 5/6 | 5/6 | 5/6 | 5/6 | 5/6 | 5/6 |
| cleanup_pl | 3/3 | 1/3 | 3/3 | 3/3 | 3/3 | 3/3 |
| rewrite_friendly | 3/3 | 0/3 | 3/3 | 3/3 | 3/3 | 3/3 |
| rewrite_senior | 2/3 | 0/3 | 2/3 | 2/3 | 2/3 | 0/3 |
| summary | 1/3 | 0/3 | 1/3 | 1/3 | 1/3 | 0/3 |
| translate_de_en | 3/3 | 0/3 | 3/3 | 3/3 | 3/3 | 3/3 |
| translate_pl_de | 2/3 | 0/3 | 3/3 | 3/3 | 3/3 | 3/3 |
| translate_pl_en | 4/4 | 0/4 | 4/4 | 4/4 | 4/4 | 4/4 |

## Evidence

Run root: `F:\dev-artifacts\runs\handy-local-llm-eval\20261016-large-5qam7597`. Retain pinned download hashes, raw responses, GPU samples, manifests, and semantic judgments. Harness: `scripts/handy-eval/README.md`. No corpus or production prompt tuning was performed.

The blind review packet was regenerated with explicit UTF-8 decoding after a Windows-default decoding mistake was identified. Raw inference inputs and receipts were unchanged. Baseline/35B non-MTP candidates were reviewed together without the model mapping; the IQ3 MTP reviewer had prior implementation context, so that review was not fully blind.

The old WinGet Vulkan llama.cpp package was uninstalled with WinGet, reclaiming 91,231,517 bytes (87.0 MiB). The separately installed CUDA servers are healthy. Handy remains closed. Three experimental GGUF downloads are retained in C:/models/llama.cpp, totaling 47,255,788,000 bytes (44.0 GiB); each matched pinned Hugging Face LFS SHA-256 metadata.

## Whisper and next candidate

Handy Whisper remains on its bundled Vulkan engine. CUDA support exists in pinned transcribe-cpp 0.3.1, but the Windows build currently enables dynamic-backends and Vulkan in src-tauri/Cargo.toml. A CUDA comparison requires enabling/building the native CUDA backend and packaging its runtime/cuBLAS dependencies. No Whisper CUDA speedup was measured; the LLM benchmark cannot establish one.

A useful future non-Qwen comparison is [Mistral Small 3.2 24B Instruct](https://huggingface.co/mistralai/Mistral-Small-3.2-24B-Instruct-2506), whose language metadata includes Polish and German. The inspected [Unsloth GGUF revision](https://huggingface.co/unsloth/Mistral-Small-3.2-24B-Instruct-2506-GGUF/tree/b750ec2299225e492f1bd27cab88a0a595fa848f) lists Q4_K_M at 14,333,922,848 bytes. This candidate was researched, not downloaded or tested.
