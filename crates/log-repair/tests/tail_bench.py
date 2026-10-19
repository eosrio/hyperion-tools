#!/usr/bin/env python3
"""Measure read-only recover-tail inspection on a stable Linux log/index pair.

Run on a retained log or an independent, frozen copy. Never stop a live node or
evict the host's cache for this benchmark. Label cache conditions explicitly.
Python stdlib only; output directories are new and never reused.
"""
import argparse
import hashlib
import json
import pathlib
import resource
import subprocess
import time


def stamp(path):
    value = path.stat()
    return [value.st_dev, value.st_ino, value.st_size, value.st_mtime_ns]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--tool", required=True, type=pathlib.Path)
    parser.add_argument("--log", required=True, type=pathlib.Path)
    parser.add_argument("--out", required=True, type=pathlib.Path)
    parser.add_argument("--cache-note", required=True)
    parser.add_argument("--timeout", type=float, default=1200)
    args = parser.parse_args()
    tool, log = args.tool.resolve(), args.log.resolve()
    index = log.with_suffix(".index")
    before = {str(p): stamp(p) for p in (log, index)}
    args.out.mkdir()
    out = args.out.resolve()
    argv = [str(tool), "--json", "recover-tail", "--log", str(log)]
    metadata = {
        "argv": argv,
        "tool_sha256": hashlib.sha256(tool.read_bytes()).hexdigest(),
        "input_stamps": before,
        "cache_note": args.cache_note,
    }
    (out / "run.json").write_text(json.dumps(metadata, indent=2))
    usage = resource.getrusage(resource.RUSAGE_CHILDREN)
    started = time.monotonic()
    timed_out = False
    with (out / "report.json").open("x") as output, \
            (out / "stderr.txt").open("x") as error, \
            (out / "samples.jsonl").open("x") as samples:
        process = subprocess.Popen(argv, stdout=output, stderr=error)
        while process.poll() is None:
            sample = {"seconds": time.monotonic() - started}
            try:
                sample["io"] = {
                    key: int(value.strip())
                    for key, value in (
                        line.split(":", 1)
                        for line in pathlib.Path(f"/proc/{process.pid}/io").read_text().splitlines()
                    )
                }
            except (FileNotFoundError, PermissionError, ProcessLookupError):
                pass
            samples.write(json.dumps(sample) + "\n")
            samples.flush()
            try:
                process.wait(timeout=1)
            except subprocess.TimeoutExpired:
                if time.monotonic() - started > args.timeout:
                    timed_out = True
                    process.kill()
                    process.wait()
        elapsed = time.monotonic() - started
    used = resource.getrusage(resource.RUSAGE_CHILDREN)
    result = {
        **metadata,
        "returncode": process.returncode,
        "timed_out": timed_out,
        "elapsed_seconds": elapsed,
        "user_seconds": used.ru_utime - usage.ru_utime,
        "system_seconds": used.ru_stime - usage.ru_stime,
        "max_rss_kib": used.ru_maxrss,
        "inputs_unchanged": before == {str(p): stamp(p) for p in (log, index)},
    }
    if process.returncode == 0:
        report = json.loads((out / "report.json").read_text())
        result.update({
            "log_bytes": before[str(log)][2],
            "entries": report["prefix"]["entries"],
            "log_mb_per_second": before[str(log)][2] / elapsed / 1e6,
            "entries_per_second": report["prefix"]["entries"] / elapsed,
            "inspection": report["inspection"],
        })
    (out / "result.json").write_text(json.dumps(result, indent=2))
    print(json.dumps(result, indent=2))
    if process.returncode or timed_out or not result["inputs_unchanged"]:
        raise SystemExit(1)


if __name__ == "__main__":
    main()
