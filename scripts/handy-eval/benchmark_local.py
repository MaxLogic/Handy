"""Serial, owned-process launcher for the endpoint-only Handy evaluation runner.

No existing server is stopped. Each invocation creates a new evidence directory;
only the exact server child it starts is terminated. No shell is involved.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import socket
import subprocess
import sys
import threading
import time
import urllib.error
import urllib.request
from urllib.parse import urlparse

HERE = Path(__file__).resolve().parent
NO_WINDOW = subprocess.CREATE_NO_WINDOW if os.name == "nt" else 0


def file_hash(path: Path) -> str:
    with path.open("rb") as source:
        return hashlib.file_digest(source, "sha256").hexdigest()


def gpu_sample() -> dict:
    try:
        result = subprocess.run(
            ["nvidia-smi", "--query-gpu=memory.used,memory.free,utilization.gpu,temperature.gpu", "--format=csv,noheader,nounits"],
            capture_output=True, text=True, timeout=5, creationflags=NO_WINDOW,
            check=True,
        )
        rows = [list(map(float, row.split(","))) for row in result.stdout.strip().splitlines()]
        return {"time_ns": time.time_ns(), "gpus": [dict(zip(
            ["used_mib", "free_mib", "utilization_percent", "temperature_c"], row)) for row in rows]}
    except (OSError, ValueError, subprocess.SubprocessError) as error:
        return {"time_ns": time.time_ns(), "error": str(error)}


def watch_gpu(path: Path, stopped: threading.Event) -> None:
    with path.open("x", encoding="utf-8") as out:
        while not stopped.is_set():
            out.write(json.dumps(gpu_sample()) + "\n")
            out.flush()
            stopped.wait(0.5)


def wait_ready(process: subprocess.Popen, endpoint: str, model: str, timeout: float) -> None:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if process.poll() is not None:
            raise RuntimeError(f"Owned server exited with code {process.returncode}; inspect server logs")
        try:
            with urllib.request.urlopen(endpoint + "/models", timeout=2) as response:
                data = json.load(response)
            ids = [entry["id"] for entry in data.get("data", [])]
            if ids != [model]:
                raise RuntimeError(f"Unexpected server model identity: {ids}")
            return
        except (urllib.error.URLError, TimeoutError):
            time.sleep(0.25)
    raise TimeoutError(f"Server readiness exceeded {timeout} seconds")


def build_server_command(exe: str, model: dict) -> list[str]:
    """Build an owned loopback server command, with opt-in speculation controls."""
    server = model["server"]
    spec_type = server.get("spec_type", "none")
    if spec_type not in ("none", "draft-mtp"):
        raise ValueError("model.server.spec_type must be 'none' or 'draft-mtp'")
    if "spec_draft_n_max" in server:
        draft_max = server["spec_draft_n_max"]
        if spec_type != "draft-mtp":
            raise ValueError("model.server.spec_draft_n_max requires spec_type 'draft-mtp'")
        if type(draft_max) is not int or not 1 <= draft_max <= 16:
            raise ValueError("model.server.spec_draft_n_max must be an integer from 1 to 16")
    parsed = urlparse(model["endpoint"])
    if parsed.hostname != "127.0.0.1" or not parsed.port:
        raise ValueError("Owned local-server driver requires explicit 127.0.0.1 endpoint/port")
    model_path = Path(model["server"]["model_path"]).resolve(strict=True)
    command = [exe, "-m", str(model_path), "--alias", model["model"],
            "--host", parsed.hostname, "--port", str(parsed.port),
            "--ctx-size", str(model["server"].get("context", 4096)),
            "--parallel", "1", "--n-gpu-layers", "99", "--flash-attn", "on",
            "--reasoning", "off", "--jinja", "--temp", "0", "--threads", "16",
            "--sleep-idle-seconds", "-1"]
    if "spec_type" in server:
        command.extend(["--spec-type", spec_type])
    if "spec_draft_n_max" in server:
        command.extend(["--spec-draft-n-max", str(server["spec_draft_n_max"])])
    return command


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--config", type=Path, default=HERE / "models.local.json")
    parser.add_argument("--corpus", type=Path, default=HERE / "cases.json")
    parser.add_argument("--output", type=Path, required=True, help="New, nonexistent directory")
    parser.add_argument("--backends", nargs="+", default=["cuda"])
    parser.add_argument("--models", nargs="+", default=["qwen9b", "qwen4b", "s1mini"])
    parser.add_argument("--repeats", type=int, default=3)
    parser.add_argument("--case", action="append", default=[])
    parser.add_argument("--timeout", type=float, default=120)
    args = parser.parse_args()
    config = json.loads(args.config.read_text(encoding="utf-8-sig"))
    models = {model["id"]: model for model in config["models"]}
    for model_id in args.models:
        if model_id not in models:
            parser.error(f"Unknown model {model_id}")
    for backend in args.backends:
        if backend not in config["backends"] or not Path(config["backends"][backend]).is_file():
            parser.error(f"Missing backend {backend}")
    args.output.mkdir(parents=True, exist_ok=False)
    record = {"created_unix": time.time(), "config": config, "corpus_sha256": hashlib.sha256(args.corpus.read_bytes()).hexdigest(),
              "driver_sha256": hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
              "args": {key: str(value) if isinstance(value, Path) else value for key, value in vars(args).items()},
              "gpu_initial": gpu_sample(), "runs": []}
    failed = False
    for backend in args.backends:
        exe = str(Path(config["backends"][backend]).resolve())
        version = subprocess.run([exe, "--version"], capture_output=True, text=True, creationflags=NO_WINDOW, timeout=30)
        for model_id in args.models:
            model = models[model_id]
            parsed = urlparse(model["endpoint"])
            if parsed.hostname != "127.0.0.1" or not parsed.port:
                raise ValueError("Owned local-server driver requires explicit 127.0.0.1 endpoint/port")
            # A conflicting listener is an error, never permission to stop its owner.
            with socket.socket() as probe:
                probe.bind((parsed.hostname, parsed.port))
            folder = args.output / f"{backend}-{model_id}"
            folder.mkdir()
            model_path = Path(model["server"]["model_path"]).resolve(strict=True)
            command = build_server_command(exe, model)
            info = {"backend": backend, "model": model_id, "command": command,
                    "version": version.stdout + version.stderr, "exe_sha256": file_hash(Path(exe)),
                    "model_sha256": file_hash(model_path),
                    "gpu_before_server": gpu_sample()}
            process = None
            stopped = threading.Event()
            monitor = None
            try:
                with (folder / "server.stdout.log").open("x", encoding="utf-8") as stdout, (folder / "server.stderr.log").open("x", encoding="utf-8") as stderr:
                    began = time.perf_counter()
                    process = subprocess.Popen(command, stdout=stdout, stderr=stderr, creationflags=NO_WINDOW)
                    info["pid"] = process.pid
                    wait_ready(process, model["endpoint"], model["model"], 180)
                    info["load_seconds"] = time.perf_counter() - began
                    info["gpu_loaded"] = gpu_sample()
                    monitor = threading.Thread(target=watch_gpu, args=(folder / "gpu-samples.jsonl", stopped), daemon=True)
                    monitor.start()
                    runner = [sys.executable, str(HERE / "runner.py"), "--config", str(args.config.resolve()),
                              "--corpus", str(args.corpus.resolve()), "--model", model_id,
                              "--output", str(folder / "evaluation"), "--repeats", str(args.repeats),
                              "--warmup", "1", "--timeout", str(args.timeout), "--max-tokens", "768"]
                    for case in args.case:
                        runner.extend(["--case", case])
                    print(f"RUN {backend} {model_id} pid={process.pid} loaded={info['load_seconds']:.2f}s", flush=True)
                    # A single long-lived child; runner itself imposes per-request deadlines.
                    with (folder / "runner.log").open("x", encoding="utf-8") as log:
                        result = subprocess.run(runner, stdout=log, stderr=subprocess.STDOUT, creationflags=NO_WINDOW)
                    info["runner_exit_code"] = result.returncode
                    failed |= result.returncode != 0
            except Exception as error:
                info["error"] = f"{type(error).__name__}: {error}"
                failed = True
            finally:
                stopped.set()
                if monitor is not None:
                    monitor.join(timeout=7)
                if process is not None and process.poll() is None:
                    process.terminate()
                    try:
                        process.wait(timeout=15)
                    except subprocess.TimeoutExpired:
                        process.kill()
                        process.wait(timeout=5)
                info["gpu_after_server"] = gpu_sample()
                (folder / "server-manifest.json").write_text(json.dumps(info, indent=2), encoding="utf-8")
                record["runs"].append(info)
                (args.output / "batch-manifest.json").write_text(json.dumps(record, indent=2), encoding="utf-8")
                print(f"DONE {backend} {model_id} exit={info.get('runner_exit_code')} error={info.get('error')}", flush=True)
    return int(failed)


if __name__ == "__main__":
    raise SystemExit(main())
