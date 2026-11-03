#!/usr/bin/env python3
"""Opt-in real-model protocol/privacy checks and reproducible timing measurements.

Build with scripts/build-sidecar.sh, then run:
  python3 crates/murmur-llm/check.py checks
  python3 crates/murmur-llm/check.py messages
  python3 crates/murmur-llm/check.py qwen --model ~/.murmur-dev-models/llm/Qwen3.5-0.8B-Q4_K_M.gguf

MURMUR_SIDECAR_BINARY overrides the executable. Models must already exist; no downloads.

Only numeric results are written to disk. Prompts, output and stderr stay in memory.
"""
import argparse
import json
import math
import os
from pathlib import Path
import resource
import selectors
import statistics
import subprocess
import time

ROOT = Path(__file__).resolve().parent
BINARY = Path(os.environ.get("MURMUR_SIDECAR_BINARY", str(ROOT / "target/release/murmur-llm")))
MODEL = Path.home() / ".murmur-dev-models/llm/LFM2-350M-ENJP-MT-Q4_K_M.gguf"
SYSTEM = "Translate to English."
# Exactly 60 Unicode characters, shared by every timing scenario.
TEXT = "明日の会議は午後三時から。資料を確認して、変更があれば午前中に連絡してください。参加できない場合も早めに教えてください。"


class Client:
    def __init__(self):
        self.started = time.perf_counter()
        self.proc = subprocess.Popen([str(BINARY)], stdin=subprocess.PIPE,
                                     stdout=subprocess.PIPE, stderr=subprocess.PIPE, bufsize=0)
        self.selector = selectors.DefaultSelector()
        self.selector.register(self.proc.stdout, selectors.EVENT_READ)
        self.next_id = 0
        self.pending = b""

    def send(self, op, **kwargs):
        self.next_id += 1
        message = dict(id=self.next_id, op=op, **kwargs)
        self.proc.stdin.write(json.dumps(message, ensure_ascii=False).encode() + b"\n")
        self.proc.stdin.flush()
        return self.next_id

    def receive(self, timeout=65):
        deadline = time.monotonic() + timeout
        while b"\n" not in self.pending:
            remaining = deadline - time.monotonic()
            if remaining <= 0 or not self.selector.select(remaining):
                raise TimeoutError("Sidecar response deadline exceeded")
            chunk = os.read(self.proc.stdout.fileno(), 65536)
            if not chunk:
                raise RuntimeError("Sidecar closed stdout")
            self.pending += chunk
        line, self.pending = self.pending.split(b"\n", 1)
        return json.loads(line)

    def request(self, op, **kwargs):
        request_id = self.send(op, **kwargs)
        response = self.receive()
        if response["id"] != request_id:
            raise AssertionError("Unexpected response id")
        return response

    def load(self, model=MODEL, ctx=2048):
        result = self.request("load", model=str(model), ctx=ctx)
        assert result["ok"], result
        return result

    def generate(self, system=SYSTEM, user=TEXT, max_tokens=160):
        result = self.request("generate", system=system, user=user,
                              max_tokens=max_tokens, temperature=0)
        assert result["ok"], {k: v for k, v in result.items() if k != "text"}
        return result

    def generate_messages(self, messages, max_tokens=160, **kwargs):
        result = self.request("generate", messages=messages, max_tokens=max_tokens,
                              temperature=0, **kwargs)
        assert result["ok"], {k: v for k, v in result.items() if k != "text"}
        return result

    def close(self):
        self.proc.stdin.close()
        try:
            self.proc.wait(timeout=2)
        except subprocess.TimeoutExpired:
            self.proc.kill()
            self.proc.wait(timeout=2)
            raise AssertionError("Sidecar did not exit on stdin EOF")
        stderr = self.proc.stderr.read()
        self.proc.stderr.close()
        self.proc.stdout.close()
        self.selector.close()
        assert self.proc.returncode == 0, dict(exit_code=self.proc.returncode, stderr_bytes=len(stderr))
        return stderr


def checks(model):
    client = Client()
    client.proc.stdin.write(b"not json\n")
    client.proc.stdin.flush()
    assert client.receive()["error"] == "invalid_request"
    assert client.request("warm", system=SYSTEM)["error"] == "model_not_loaded"
    assert not client.request("load", model="/nonexistent/model.gguf", ctx=2048)["ok"]
    loaded = client.load(model)
    sentinel = "MURMUR_PRIVATE_SENTINEL_7f9b2c"
    first = client.generate(user=TEXT + sentinel)
    baseline = client.generate()
    warm = client.request("warm", system=SYSTEM)
    assert warm["ok"]
    cached = client.generate()
    assert cached["cached_tokens"] > 0
    assert cached["text"] == baseline["text"], "Warmed output differs from uncached output"
    repeated = client.generate()
    assert cached["text"] == repeated["text"], "Cached recurrent state changed output"
    changed = client.generate(system="Translate to Japanese.", user="Please check the document.")
    assert changed["cached_tokens"] == 0
    again = client.generate()
    assert again["text"] == cached["text"]
    assert not client.request("generate", system=SYSTEM, user=TEXT,
                              max_tokens=160, temperature=0.1)["ok"]
    assert not client.request("generate", system=SYSTEM, user=TEXT,
                              max_tokens=8192, temperature=0)["ok"]
    # A sufficiently long prompt ensures the generation is still active when cancel arrives.
    target = client.send("generate", system=SYSTEM, user=TEXT * 15,
                         max_tokens=512, temperature=0)
    busy_id = client.send("warm", system=SYSTEM)
    busy = client.receive()
    assert busy["id"] == busy_id and busy["error"] == "busy"
    # lsof defaults to OR; -a is necessary to restrict socket selection to this PID.
    socket_check = subprocess.run(["lsof", "-a", "-p", str(client.proc.pid), "-i"],
                                  capture_output=True, timeout=5)
    cancel_start = time.perf_counter()
    cancel_id = client.send("cancel", target=target)
    replies = [client.receive(), client.receive()]
    cancel_ms = (time.perf_counter() - cancel_start) * 1000
    by_id = {response["id"]: response for response in replies}
    assert by_id[cancel_id]["ok"]
    assert by_id[target].get("error") == "cancelled"
    assert cancel_ms < 500, cancel_ms
    assert socket_check.returncode == 1
    assert socket_check.stdout == b""
    stderr = client.close()
    assert stderr == b"", "Native stderr was not empty"
    # EOF during a busy operation must not leave a worker process behind.
    eof_client = Client()
    eof_client.load(model)
    eof_client.send("generate", system=SYSTEM, user=TEXT * 15,
                    max_tokens=512, temperature=0)
    eof_start = time.perf_counter()
    assert eof_client.close() == b""
    return dict(protocol_checks="passed", gpu_offload_supported=loaded["gpu_offload_supported"],
                stderr_bytes=len(stderr), sentinel_in_stderr=sentinel.encode() in stderr,
                lsof_command=f"lsof -a -p {client.proc.pid} -i",
                lsof_exit=socket_check.returncode, lsof_stdout_bytes=len(socket_check.stdout),
                lsof_stderr_bytes=len(socket_check.stderr), cancel_ms=cancel_ms,
                busy_eof_exit_ms=(time.perf_counter() - eof_start) * 1000,
                warm_tokens=warm["cached_tokens"], generated_tokens=first["generated_tokens"])


def messages_checks(model):
    client = Client()
    client.load(model)
    fixed = [dict(role="system", content=SYSTEM),
             dict(role="user", content="おはようございます。"),
             dict(role="assistant", content="Good morning.")]
    full = fixed + [dict(role="user", content=TEXT)]
    baseline = client.generate_messages(full)
    assert baseline["cached_tokens"] == 0
    warmed = client.request("warm", messages=fixed)
    assert warmed["ok"]
    cached = client.generate_messages(full)
    assert cached["cached_tokens"] == warmed["cached_tokens"] > 10
    assert cached["text"] == baseline["text"]
    repeated = client.generate_messages(full)
    assert repeated["text"] == baseline["text"]
    different_user = client.generate_messages(fixed + [dict(role="user", content="資料を確認してください。")])
    assert different_user["cached_tokens"] == cached["cached_tokens"]
    changed_fixed = fixed[:-1] + [dict(role="assistant", content="Good morning. ")]
    changed = client.generate_messages(changed_fixed + [full[-1]])
    assert changed["cached_tokens"] == 0
    assert client.generate_messages(full)["text"] == baseline["text"]
    ambiguous = client.request("warm", messages=fixed, system=SYSTEM)
    assert ambiguous["error"] == "ambiguous_chat_input"
    assert not client.request("warm", messages=full)["ok"]
    assert not client.request("generate", messages=fixed, max_tokens=160)["ok"]
    assert not client.request("warm", messages=[])["ok"]
    assert client.request("warm", messages=fixed[:1])["ok"]
    assert client.generate()["cached_tokens"] > 0
    assert client.request("warm", system=SYSTEM)["ok"]
    assert client.generate_messages([fixed[0], full[-1]])["cached_tokens"] > 0
    stderr = client.close()
    assert stderr == b""
    return dict(messages_checks="passed", cold_equals_warm=True,
                repeat_isolation=True, exact_fixed_part_matching=True,
                legacy_messages_cache_interop=True, cached_tokens=cached["cached_tokens"],
                prompt_tokens=cached["prompt_tokens"], generated_tokens=cached["generated_tokens"],
                changed_fixed_cached_tokens=changed["cached_tokens"], stderr_bytes=len(stderr))


def qwen_checks(model):
    if not model.is_file():
        raise FileNotFoundError("Requested existing Qwen model is absent; do not download implicitly")
    client = Client()
    client.load(model)
    fixed = [dict(role="system", content="Follow instructions exactly. Output only the requested answer."),
             dict(role="user", content="Reply with exactly READY and nothing else."),
             dict(role="assistant", content="READY")]
    full = fixed + [dict(role="user", content="Reply with exactly READY and nothing else.")]
    cold = client.generate_messages(full, max_tokens=64)
    assert cold["no_think"] is True
    assert cold["text"].strip() == "READY", "Non-thinking answer contains unexpected content"
    warmed = client.request("warm", messages=fixed)
    assert warmed["ok"]
    cached = client.generate_messages(full, max_tokens=64)
    assert cached["text"] == cold["text"] and cached["cached_tokens"] > 0
    explicit = client.request("generate", system=fixed[0]["content"],
                              user=full[-1]["content"], max_tokens=64, temperature=0, no_think=True)
    assert explicit["ok"] and explicit["text"].strip() == "READY"
    checked = [cold, cached, explicit]
    for response in checked:
        assert "<think>" not in response["text"].lower()
        assert "</think>" not in response["text"].lower()
        assert response["stop_reason"] == "eos"
    # Do not strip reasoning or tags from the output: this tests actual generation-prefix control.
    control = client.request("generate", system=fixed[0]["content"], user="What is 17 times 23?",
                             max_tokens=64, temperature=0, no_think=False)
    assert control["ok"] and control["no_think"] is False
    stderr = client.close()
    assert stderr == b""
    return dict(qwen_checks="passed", non_thinking_requests=len(checked), all_exact_ready=True,
                think_tags_in_non_thinking_output=False,
                cached_tokens=cached["cached_tokens"],
                generated_tokens=[response["generated_tokens"] for response in checked],
                explicit_thinking_control_has_think_tag="<think>" in control["text"],
                control_stop_reason=control["stop_reason"], stderr_bytes=len(stderr))


def summary(values):
    ordered = sorted(values)
    return dict(median=statistics.median(values), p95=ordered[math.ceil(len(values) * .95) - 1],
                runs=values)


def benchmark(model, runs):
    assert len(TEXT) == 60, len(TEXT)
    values = {key: [] for key in ("cold", "load_only", "load_warm", "spawn_load", "warm")}
    details = {key: [] for key in ("cold", "load_only", "load_warm")}
    loaded_rss_kib = []
    # Each measurement starts a new process: no generated-user cache can leak across cases.
    for iteration in range(runs):
        for mode in ("cold", "load_only", "load_warm"):
            client = Client()
            client.load(model)
            spawn_load_ms = (time.perf_counter() - client.started) * 1000
            if mode == "cold":
                values["spawn_load"].append(spawn_load_ms)
            if mode == "load_warm":
                start = time.perf_counter()
                result = client.request("warm", system=SYSTEM)
                assert result["ok"]
                values["warm"].append((time.perf_counter() - start) * 1000)
            start = client.started if mode == "cold" else time.perf_counter()
            result = client.generate()
            values[mode].append((time.perf_counter() - start) * 1000)
            details[mode].append({key: result[key] for key in (
                "prompt_ms", "gen_ms", "prompt_tokens", "evaluated_prompt_tokens",
                "cached_tokens", "generated_tokens", "stop_reason")})
            rss = subprocess.run(["ps", "-o", "rss=", "-p", str(client.proc.pid)],
                                 capture_output=True, check=True, timeout=5)
            loaded_rss_kib.append(int(rss.stdout.strip()))
            assert client.close() == b""
        print(f"Completed timing run {iteration + 1}/{runs}", flush=True)
    return dict(input_characters=len(TEXT), max_tokens=160, temperature=0,
                p95_method="nearest rank, ceil(0.95*n), so max for n=10",
                timings_ms={key: summary(value) for key, value in values.items()},
                generation_details=details, max_loaded_rss_kib=max(loaded_rss_kib),
                peak_child_rss_bytes=resource.getrusage(resource.RUSAGE_CHILDREN).ru_maxrss)


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("mode", choices=["checks", "messages", "qwen", "bench"])
    parser.add_argument("--model", type=Path, default=MODEL)
    parser.add_argument("--runs", type=int, default=10)
    parser.add_argument("--output", type=Path)
    args = parser.parse_args()
    if args.mode == "bench":
        result = benchmark(args.model, args.runs)
    else:
        result = {"checks": checks, "messages": messages_checks, "qwen": qwen_checks}[args.mode](args.model)
    result["model"] = str(args.model)
    encoded = json.dumps(result, indent=2)
    print(encoded)
    if args.output:
        args.output.write_text(encoded + "\n")
