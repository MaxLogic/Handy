# Local Handy model evaluation

For an automated local comparison, `benchmark_local.py` launches one owned
server at a time, samples whole-GPU memory/utilization, calls the runner, and
stops that exact child. It refuses an occupied port and never stops an existing
server. Close Handy and stop the existing dictation servers first if they would
compete for the GPU. The launcher does not close or restore those applications.

```powershell
python -B scripts/handy-eval/benchmark_local.py `
  --config scripts/handy-eval/models.local.json `
  --backends cuda --models qwen9b qwen4b s1mini `
  --output F:/dev-artifacts/runs/handy-local-llm-eval/NEW-RUN `
  --repeats 3 --timeout 60
```

`models.local.json` records Pawel's installed backend executables, model files,
aliases, and a common 4,096-token context. To test another model, add an entry
with a unique `id`, its GGUF `server.model_path`, API `model` alias, and adapter;
then pass its ID to `--models`. The `qwen` adapter represents Handy's general
prompt route and can be used for another compatible instruction model. The
`s1` adapter uses S1-mini's trained English normalization format. To compare
backends, add their executable paths to the config and select their keys with
`--backends`. Only CUDA is currently installed/configured. Each selected model
runs on each backend. Startup, model hashing, GPU sampling, and server shutdown sit outside
the runner's HTTP request timer. First model load time is recorded separately.

The GPU CSV samples are whole-card measurements including the desktop, not
exclusive per-model allocations. A 0.5-second sampling interval can miss brief
peaks. Server logs, exact launch commands, executable/model hashes, GPU samples,
and runner receipts remain under the unique output directory.

`runner.py` sends serial, non-streaming chat-completion requests to an already
running local server. Python 3.13 and its standard library are sufficient. The
runner never starts/stops Handy or a model server, downloads models, or samples
the GPU. Run each model/backend configuration separately with external GPU
telemetry when comparing performance.

```powershell
py -3.13 -B scripts/handy-eval/runner.py `
  --config scripts/handy-eval/models.local.json `
  --corpus scripts/handy-eval/cases.json `
  --model qwen9b `
  --output F:/dev-artifacts/runs/Handy-local-eval/example-unique-run `
  --repeats 3 --warmup 1 --timeout 120 --max-tokens 768
```

The output directory **must not exist**. Use a unique run path under the
project's evidence/run location. Files use exclusive creation; runs never
replace earlier results. Treat these files as retained evaluation evidence.

Repeat `--case ID` or `--category CATEGORY` to select cases. When both filters
are present their intersection is used. Unknown filters or an empty selection
fail before sending requests. Repeat `--model ID` for serial model execution in
configuration order, though one model per run makes externally collected GPU
telemetry easier to attribute. Repeats follow corpus case order. Warmup defaults
to one neutral synthetic request per model with ID `__warmup__` and input
`The benchmark is ready period`, using the first selected case's request kind.
It is snapshotted in the manifest and saved as an excluded warmup row. It never
uses a measured case's exact input, but can warm the shared system prefix and
model. It does not guarantee a fully warm model or cache.

Configuration shape:

```json
{"models":[{"id":"qwen9b","endpoint":"http://127.0.0.1:18080/v1","model":"qwen3.5:9b","adapter":"qwen","metadata":{"backend":"cuda"}}]}
```

Only credential-free loopback HTTP endpoints are accepted. Proxies and
redirects are disabled. Keep secrets out of this evaluation configuration:
the exact configuration is retained. `metadata` is descriptive; the caller
must verify it against the server's actual build, model hash and launch log.
Supported adapters are `qwen` and `s1`; the model string is sent unchanged.
`supports_structured_output` defaults to `false`, matching Handy's Custom
provider default. Set it explicitly to `true` only when the evaluated production
provider actually uses structured output. It controls both cleanup and
assistant request/response formats; specialized S1 English cleanup always
uses its fixed plain-text format.

Corpus shape:

```json
{"version":1,"cases":[{"id":"example","category":"cleanup_en","kind":"cleanup","language":"en","input":"um hello period","instruction":"Clean the transcript without changing its language.","checks":{"required_all":["hello"],"required_any":[],"forbidden":["um"]},"rubric":["Preserve meaning."],"reference":"Hello."}]}
```

`kind` is `cleanup` or `assistant`; `language` is `en`, `pl` or `de`.
`required_all` requires every phrase, `required_any` requires at least one phrase
from each supplied group, and `forbidden` rejects listed phrases. Matching uses
Unicode NFKC normalization, case folding, collapsed whitespace and word
boundaries. `max_words` optionally limits Unicode word tokens; contractions
and hyphenated words count as one. Numeric spelling and punctuation are not
silently rewritten by the scorer.

These checks are lexical guardrails. They can miss valid paraphrases and pass
semantically wrong text. No reference-equality score or automatic semantic pass
is claimed. Inspect the full outputs against `rubric` and `reference`, especially
for omissions, invented facts, tone, technical identifiers and translation
quality. Use `required_any` for accepted alternative formulations; do not
interpret a lexical miss as an established model defect.

Requests mirror the current built-in Handy prompt framing:

- Qwen cleanup extracts the exact `CLEANUP_PROMPT` from `settings.rs` and
  appends the production `CLEANUP_FRAME`, with escaped `<transcript>` user
  framing. By default its response is plain cleaned text, with no JSON schema
  or JSON-output instruction, matching the Custom provider. When
  `supports_structured_output` is explicitly true it additionally requests the
  named strict `transcription_output` JSON schema and matching JSON instruction.
  Structured parsing rejects extra fields, duplicate fields, invalid types
  and Markdown fences. Both modes reject empty output and reasoning leakage
  and remove the same invisible characters as production cleanup.
- Assistant requests use the current built-in assistant system prompt and
  `ASSISTANT_FRAME`, with JSON user fields `instruction` and `selected_text`.
  Commands inside selected text remain untrusted source material. Output is
  plain replacement text by default, matching Handy's Custom assistant route.
  An explicitly structured provider requests the strict `assistant_output`
  schema with only the string field `replacement` and corresponding system
  instruction, as production does.
- S1 uses its fixed normalizer system prompt and default
  `[Styling: semi-formal] [Structure: prose] [Context: general]` line **only for
  English cleanup**. Every other S1 case uses the generic assistant route and
  is marked `out_of_domain`. Handy itself skips non-English S1 cleanup;
  those exploratory results do not imply production support.

Cleanup cases may leave `instruction` empty. Non-English exploratory S1 cleanup
then uses a fixed cleanup instruction recorded in the manifest and exact
request. Assistant instructions must be nonempty; an empty assistant `input`
means generate text without a selection, matching Handy's insertion behavior.

The runner freezes prompt literals once before the run and records source
hashes plus exact request bodies. It fails if it cannot recognize current Rust
prompt literals, rather than silently substituting older prompts. It models
the built-in profiles with no custom vocabulary or edited user prompts, and
does not exercise Handy's full hotkey, STT, selection or insertion pipeline.

All requests use temperature 0, seed 42, `reasoning_effort: "none"`, and
`chat_template_kwargs: {"enable_thinking": false}`. The current Handy Custom
provider sends `reasoning_effort: "none"` initially because its target disables
reasoning. Seed and `chat_template_kwargs` are extra evaluation controls that
Handy does not send; these differences are recorded in the manifest. Request
temperature and output limit are fixed by this harness rather than copied from
persisted Handy settings. Unsupported options fail visibly; there is no
compatibility retry or hidden retry (Handy can retry some requests without
reasoning fields). A fixed seed does not guarantee bitwise deterministic GPU
inference across backends.

Each run retains:

- `manifest.json`: version/time/platform, settings, selected model metadata,
  SHA-256 of config, corpus, runner and relevant Rust sources, prompt snapshot,
  selected case IDs, planned measured row count and retention purpose.
- `config.json`, `corpus.json`: exact evaluated inputs.
- `rows.jsonl`: flushed after every attempt, including warmups and failures;
  full case/request, raw response body, parsed response, normalized output,
  usage, server `timings`, wall latency, finish reason, truncation, error and
  lexical checks. Provider-specific cache/usage fields remain in the full
  response. No cache state is assumed or reset.
- `summary.json`: warmup and measured counts, totals per model/category and
  model/category pair, lexical passes, out-of-domain counts, error/truncation
  counts, and min/median/p95/max latency for all attempts and successful
  attempts separately. Error latency is retained, avoiding silent removal of
  failed requests from the measured distribution. `latency_first_per_case`
  separates repeat 1 of each case from `latency_repeated` for repeats 2+;
  these distributions also include failures. First-per-case reflects distinct
  measured inputs after neutral warmup, while later repeats expose exact-input
  cache reuse; neither is a guaranteed cold-cache measurement.

Latency uses `perf_counter` from HTTP send through complete response-body read;
request construction, JSON/output parsing and file writes are outside that
timer. Server timings are recorded separately without assuming their units.
This does not measure time to first token. p95 uses nearest rank and is a
descriptive statistic at small sample counts, **not a robust tail-latency SLA**.
Prompt caching may make repeated runs faster; compare the saved usage/cache
data and launch conditions before attributing a speed difference to hardware.

Exit status is 0 for completed transport/parse runs, even if quality checks
miss; 1 if any request fails, times out, is filtered or is truncated; 2 for
invalid setup/files; 130 for interruption. Requests are not retried. A clean
keyboard interruption writes an incomplete summary and keeps attempted rows;
forced termination can leave only the flushed rows and initial manifest.
To summarize those rows without contacting a server:

```python
import json
import sys
sys.path.insert(0, "scripts/handy-eval")
import runner
from pathlib import Path
rows = [json.loads(line) for line in Path("RUN/rows.jsonl").read_text(encoding="utf-8").splitlines()]
print(json.dumps(runner.summarize(rows, completed=False), indent=2))
```

Focused offline verification:

```powershell
py -3.13 -B -m unittest discover -s scripts/handy-eval -p 'test_*.py' -v
```

Tests use disposable OS temporary directories and loopback fake HTTP servers;
they need no model, GPU, application lifecycle changes or third-party packages.

## Larger-model experiments

The local config also includes `qwen35b-base`, `qwen35b-mtp`,
`qwen35b-iq4-mtp`, and `qwen27b`.
These are experiments, not changes to Handy's production model assignments.
Download the exact GGUFs named in `server.model_path` before running them.
Use `--models` to choose a subset; experiments run serially on port 18080.

`server.spec_type` accepts `none` or `draft-mtp`. An omitted value preserves
llama.cpp's default. With `draft-mtp`, optional `server.spec_draft_n_max` must
be an integer from 1 through 16. The 35B MTP experiment uses 2. Compare the
same model with speculation disabled and enabled, including consecutive
requests. Model file size does not establish runtime VRAM usage.

The follow-up alternatives are `mistral24b` (Mistral Small 3.2 24B Q4_K_M),
`gemma31b`, and `gemma26b` (Google's official Gemma4 QAT Q4_0 GGUFs).
They use the same general Handy prompt route. Gemma thinking is disabled
through `--reasoning off` plus the runner's chat-template setting. Text-only
evaluation does not need the vision projectors. These entries also remain
experiments and do not change Handy's configured providers or autostart.

