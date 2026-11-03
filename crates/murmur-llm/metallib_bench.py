#!/usr/bin/env python3
"""Measure startup/prefill without saving prompts, answers or native stderr.

Supply MURMUR_SIDECAR_BINARY and --output under target/. Models must already exist.
The cleanup prefix is read from the core's current fixed prompt constants.
"""
import argparse
import json
import os
from pathlib import Path
import re
import subprocess
import threading
import time

from check import Client

ROOT = Path(__file__).resolve().parent
# Same model location as the app's real-sidecar test (src-tauri/src/adapters/refiner.rs).
MODELS = Path(os.environ.get("MURMUR_TEST_MODELS", Path.home() / ".murmur-dev-models")) / "llm"
FILES = {
    "translate": "LFM2-350M-ENJP-MT-Q4_K_M.gguf",
    "clean": "Qwen3.5-0.8B-Q4_K_M.gguf",
}
# Synthetic fixture only. No spoken or generated text is persisted by the runner.
TEXT = ("えっと、来週の打ち合わせは火曜日の午後三時から会議室で行います。"
        "参加する方は月曜日の午前中までに資料を確認して、質問があれば担当者に連絡してください。"
        "当日は新しい計画の進め方と作業の分担、それから今月中に終える項目を相談します。"
        "私は議事録をまとめて、会議が終わったあとに全員へメールで共有する予定です。"
        "必要な図と表も用意しておきます。")


def clean_prefix():
    source = (ROOT.parent / "murmur-core/src/refine.rs").read_text()

    def constant(name):
        match = re.search(r'const ' + name + r': &str = "((?:[^"\\]|\\.)*)";', source)
        assert match, "Core prompt constant not found"
        return json.loads('"' + match.group(1) + '"', strict=False)

    block = source.split("const CLEAN_EXAMPLES:", 1)[1].split("];", 1)[0]
    values = [json.loads('"' + value + '"', strict=False)
              for value in re.findall(r'"((?:[^"\\]|\\.)*)"', block)]
    assert len(values) == 12, "Expected six fixed cleanup examples"
    messages = [dict(role="system", content=constant("CLEAN_SYSTEM_RULES")
                     + constant("CLEAN_SYSTEM_END"))]
    for index in range(0, len(values), 2):
        messages += [dict(role="user", content=f"<transcript>{values[index]}</transcript>"),
                     dict(role="assistant", content=values[index + 1])]
    return messages


class CompilerCpu:
    """Sample total MTLCompilerService CPU; unrelated desktop compiler activity is included."""
    def __init__(self):
        self.stop = threading.Event()
        self.peak = 0.0
        self.samples = 0
        self.thread = threading.Thread(target=self.sample, daemon=True)

    def sample(self):
        while not self.stop.is_set():
            try:
                result = subprocess.run(["ps", "-axo", "pcpu=,comm="], capture_output=True,
                                        timeout=2, check=True)
                total = 0.0
                for line in result.stdout.decode().splitlines():
                    fields = line.strip().split(None, 1)
                    if len(fields) == 2 and "MTLCompilerService" in fields[1]:
                        total += float(fields[0])
                self.peak = max(self.peak, total)
                self.samples += 1
            except (subprocess.SubprocessError, ValueError):
                pass
            self.stop.wait(.1)

    def __enter__(self):
        self.thread.start()
        return self

    def __exit__(self, *_args):
        self.stop.set()
        self.thread.join(timeout=3)
        assert not self.thread.is_alive(), "CPU sampler did not stop"


def close(client):
    assert client.close() == b"", "Sidecar stderr was not silent"


def load(client, role):
    started = time.perf_counter()
    answer = client.load(MODELS / FILES[role], ctx=2048)
    return dict(role=role, load_ms=answer["load_ms"],
                wall_ms=(time.perf_counter() - started) * 1000,
                gpu_offload_supported=answer["gpu_offload_supported"])


def measure():
    result = dict(four_live=[], solo=[], first_prefill=[], synthetic_input_chars=len(TEXT))
    clients = []
    try:
        # Keep all earlier children alive so four simultaneous Metal device/cache users exist.
        for role in ["translate", "clean", "translate", "clean"]:
            with CompilerCpu() as cpu:
                client = Client()
                clients.append(client)
                row = load(client, role)
            row.update(compiler_peak_cpu=cpu.peak, cpu_samples=cpu.samples)
            result["four_live"].append(row)
    finally:
        for client in clients:
            close(client)

    # Three fresh processes per model, each alone (normal filesystem/OS caches retained).
    for role in FILES:
        for run in range(3):
            with CompilerCpu() as cpu:
                client = Client()
                try:
                    row = load(client, role)
                finally:
                    close(client)
            row.update(run=run + 1, compiler_peak_cpu=cpu.peak)
            result["solo"].append(row)

    for role in FILES:
        prefix = [dict(role="system", content="Translate to English.")] if role == "translate" else clean_prefix()
        user = TEXT if role == "translate" else f"<transcript>{TEXT}</transcript>"
        with CompilerCpu() as cpu:
            client = Client()
            try:
                row = load(client, role)
                warm = client.request("warm", messages=prefix)
                assert warm["ok"], "Warm failed"
                answer = client.generate_messages(prefix + [dict(role="user", content=user)],
                                                  max_tokens=192)
                assert answer["evaluated_prompt_tokens"] >= 84, "Fixture too short for prefill check"
                row.update(warm_prompt_ms=warm["prompt_ms"],
                           prompt_ms=answer["prompt_ms"], gen_ms=answer["gen_ms"],
                           evaluated_prompt_tokens=answer["evaluated_prompt_tokens"],
                           cached_tokens=answer["cached_tokens"],
                           generated_tokens=answer["generated_tokens"],
                           reached_eos=int(answer["stop_reason"] == "eos"))
            finally:
                close(client)
        row["compiler_peak_cpu"] = cpu.peak
        result["first_prefill"].append(row)
    return result


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--label", required=True)
    args = parser.parse_args()
    assert os.environ.get("MURMUR_SIDECAR_BINARY"), "Explicit binary required"
    for name in FILES.values():
        assert (MODELS / name).is_file(), "Existing E2E model missing; no download allowed"
    results = measure()
    results["label"] = args.label
    args.output.write_text(json.dumps(results, indent=2) + "\n")
    print(json.dumps(results, indent=2))
