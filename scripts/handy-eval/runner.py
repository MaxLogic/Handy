"""Serial, dependency-free evaluation of local OpenAI-compatible Handy models."""
import argparse
import hashlib
import json
import math
from pathlib import Path
import platform
import re
import statistics
import sys
import time
import unicodedata
from datetime import datetime, timezone
from urllib.error import HTTPError
from urllib.parse import urlsplit
from urllib.request import Request, build_opener, ProxyHandler, HTTPRedirectHandler

VERSION = 1
REPO = Path(__file__).resolve().parents[2]
EXPLORATORY_CLEANUP_INSTRUCTION = (
    "Clean up this dictated transcript in its original language. Remove fillers and accidental "
    "repetitions, apply explicit corrections and dictated punctuation, preserve meaning and "
    "technical identifiers. Return only cleaned text."
)


def utc_now():
    return datetime.now(timezone.utc).isoformat()


def sha256(data):
    return hashlib.sha256(data).hexdigest()


def strict_json(text):
    def unique(pairs):
        result = {}
        for key, value in pairs:
            if key in result:
                raise ValueError(f"Duplicate JSON key: {key}")
            result[key] = value
        return result
    def invalid(value):
        raise ValueError(f"Invalid JSON constant: {value}")
    return json.loads(text, object_pairs_hook=unique, parse_constant=invalid)


def load_prompts():
    """Read exact current Rust literals, failing closed if their shape changes."""
    paths = ["src-tauri/src/settings.rs", "src-tauri/src/prompts.rs",
             "src-tauri/src/llm_client.rs", "src-tauri/src/settings/profiles.rs"]
    data = {name: (REPO / name).read_bytes() for name in paths}
    sources = {name: sha256(content) for name, content in data.items()}
    settings = data[paths[0]].decode("utf-8-sig")
    framing = data[paths[1]].decode("utf-8-sig")
    literal = r'("(?:[^"\\]|\\.)*")'

    def constant(source, name):
        match = re.search(r'const ' + re.escape(name) + r': &str = ' + literal + ';', source)
        if not match:
            raise ValueError(f"Cannot extract Rust prompt {name}; update runner explicitly")
        return strict_json(match.group(1))

    assistant = re.search(r'name: "Selected Text Assistant"\s*\.into\(\),\s*prompt: ' + literal, settings)
    if not assistant:
        raise ValueError("Cannot extract built-in assistant prompt")
    controls = data[paths[3]].decode("utf-8-sig")
    defaults = {}
    for key in ("styling", "structure", "context"):
        match = re.search(key + r': ' + literal + r'\.into\(\)', controls)
        if not match:
            raise ValueError(f"Cannot extract S1 default {key}")
        defaults[key] = strict_json(match.group(1))
    return {"cleanup": constant(settings, "CLEANUP_PROMPT"),
            "assistant": strict_json(assistant.group(1)),
            "cleanup_frame": constant(framing, "CLEANUP_FRAME"),
            "assistant_frame": constant(framing, "ASSISTANT_FRAME"),
            "s1_system": constant(framing, "S1_SYSTEM"), "s1_controls": defaults}, sources


def build_request(model, case, prompts, max_tokens):
    cleanup = case["kind"] == "cleanup"
    s1 = model["adapter"] == "s1"
    specialized = s1 and cleanup and case["language"] == "en"
    structured = model.get("supports_structured_output", False) and not specialized
    field = ("transcription" if cleanup and not s1 else "replacement") if structured else None
    if specialized:
        system = prompts["s1_system"]
        controls = prompts["s1_controls"]
        user = (f"[Styling: {controls['styling']}] [Structure: {controls['structure']}] "
                f"[Context: {controls['context']}]\n{case['input']}")
    elif cleanup and not s1:
        system = (prompts["cleanup"].replace("${output}", "").strip() + "\n\n" +
                  prompts["cleanup_frame"])
        if structured:
            system += "\nReturn a JSON object whose only field `transcription` holds the cleaned text."
        escaped = case["input"].replace("&", "&amp;").replace("<", "&lt;").replace(">", "&gt;")
        user = f"<transcript>\n{escaped}\n</transcript>"
    else:
        # Non-English S1 cleanup deliberately uses the generic assistant route.
        # Handy skips that cleanup in production, so this is an exploratory row.
        line = ("Return a JSON object whose only field `replacement` holds the replacement text."
                if structured else "Return only the replacement text.")
        system = (prompts["assistant"].replace("${output}", "").strip() + "\n\n" +
                  prompts["assistant_frame"] + "\n" + line)
        instruction = case["instruction"] or (EXPLORATORY_CLEANUP_INSTRUCTION if cleanup else "")
        user = json.dumps({"instruction": instruction, "selected_text": case["input"]},
                          ensure_ascii=False, separators=(",", ":"))
    request = {"model": model["model"], "messages": [
        {"role": "system", "content": system}, {"role": "user", "content": user}],
        "stream": False, "temperature": 0, "seed": 42, "max_tokens": max_tokens,
        "chat_template_kwargs": {"enable_thinking": False}, "reasoning_effort": "none"}
    if field:
        request["response_format"] = {"type": "json_schema", "json_schema": {
            "name": "transcription_output" if field == "transcription" else "assistant_output", "strict": True,
            "schema": {"type": "object", "properties": {field: {"type": "string"}},
                       "required": [field], "additionalProperties": False}}}
    mode = {"field": field, "cleanup": cleanup,
            "route": ("s1_normalizer" if specialized else
                      ("cleanup_json" if structured else "cleanup_plain") if cleanup and not s1 else
                      "assistant_json" if structured else "assistant_plain"),
            "out_of_domain": s1 and not specialized,
            "domain_note": ("S1 specialized training is English cleanup only; Handy skips non-English S1 cleanup."
                            if s1 and not specialized else None)}
    return request, mode


def clean_output(content, mode):
    if not isinstance(content, str) or not content.strip():
        raise ValueError("Empty or non-text message content")
    if content.lstrip().startswith("<think>"):
        closing = content.find("</think>")
        if closing < 0:
            raise ValueError("Unclosed reasoning block")
        content = content[closing + len("</think>"):].lstrip()
    if mode["field"]:
        obj = strict_json(content)
        if not isinstance(obj, dict) or set(obj) != {mode["field"]} or not isinstance(obj[mode["field"]], str):
            raise ValueError("Invalid strict output schema")
        content = obj[mode["field"]]
    if mode["cleanup"]:
        content = content.translate(dict.fromkeys(map(ord, "\u200b\u200c\u200d\ufeff")))
    if not content.strip() or content.lstrip().startswith("<think>"):
        raise ValueError("Empty or reasoning-only output")
    if len(content.encode("utf-8")) > 1048576:
        raise ValueError("Output exceeds production byte limit")
    return content


def check_output(output, checks):
    """Lexical guardrails only; rubric/reference require a human semantic review."""
    def normalize(text):
        return " ".join(unicodedata.normalize("NFKC", text).casefold().split())
    text = normalize(output)

    def contains(term):
        return re.search(r"(?<!\w)" + re.escape(normalize(term)) + r"(?!\w)", text) is not None

    missing = [term for term in checks.get("required_all", []) if not contains(term)]
    missing_any = [group for group in checks.get("required_any", []) if not any(map(contains, group))]
    forbidden = [term for term in checks.get("forbidden", []) if contains(term)]
    words = len(re.findall(r"\b\w+(?:['’\-]\w+)*\b", output))
    too_long = "max_words" in checks and words > checks["max_words"]
    return {"passed": not (missing or missing_any or forbidden or too_long),
            "missing_all": missing, "missing_any": missing_any, "forbidden_found": forbidden,
            "word_count": words, "over_word_limit": too_long, "requires_human_review": True,
            "scope": "lexical guardrails, not semantic correctness"}


class NoRedirect(HTTPRedirectHandler):
    def redirect_request(self, *_args, **_kwargs):
        raise ValueError("Redirect refused: local endpoint must remain local")


def perform_request(endpoint, request, mode, timeout):
    result = {"request": request, "response": None, "response_body": None, "http_status": None,
              "cleaned_output": None, "usage": None, "timings": None,
              "finish_reason": None, "truncated": False, "error": None}
    body = json.dumps(request, ensure_ascii=False).encode("utf-8")
    # Ignore environment proxies: requests must stay on the configured loopback server.
    opener = build_opener(ProxyHandler({}), NoRedirect())
    req = Request(endpoint.rstrip("/") + "/chat/completions", data=body,
                  headers={"Content-Type": "application/json"}, method="POST")
    started = time.perf_counter()
    try:
        try:
            with opener.open(req, timeout=timeout) as response:
                result["http_status"] = response.status
                raw = response.read()
        except HTTPError as error:
            result["http_status"] = error.code
            raw = error.read()
        finally:
            # HTTP timing ends after the complete response body, before JSON/output parsing.
            result["latency_ms"] = (time.perf_counter() - started) * 1000
        result["response_body"] = raw.decode("utf-8", errors="replace")
        try:
            result["response"] = strict_json(result["response_body"])
        except ValueError:
            if result["http_status"] != 200:
                raise ValueError(f"HTTP {result['http_status']}") from None
            raise
        if result["http_status"] != 200:
            raise ValueError(f"HTTP {result['http_status']}")
        response = result["response"]
        result["usage"] = response.get("usage")
        result["timings"] = response.get("timings")
        choice = response["choices"][0]
        result["finish_reason"] = choice.get("finish_reason")
        result["truncated"] = result["finish_reason"] in ("length", "max_tokens")
        if result["truncated"]:
            raise ValueError("Truncated response")
        if result["finish_reason"] == "content_filter":
            raise ValueError("Filtered response")
        result["cleaned_output"] = clean_output(choice["message"]["content"], mode)
    except KeyboardInterrupt:
        result["error"] = {"type": "KeyboardInterrupt", "message": "Interrupted request"}
    except Exception as error:
        # Includes protocol errors such as http.client.IncompleteRead. Each
        # attempted request must produce a row even for an unusual HTTP failure.
        result["error"] = {"type": type(error).__name__, "message": str(error)}
    return result


def latency_stats(values):
    ordered = sorted(values)
    return {"samples": len(values), "min_ms": min(values) if values else None,
            "median_ms": statistics.median(values) if values else None,
            "p95_ms": ordered[math.ceil(.95 * len(values)) - 1] if values else None,
            "max_ms": max(values) if values else None}


def summarize(rows, completed=True):
    def group(items):
        return {"attempts": len(items), "failures": sum(r["error"] is not None for r in items),
                "truncated": sum(r["truncated"] for r in items),
                "lexical_passes": sum(bool((r.get("checks") or {}).get("passed")) for r in items),
                "out_of_domain": sum(r["mode"]["out_of_domain"] for r in items),
                "latency_all_attempts": latency_stats([r["latency_ms"] for r in items]),
                "latency_successful": latency_stats([r["latency_ms"] for r in items if not r["error"]]),
                "latency_first_per_case": latency_stats([r["latency_ms"] for r in items
                                                          if r["phase"] == "measured" and r["repeat"] == 1]),
                "latency_repeated": latency_stats([r["latency_ms"] for r in items
                                                    if r["phase"] == "measured" and r["repeat"] > 1])}
    measured = [r for r in rows if r["phase"] == "measured"]
    keys = sorted({(r["model_id"], r["category"]) for r in measured})
    return {"version": VERSION, "completed": completed,
            "percentile_method": "nearest-rank p95; descriptive small-sample statistics, not an SLA estimate",
            "quality_note": "Lexical checks are guardrails only. Review rubric/reference and full outputs for meaning, omissions and invention.",
            "measured": group(measured), "warmup": group([r for r in rows if r["phase"] == "warmup"]),
            "models": [{"model_id": model, **group([r for r in measured if r["model_id"] == model])}
                       for model in sorted({r["model_id"] for r in measured})],
            "categories": [{"category": category, **group([r for r in measured if r["category"] == category])}
                           for category in sorted({r["category"] for r in measured})],
            "groups": [{"model_id": model, "category": category, **group([
                r for r in measured if r["model_id"] == model and r["category"] == category])}
                       for model, category in keys]}


def validate(config, corpus):
    if corpus.get("version") != 1 or not corpus.get("cases") or not config.get("models"):
        raise ValueError("Expected version 1 corpus with cases and config with models")
    for key, items in (("model", config["models"]), ("case", corpus["cases"])):
        ids = [item["id"] for item in items]
        if len(ids) != len(set(ids)):
            raise ValueError(f"Duplicate {key} IDs")
    for model in config["models"]:
        endpoint = urlsplit(model["endpoint"])
        if (endpoint.scheme != "http" or endpoint.hostname not in ("127.0.0.1", "localhost", "::1")
                or endpoint.username or endpoint.password or endpoint.query or endpoint.fragment):
            raise ValueError("Only credential-free loopback HTTP endpoints are supported")
        if model["adapter"] not in ("qwen", "s1") or not model["model"]:
            raise ValueError("Expected qwen/s1 adapter and nonempty model")
        if type(model.get("supports_structured_output", False)) is not bool:
            raise ValueError("supports_structured_output must be a boolean")
    for case in corpus["cases"]:
        if case["kind"] not in ("cleanup", "assistant") or case["language"] not in ("en", "pl", "de"):
            raise ValueError("Invalid case kind/language")
        for field in ("id", "category", "input", "instruction", "reference"):
            if not isinstance(case[field], str):
                raise ValueError(f"Case {field} must be a string")
        if case["kind"] == "cleanup" and not case["input"].strip():
            raise ValueError("Cleanup input must be nonempty")
        if case["kind"] == "assistant" and not case["instruction"].strip():
            raise ValueError("Assistant instruction must be nonempty")
        checks = case["checks"]
        if not isinstance(checks, dict) or not isinstance(case["rubric"], list):
            raise ValueError("Case checks/rubric have invalid shape")
        for field in ("required_all", "forbidden"):
            if not isinstance(checks.get(field, []), list) or any(not isinstance(x, str) or not x.strip() for x in checks.get(field, [])):
                raise ValueError(f"Invalid {field} checks")
        if not isinstance(checks.get("required_any", []), list) or any(
                not isinstance(g, list) or not g or any(not isinstance(x, str) or not x.strip() for x in g)
                for g in checks.get("required_any", [])):
            raise ValueError("Invalid required_any checks")
        if "max_words" in checks and (type(checks["max_words"]) is not int or checks["max_words"] < 1):
            raise ValueError("max_words must be a positive integer")


def write_json(path, value):
    with path.open("x", encoding="utf-8") as handle:
        json.dump(value, handle, ensure_ascii=False, indent=2)
        handle.write("\n")


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--config", type=Path, required=True)
    parser.add_argument("--corpus", type=Path, required=True)
    parser.add_argument("--model", action="append", required=True, help="Config ID; repeat to run serially")
    parser.add_argument("--output", type=Path, required=True, help="New run directory; existing paths refused")
    parser.add_argument("--repeats", type=int, default=3)
    parser.add_argument("--warmup", type=int, default=1, help="Neutral synthetic requests per model; excluded from measurements")
    parser.add_argument("--case", action="append", default=[])
    parser.add_argument("--category", action="append", default=[])
    parser.add_argument("--timeout", type=float, default=120)
    parser.add_argument("--max-tokens", type=int, default=768)
    args = parser.parse_args(argv)
    if args.repeats < 1 or args.warmup < 0 or args.timeout <= 0 or args.max_tokens < 1:
        parser.error("repeats, timeout, max-tokens must be positive; warmup must be nonnegative")
    config_bytes, corpus_bytes = args.config.read_bytes(), args.corpus.read_bytes()
    config, corpus = strict_json(config_bytes.decode("utf-8-sig")), strict_json(corpus_bytes.decode("utf-8-sig"))
    validate(config, corpus)
    models = [m for m in config["models"] if m["id"] in args.model]
    if {m["id"] for m in models} != set(args.model) or len(set(args.model)) != len(args.model):
        parser.error("Unknown or duplicate --model ID")
    for field, requested in (("id", args.case), ("category", args.category)):
        if set(requested) - {c[field] for c in corpus["cases"]}:
            parser.error(f"Unknown case {field} filter")
    cases = [c for c in corpus["cases"] if (not args.case or c["id"] in args.case)
             and (not args.category or c["category"] in args.category)]
    if not cases:
        parser.error("Filters selected no cases")
    prompts, sources = load_prompts()
    warmup_case = dict(cases[0], id="__warmup__", category="__warmup__", language="en",
                       input="The benchmark is ready period", checks={}, rubric=[],
                       instruction="" if cases[0]["kind"] == "cleanup" else
                       "Clean up this dictated transcript in its original language. Return only cleaned text.",
                       reference="The benchmark is ready.")
    if any(case["input"] == warmup_case["input"] for case in cases):
        parser.error("Measured corpus contains reserved neutral warmup text")
    args.output.mkdir(parents=True, exist_ok=False)
    manifest = {"version": VERSION, "created_at": utc_now(), "python": sys.version,
                "platform": platform.platform(), "config_sha256": sha256(config_bytes),
                "corpus_sha256": sha256(corpus_bytes), "source_sha256": sources,
                "runner_sha256": sha256(Path(__file__).read_bytes()), "models": models,
                "prompt_snapshot": prompts, "selected_cases": [c["id"] for c in cases],
                "exploratory_cleanup_instruction": EXPLORATORY_CLEANUP_INSTRUCTION,
                "warmup_case": warmup_case,
                "wire_differences_from_production": [
                    "Evaluation fixes temperature=0 and max_tokens from CLI instead of persisted request options.",
                    "Evaluation adds seed=42 and chat_template_kwargs.enable_thinking=false; Handy does not send these fields.",
                    "Custom Handy and evaluation both send reasoning_effort=none initially; evaluation never retries without it.",
                    "Evaluation uses built-in prompts without custom vocabulary; user-edited prompts are outside this harness.",
                ],
                "settings": {"repeats": args.repeats, "warmup": args.warmup, "timeout": args.timeout,
                             "max_tokens": args.max_tokens, "temperature": 0, "seed": 42,
                             "order": "config model order; warmups first; corpus case order then repeats"},
                "planned_measured_rows": len(models) * len(cases) * args.repeats,
                "retention": "Evaluation evidence; retain with benchmark report until explicitly retired.",
                "measurement": "perf_counter total HTTP request/body latency; no streaming TTFT; no retries; warmups excluded"}
    write_json(args.output / "manifest.json", manifest)
    # Freeze exact evaluated input/config; do not rely on mutable source paths later.
    with (args.output / "corpus.json").open("xb") as handle:
        handle.write(corpus_bytes)
    with (args.output / "config.json").open("xb") as handle:
        handle.write(config_bytes)
    rows, completed = [], False
    with (args.output / "rows.jsonl").open("x", encoding="utf-8") as handle:
        try:
            for model in models:
                schedule = [("warmup", warmup_case, i) for i in range(args.warmup)] + [
                    ("measured", case, i) for case in cases for i in range(args.repeats)]
                for phase, case, repeat in schedule:
                    request, mode = build_request(model, case, prompts, args.max_tokens)
                    row = {"model_id": model["id"], "case_id": case["id"], "category": case["category"],
                           "phase": phase, "repeat": repeat + 1, "started_at": utc_now(), "case": case, "mode": mode}
                    row.update(perform_request(model["endpoint"], request, mode, args.timeout))
                    row["checks"] = check_output(row["cleaned_output"], case["checks"]) if not row["error"] else None
                    handle.write(json.dumps(row, ensure_ascii=False) + "\n")
                    handle.flush()
                    rows.append(row)
                    if row["error"] and row["error"]["type"] == "KeyboardInterrupt":
                        raise KeyboardInterrupt
            completed = True
        except KeyboardInterrupt:
            completed = False
        finally:
            write_json(args.output / "summary.json", summarize(rows, completed))
    return 130 if not completed else int(any(row["error"] for row in rows))


if __name__ == "__main__":
    try:
        sys.exit(main())
    except (OSError, ValueError, KeyError, TypeError) as error:
        print(f"Evaluation failed: {type(error).__name__}: {error}", file=sys.stderr)
        sys.exit(2)
