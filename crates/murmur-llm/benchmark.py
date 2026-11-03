#!/usr/bin/env python3
"""Run the approved fixed-prefix benchmark. Persist numeric metrics, never model output."""
import argparse
import hashlib
import json
import math
import os
from pathlib import Path
import signal
import statistics
import time

from check import Client

MODELS = Path.home() / ".murmur-dev-models/llm"


def summarize(values):
    ordered = sorted(values)
    return dict(median=statistics.median(values),
                p95=ordered[math.ceil(.95 * len(ordered)) - 1],
                minimum=ordered[0], maximum=ordered[-1])


def deadline(_signum, _frame):
    raise TimeoutError("Sidecar EOF exit deadline exceeded")


def close_with_peak_rss(client):
    # wait4 gives THIS process's peak RSS, unlike the cumulative RUSAGE_CHILDREN maximum.
    client.proc.stdin.close()
    signal.setitimer(signal.ITIMER_REAL, 2)
    try:
        _, status, usage = os.wait4(client.proc.pid, 0)
    except TimeoutError:
        client.proc.kill()
        client.proc.wait(timeout=2)
        raise
    finally:
        signal.setitimer(signal.ITIMER_REAL, 0)
    client.proc.returncode = os.waitstatus_to_exitcode(status)
    stderr = client.proc.stderr.read()
    client.proc.stderr.close()
    client.proc.stdout.close()
    client.selector.close()
    assert client.proc.returncode == 0, client.proc.returncode
    assert not stderr, dict(stderr_bytes=len(stderr))
    # On macOS ru_maxrss is bytes, not KiB.
    return dict(peak_rss_bytes=usage.ru_maxrss, stderr_bytes=len(stderr),
                exit_code=client.proc.returncode)


def timed_request(client, op, **kwargs):
    started = time.perf_counter()
    result = client.request(op, **kwargs)
    elapsed = (time.perf_counter() - started) * 1000
    assert result["ok"], {key: value for key, value in result.items() if key != "text"}
    return result, elapsed


def generate(client, spec, item):
    result, wall_ms = timed_request(
        client, "generate", messages=spec["prefix"] + [dict(role="user", content=item["user"])],
        max_tokens=item["max_tokens"], temperature=0, no_think=spec["no_think"])
    assert result["cached_tokens"] > 0
    assert result["no_think"] == spec["no_think"]
    # Deliberately retain no text, including no text hash or output excerpts.
    return dict(item_id=item["id"], max_tokens=item["max_tokens"], wall_ms=wall_ms,
                prompt_ms=result["prompt_ms"], gen_ms=result["gen_ms"],
                cached_tokens=result["cached_tokens"],
                evaluated_prompt_tokens=result["evaluated_prompt_tokens"],
                prompt_tokens=result["prompt_tokens"], generated_tokens=result["generated_tokens"],
                reached_eos=int(result["stop_reason"] == "eos"))


def measure(spec):
    model = MODELS / spec["file"]
    assert model.is_file(), "Approved model file is absent; no downloads allowed"
    assert len(spec["items"]) == 10
    client = Client()
    load, load_ms = timed_request(client, "load", model=str(model), ctx=2048)
    warm, warm_ms = timed_request(client, "warm", messages=spec["prefix"])
    samples = []
    for run in range(10):
        for item in spec["items"]:
            sample = generate(client, spec, item)
            sample["run"] = run + 1
            assert sample["cached_tokens"] == warm["cached_tokens"]
            samples.append(sample)
        print(f'{spec["file"]}: completed pass {run + 1}/10', flush=True)
    warm_process = close_with_peak_rss(client)

    # One separate, fresh-process cold case. Filesystem caches are intentionally not purged.
    cold = Client()
    _, cold_load_ms = timed_request(cold, "load", model=str(model), ctx=2048)
    _, cold_warm_ms = timed_request(cold, "warm", messages=spec["prefix"])
    first = generate(cold, spec, spec["items"][0])
    cold_total_ms = (time.perf_counter() - cold.started) * 1000
    cold_process = close_with_peak_rss(cold)

    for sample in samples:
        sample["ms_per_token"] = sample["gen_ms"] / max(sample["generated_tokens"], 1)
    fields = ["wall_ms", "prompt_ms", "gen_ms", "ms_per_token", "cached_tokens", "evaluated_prompt_tokens",
              "prompt_tokens", "generated_tokens"]
    per_item = {}
    for item in spec["items"]:
        rows = [sample for sample in samples if sample["item_id"] == item["id"]]
        per_item[str(item["id"])] = {field: summarize([row[field] for row in rows]) for field in fields}
    return dict(role=spec["role"], no_think=int(spec["no_think"]),
                load_wall_ms=load_ms, load_native_ms=load["load_ms"],
                warm_wall_ms=warm_ms, warm_native_prompt_ms=warm["prompt_ms"],
                cached_prefix_tokens=warm["cached_tokens"],
                eval_reference_prefix_tokens=spec.get("prefix_tokens_in_this_spike"),
                summary={field: summarize([sample[field] for sample in samples]) for field in fields},
                sample_count=len(samples), eos_count=sum(row["reached_eos"] for row in samples),
                warm_process=warm_process,
                cold=dict(spawn_load_warm_first_generate_ms=cold_total_ms,
                          load_wall_ms=cold_load_ms, warm_wall_ms=cold_warm_ms,
                          first_generate=first, process=cold_process),
                peak_rss_bytes=max(warm_process["peak_rss_bytes"], cold_process["peak_rss_bytes"]),
                stderr_bytes=warm_process["stderr_bytes"] + cold_process["stderr_bytes"],
                per_item=per_item, samples=samples)


if __name__ == "__main__":
    signal.signal(signal.SIGALRM, deadline)
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--input", type=Path, required=True, help="Read-only request-set JSON")
    parser.add_argument("--output", type=Path, required=True, help="Numeric-only result JSON")
    parser.add_argument("--label", required=True, help="Build/configuration identifier")
    parser.add_argument("--model", action="append", help="GGUF filename to measure (repeatable)")
    args = parser.parse_args()
    raw = args.input.read_bytes()
    specs = [spec for spec in json.loads(raw)["models"]
             if not args.model or spec["file"] in args.model]
    assert specs, "No requested models in request set"
    if args.model:
        assert set(args.model) == {spec["file"] for spec in specs}, "Requested model not in input"
    for spec in specs:
        assert (MODELS / spec["file"]).is_file(), spec["file"]
    result = dict(label=args.label, input_sha256=hashlib.sha256(raw).hexdigest(), ctx=2048,
                  temperature=0, items_per_model=10, runs_per_item=10,
                  p95_rank_for_100_samples=95, models={})
    for spec in specs:
        result["models"][spec["file"]] = measure(spec)
        args.output.write_text(json.dumps(result, indent=2) + "\n")
        print(json.dumps({spec["file"]: result["models"][spec["file"]]["summary"]}, indent=2), flush=True)
    assert args.input.read_bytes() == raw, "Input set changed during benchmark"
    print(f"Completed {len(specs) * 100} warm and {len(specs)} cold generations. "
          f"Numeric results: {args.output}", flush=True)
