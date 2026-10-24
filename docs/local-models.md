# Local models and shortcuts

Configuration checked on 2026-10-16 against the installed Handy MaxLogic settings,
local server launch scripts and Windows scheduled task. This describes Pawel's
local setup; changing settings can change these assignments.

## Models and purposes

| Model | Purpose | API base URL | API model name |
| --- | --- | --- | --- |
| Qwen3.5-9B Q4_K_M | Selected Text Assistant, Polish-to-English dictation, and default cleanup when the English cleanup route does not apply | `http://127.0.0.1:8080/v1` | `qwen3.5:9b` |
| S1-mini Q4_K_M (Superwhisper) | English transcript cleanup for profiles that enable English cleanup, including the built-in cleanup profile | `http://127.0.0.1:8082/v1` | `S1-mini` |
| Qwen3.5-4B Q4_K_M | Optional smaller alternative through a profile model override; no current profile selects it | `http://127.0.0.1:8081/v1` | `qwen3.5:4b` |

S1-mini normalizes English transcripts using styling, structure and context
controls. The built-in cleanup profile uses semi-formal prose in a general
context. S1-mini runs with thinking disabled and temperature 0. It is not the
selected model for translation or spoken assistant instructions. English cleanup
routing requires an eligible English transcript, a dictation profile that opts
in, and no profile model override. Otherwise the configured default or profile
model handles cleanup.

The assistant inherits Qwen 9B from the Custom provider. The Polish-to-English
profile explicitly selects `qwen3.5:9b`, sets speech recognition to Polish (`pl`),
and uses its translation prompt. Its English cleanup route is disabled.

Speech recognition runs before LLM processing. The selected speech model is
Whisper Large v3, file `whisper-large-v3-Q5_K_M.gguf`, from
`handy-computer/whisper-large-v3-gguf`. It uses Vulkan GPU acceleration on the
RTX 3090 and remains loaded (`Never` unload timeout). General dictation uses
automatic language detection; the translation profile overrides it to Polish.
Raw transcription uses this speech model without an LLM cleanup request.

## Shortcuts

| Shortcut | Action when idle |
| --- | --- |
| Ctrl+Space | Dictation with AI post-processing |
| Ctrl+Shift+Space | Raw dictation |
| Ctrl+Alt+Space | Selected Text Assistant: apply a spoken instruction to selected text |
| Ctrl+Win+F9 | Polish-to-English dictation |
| Escape | Cancel the current operation |

Ctrl+Space and Ctrl+Shift+Space also finish any active recording, including an
assistant or profile recording. They preserve the mode that started it.

For the assistant, select the text, press Ctrl+Alt+Space, speak the editing
instruction, then finish recording with Ctrl+Space or Ctrl+Shift+Space.
Ctrl+Win+F9 translates new Polish dictation; use the assistant shortcut to edit
a selection. The user confirmed the assistant worked with the correct shortcut.

## Model files and server controls

GGUF files are under `C:\models\llama.cpp`:

- `Qwen3.5-9B-Q4_K_M.gguf`
- `Qwen3.5-4B-Q4_K_M.gguf`
- `s1-mini-q4_k_m.gguf`

Launchers and logs are under `C:\models\commands`. Run the matching
`start-qwen3.5-9b.cmd`, `start-qwen3.5-4b.cmd` or `start-s1-mini.cmd`.
Use the corresponding `stop-*.cmd`, or `stop-all.cmd` for these model servers.
PowerShell counterparts are available. See `C:\models\commands\README.txt`
for operational details and S1-mini's input format.

The scheduled task `Handy MaxLogic - start dictation models` is enabled and
launches `start-dictation-models.ps1` 20 seconds after Windows login. It starts
Qwen 9B and S1-mini. Qwen 4B is started manually. Servers run hidden and stay
loaded. Each has one request slot; Qwen context is 16384 tokens and S1-mini
context is 4096 tokens. Startup configuration does not prove current health.
Check `/health` on the relevant port before diagnosing a failed request.

Settings source:
`F:\projects\MaxLogic\Handy MaxLogic-portable-instance\Data\settings_store.json`.
Project copy of this note: `F:\projects\MaxLogic\Handy\docs\local-models.md`.
Model-folder copy: `C:\models\README.md`. Update both when assignments change.

## LLM inference backend

The local LLM launchers use llama.cpp b11560 (commit `b13a93a3d`) with CUDA 12.4
on the RTX 3090. Executable:
`C:\models\llama.cpp-b11560-cuda12.4\llama-server.exe`.
The previous WinGet Vulkan package was uninstalled on 2026-10-16. Whisper
inside Handy continues to use its separately bundled Vulkan backend.

A reusable, synthetic text-only comparison harness lives at
`F:\projects\MaxLogic\Handy\scripts\handy-eval`. See
`F:\projects\MaxLogic\Handy\docs\local-model-evaluation.md` for the
2026-10-16 results and reproduction instructions. Model roles and production
context sizes above are unchanged.

Larger-model follow-up results are in
`F:\projects\MaxLogic\Handy\docs\local-model-evaluation-large.md`.
Experimental GGUFs are retained under `C:\models\llama.cpp`; they are not
started at login. Select them explicitly through the evaluation harness.

Mistral Small 3.2 and Google Gemma4 (31B and 26B-A4B) follow-up results are in
`F:\projects\MaxLogic\Handy\docs\local-model-evaluation-mistral-gemma.md`.
These GGUFs are also retained for explicit harness selection; production
assignments and Whisper's Vulkan backend remain unchanged.
