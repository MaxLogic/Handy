# Local model comparison, 2026-10-16

Use CUDA for the local LLM servers. Keep Qwen 9B for general assistant/translation tasks and S1-mini for English cleanup while testing a larger replacement. Qwen 4B is a useful faster, smaller alternative, particularly for translation; it did not win overall quality.

This is a synthetic, text-only Handy simulation after speech recognition. It does not measure Whisper accuracy, recording, UI responsiveness, clipboard/paste delivery, or live application acceptance.

## Protocol

30 frozen cases: six English, three Polish and two exploratory German cleanups; four Polish-to-English, three German-to-English and three Polish-to-German translations; three friendly rewrites, three senior-developer rewrites, and three summaries. Summary source bodies contain 720, 455 and 153 words. All inputs are synthetic.

Each model ran all cases three times, plus a distinct excluded warmup: 270 measured requests, one model loaded at a time, Handy closed. Same llama.cpp b11560 / b13a93a3d; CUDA 12.4; RTX 3090; 4,096-token context, one request slot, full GPU offload, flash attention on, temperature 0, seed 42, thinking off, MTP off, output limit 768 tokens, request deadline 60 seconds.

The source built-in cleanup and assistant prompts exactly matched installed Handy settings. The local Custom provider has supports_structured_output=false, so Qwen cleanup used plain text with production framing. S1 English cleanup used its trained control-line format. S1 non-English cleanup and all its translation/assistant tasks are explicit out-of-domain probes; Handy normally skips non-English S1 cleanup. Seed and chat_template_kwargs are evaluation-only controls; production reasoning_effort=none was retained.

First-request statistics use one first measured request per case after neutral warmup. Repeated inputs can reuse prompt cache and are reported separately. These are HTTP request/body wall times, excluding model loading, with server token timings retained. Small-sample p95 values are descriptive, not latency guarantees.

## Latency and memory

| Model | First-request median | First-request p95 | Repeated-input median | Request failures | Approx. extra GPU usage |
| --- | ---: | ---: | ---: | ---: | ---: |
| Qwen 3.5 9B Q4_K_M | 0.644 s | 2.013 s | 0.599 s | 0/90 | 5.38 GiB |
| Qwen 3.5 4B Q4_K_M | 0.531 s | 1.671 s | 0.441 s | 0/90 | 3.09 GiB |
| S1-mini Q4_K_M | 0.204 s | 1.978 s | 0.189 s | 3/90 | 1.12 GiB |

GPU usage is the sampled whole-card peak minus that run's pre-load whole-card baseline. It includes desktop fluctuations and is not an exclusive model allocation. Sampling every 0.5 seconds plus query overhead can miss brief peaks. Production Qwen context remains 16,384 tokens, so these common-4,096-context figures do not replace the earlier production memory snapshot.

S1-mini's low whole-suite latency is not successful general-task performance: it usually copied the source rather than translating or following rewrite instructions. Its English-cleanup first-request median was 0.164 s. All three S1 attempts at summary-01 reached the 768-token limit and were retained as failures. Both Qwens completed all 90 requests without errors/truncation.

The same-revision Qwen 9B backend pilot used four tasks, three repetitions each. First-request median was 1.370 s with Vulkan versus 1.074 s with CUDA. This is about 22% lower, with only four first-request samples per backend. Some outputs differed in length. On the two first-request tasks with identical output/token counts, cleanup was about 15% faster and translation about 21% faster. CUDA is supported by this bounded result; no universal backend speedup is claimed.

## Blind semantic review

An independent reviewer saw anonymized A/B/C outputs, source texts and case rubrics before model identities were revealed. It graded all 90 first outputs, then checked repeat differences. Scale: 0 unusable/error; 1 major task failure; 2 needs meaningful manual correction; 3 usable with minor edits; 4 directly usable. The following counts mean grade 3 or 4, not automated correctness or guaranteed safety.

| Task | Qwen 9B | Qwen 4B | S1-mini |
| --- | ---: | ---: | ---: |
| English cleanup | 3/6 | 3/6 | 5/6 |
| Polish cleanup | 2/3 | 0/3 | 0/3 |
| German cleanup (exploratory) | 0/2 | 0/2 | 0/2 |
| Polish → English | 4/4 | 4/4 | 0/4 |
| German → English | 3/3 | 3/3 | 0/3 |
| Polish → German | 2/3 | 3/3 | 0/3 |
| Friendly rewriting | 3/3 | 2/3 | 0/3 |
| Senior developer rewriting | 1/3 | 2/3 | 1/3 |
| Summaries | 2/3 | 1/3 | 0/3 |

Across all 30 cases: Qwen 9B 20/30, Qwen 4B 18/30, S1-mini 6/30. This deliberately includes S1's out-of-domain tasks. Do not use that aggregate to assess its English-cleanup role.

Concrete failures:

- Qwen 9B: `Result<(), StoreError>` became `Result&lt;(), StoreError&gt;`; a Polish-to-German translation changed recording cancellation into ending the recording. One summary exceeded the requested word limit and omitted the explicit Owner/Parent distinction.
- Qwen 4B: Polish `zimnego startu` (cold start) became `zimowego startu` (winter start). One summary exceeded its 130-word limit and strengthened an unconfirmed lifetime explanation.
- S1-mini: a lesson about literal punctuation words lost the literal word `comma` and changed `I said period, not full stop` into punctuation. Translations usually remained in the original language. Long-summary output either copied the source or truncated.

Both Qwens returned identical text across all three repetitions of every case. S1 differed textually on two out-of-domain cases, without a material quality-tier change. Sparse automated identifier/length checks are retained only as factual tripwires: all three models happened to pass 78/90, demonstrating why that count is not a quality score.

## Recommendation

1. Keep CUDA as the LLM backend. Both existing model servers are restored on CUDA at their original ports and context sizes; Qwen 4B is stopped. Handy remains closed. Whisper's Vulkan backend is unchanged. The previous WinGet Vulkan package and original launcher/doc files remain available for rollback.
2. Keep Qwen 9B as the general model for now. Its advantage over 4B was clearest in Polish cleanup, friendly tone and summaries. For translation alone, 4B is a credible faster option and preserved one trigger that 9B mistranslated.
3. Keep S1-mini for English cleanup until a larger model actually beats it on those cases. It was faster and more effective in that role here, despite one serious literal-punctuation error. It is not a general-model replacement.
4. Run the proposed larger Qwen through the unchanged corpus using the same harness, first without MTP, then with MTP. Consolidation into one resident LLM remains a reasonable aim. This comparison does not establish 35B quality, speed or VRAM fit.

## Reproduce and extend

```powershell
python -B -m unittest discover -s scripts/handy-eval -p test_runner.py -v
python -B scripts/handy-eval/benchmark_local.py --backends cuda --models qwen9b qwen4b s1mini --output F:/dev-artifacts/runs/handy-local-llm-eval/NEW-RUN --repeats 3 --timeout 60
```

Stop competing servers before the owned-process launcher runs. It refuses an occupied port and does not manage other applications. Add a model entry to `scripts/handy-eval/models.local.json` with its GGUF path, alias, adapter and context, then select its ID. See [harness README](../scripts/handy-eval/README.md). An endpoint-only runner is also available for an already-running OpenAI-compatible local server.

## Evidence and verification

Retained run root: `F:\dev-artifacts\runs\handy-local-llm-eval\20261016-q4kqzbh7`. Authoritative runs: `backend-comparison-r3` and `full-cuda-r2`. `comparison.json` aggregates measurements; `quality-review.json` contains 90 judgments and reasons; `blinded-review.json` contains inputs/outputs as reviewed. Full raw HTTP bodies, request/settings/prompt snapshots, corpus copies, model/executable/source hashes, server logs and GPU samples remain in the run folders.

Six offline harness tests passed. Independent corrected-source review passed. Updated model launcher passed PSScriptAnalyzer with zero warnings/errors. Both restored server health checks passed, and repeated 9B start did not create another process. No commits or unrelated source changes were made.

Superseded attempts are explicitly retained: initial validation mismatch caused no model requests; the subsequent JSON-constrained cleanup pilot and interrupted full run used a protocol different from installed Handy and are excluded from all reported measurements. The corrected runs above replace them.

Automatic approval review blocked removal of the two downloaded CUDA ZIP archives with only "blocked by policy" returned. Approximately 626 MiB remains in `C:\Users\pawel\AppData\Local\Temp\handy-cuda-install-4a494bnp`. No cleanup bypass was attempted.

Limitations: one GPU, small synthetic corpus, modest-length summaries, no audio errors beyond constructed messy transcripts, fixed order and limited repeats, subjective single-reviewer grades, and no held-out model/prompt tuning validation. Use these as a local decision baseline, not universal model rankings.
