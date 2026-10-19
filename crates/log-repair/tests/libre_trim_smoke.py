#!/usr/bin/env python3
"""Qualify trim/resume/rollback on a DISPOSABLE frozen Libre log copy.

Destructively reduces the earlier 269898278-block benchmark copy to the exact
power-loss incident lengths. Never pass live files. Python stdlib, Linux only.
Simulates a persisted one-file trim state; does not simulate an actual power cut.
"""
import argparse
import hashlib
import json
import os
import pathlib
import resource
import subprocess
import time

FULL_SIZES = (56409713202, 2159186224)
DAMAGED_SIZES = (48477356032, 1853814976)
KEPT_SIZES = (48477355705, 1853814416)
DAMAGED_HASHES = (
    "b5e68e7aaac92ab15863a00dab57a2afdc4ad0367262cd6b35ba8155691c094a",
    "af4a7a66594d5c49039589dc03590d92b3f6fe94b0551291f5f16c99ebd1e9a7",
)


def require(condition, detail):
    if not condition:
        raise RuntimeError(detail)


def digest(path):
    h = hashlib.sha256()
    with path.open("rb") as f:
        while chunk := f.read(16 << 20):
            h.update(chunk)
    return h.hexdigest()


def identity(path):
    s = path.stat()
    return (s.st_dev, s.st_ino)


def sync_dir(path):
    fd = os.open(path, os.O_RDONLY | os.O_DIRECTORY)
    try:
        os.fsync(fd)
    finally:
        os.close(fd)


def record(path, value):
    with path.open("x") as f:
        json.dump(value, f, indent=2)
        f.flush()
        os.fsync(f.fileno())
    sync_dir(path.parent)


def sample_source(paths):
    # Historical offsets only: the live source is allowed to keep appending.
    result = []
    for p, end in zip(paths, KEPT_SIZES):
        with p.open("rb") as f:
            first = f.read(64)
            f.seek(end - 64)
            last = f.read(64)
        result.append([identity(p), first.hex(), last.hex()])
    return result


def run(tool, out, name, *args):
    argv = [str(tool), "--json", *map(str, args)]
    before = resource.getrusage(resource.RUSAGE_CHILDREN)
    started = time.monotonic()
    print(name, "started", flush=True)
    with (out / f"{name}.json").open("x") as stdout, \
            (out / f"{name}.stderr").open("x") as stderr, \
            (out / f"{name}.samples.jsonl").open("x") as samples:
        process = subprocess.Popen(argv, stdout=stdout, stderr=stderr)
        while process.poll() is None:
            sample = {"seconds": time.monotonic() - started}
            try:
                sample["io"] = {
                    k: int(v.strip()) for k, v in (
                        line.split(":", 1) for line in
                        pathlib.Path(f"/proc/{process.pid}/io").read_text().splitlines()
                    )
                }
            except (FileNotFoundError, ProcessLookupError):
                pass
            samples.write(json.dumps(sample) + "\n")
            samples.flush()
            try:
                process.wait(timeout=1)
            except subprocess.TimeoutExpired:
                if time.monotonic() - started > 1800:
                    process.kill()
                    process.wait()
                    raise RuntimeError(f"{name} exceeded 30 minutes; keep journal")
    after = resource.getrusage(resource.RUSAGE_CHILDREN)
    result = {
        "argv": argv, "returncode": process.returncode,
        "elapsed_seconds": time.monotonic() - started,
        "user_seconds": after.ru_utime - before.ru_utime,
        "system_seconds": after.ru_stime - before.ru_stime,
    }
    record(out / f"{name}.metrics.json", result)
    require(process.returncode == 0, (name, result))
    report = json.loads((out / f"{name}.json").read_text())
    require(report["ok"], 'tool did not report success')
    print(name, "finished", result, flush=True)
    return result, report


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--tool", required=True, type=pathlib.Path)
    parser.add_argument("--fixture", required=True, type=pathlib.Path)
    parser.add_argument("--source", required=True, type=pathlib.Path)
    parser.add_argument("--out", required=True, type=pathlib.Path)
    parser.add_argument("--discard-copy-history", action="store_true", required=True)
    args = parser.parse_args()
    tool, fixture, source = args.tool.resolve(), args.fixture.resolve(), args.source.resolve()
    require(fixture.is_relative_to(pathlib.Path("/tmp")) and fixture != source, 'fixture must be a separate disposable copy under /tmp')
    paths = [fixture / name for name in ("blocks.log", "blocks.index")]
    sources = [source / p.name for p in paths]
    require(tuple(p.stat().st_size for p in paths) == FULL_SIZES, 'fixture does not match the original benchmark sizes')
    for p, src in zip(paths, sources):
        require(p.resolve().parent == fixture and p.stat().st_nlink == 1, 'fixture file escapes its directory or has hard links')
        require(not os.path.samefile(p, src), 'fixture aliases live source')
    ids = [identity(p) for p in paths]
    live_before = sample_source(sources)
    args.out.mkdir()
    out = args.out.resolve()
    journal = out / "journal"
    record(out / "run.json", {
        "fixture": str(fixture), "source": str(source),
        "tool_sha256": digest(tool), "before_identities": ids,
        "full_snapshot_sizes": FULL_SIZES, "incident_sizes": DAMAGED_SIZES,
        "interruption": "manually constructed log-trimmed/index-original state; no power cut",
    })
    for p, size in zip(paths, DAMAGED_SIZES):
        with p.open("r+b") as f:
            f.truncate(size)
            f.flush()
            os.fsync(f.fileno())
    original_hashes = tuple(digest(p) for p in paths)
    require(original_hashes == DAMAGED_HASHES, original_hashes)
    record(out / "incident-copy.json", {"hashes": original_hashes, "sizes": DAMAGED_SIZES})
    trim, report = run(tool, out, "trim", "trim-tail", "--log", paths[0],
                       "--journal", journal, "--node-stopped")
    require(report["kept_blocks"] == [1, 231726802], 'unexpected retained block range')
    require(report["log_bytes_removed"] == 327 and report["index_bytes_removed"] == 560, 'unexpected removed byte counts')
    require(tuple(p.stat().st_size for p in paths) == KEPT_SIZES, 'trimmed file sizes differ')
    require([identity(p) for p in paths] == ids, 'fixture identities changed')
    intent = json.loads((journal / "intent.json").read_text())
    prefix_hashes = tuple(digest(p) for p in paths)
    require(prefix_hashes == (intent["log"]["prefix_sha256"], intent["index"]["prefix_sha256"]), 'trimmed hashes differ from intent')
    tails = [(journal / name).read_bytes() for name in ("log.tail", "index.tail")]
    require(tuple(map(len, tails)) == (327, 560), 'saved suffix lengths differ')
    require(tuple(hashlib.sha256(t).hexdigest() for t in tails) == (
        intent["log"]["tail_sha256"], intent["index"]["tail_sha256"]), 'saved suffix hashes differ')
    # Reconstruct the durable state after log fsync, before index truncation.
    with paths[1].open("ab") as f:
        f.write(tails[1])
        f.flush()
        os.fsync(f.fileno())
    (journal / "receipt.json").rename(out / "first-trim-receipt.json")
    sync_dir(journal)
    sync_dir(out)
    resume, _ = run(tool, out, "resume", "resume-tail-trim", "--journal", journal, "--node-stopped")
    require(tuple(p.stat().st_size for p in paths) == KEPT_SIZES, 'trimmed file sizes differ')
    require(tuple(digest(p) for p in paths) == prefix_hashes, 'resumed prefix hashes differ')
    rollback, _ = run(tool, out, "rollback", "resume-tail-trim", "--journal", journal,
                      "--rollback", "--node-stopped")
    require(tuple(p.stat().st_size for p in paths) == DAMAGED_SIZES, 'rollback sizes differ from originals')
    require(tuple(digest(p) for p in paths) == DAMAGED_HASHES, 'rollback hashes differ from originals')
    require([identity(p) for p in paths] == ids, 'fixture identities changed')
    require(sample_source(sources) == live_before, 'live historical samples or identities changed')
    record(out / "complete.json", {
        "ok": True, "trim": trim, "resume": resume, "rollback": rollback,
        "tool_sha256": digest(tool), "kept_blocks": [1, 231726802],
        "saved_bytes": [327, 560], "trimmed_hashes": prefix_hashes,
        "rollback_hashes": DAMAGED_HASHES, "file_identities_preserved": True,
        "live_historical_samples_unchanged": True,
        "interruption": "simulated durable boundary, not actual power loss",
    })
    print("complete", out, flush=True)


if __name__ == "__main__":
    main()
