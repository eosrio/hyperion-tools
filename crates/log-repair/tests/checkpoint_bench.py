#!/usr/bin/env python3
"""Compare ordinary trim with saved-inspection trim on a NEW independent copy.

The source must be frozen and already contain a small damaged tail or surplus
index slots. Only files below --out are modified. No cache eviction or node stops.
Python stdlib, Linux /usr/bin/time, and an already built repair executable required.
"""
import argparse
import hashlib
import json
import pathlib
import shutil
import subprocess


def stamp(path):
    s = path.stat()
    return [s.st_dev, s.st_ino, s.st_size, s.st_mtime_ns, s.st_ctime_ns]


def require(condition, message):
    if not condition:
        raise RuntimeError(message)


def digest(path):
    h = hashlib.sha256()
    with path.open("rb") as f:
        for b in iter(lambda: f.read(16 << 20), b""):
            h.update(b)
    return h.hexdigest()


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--tool", required=True, type=pathlib.Path)
    p.add_argument("--source-dir", required=True, type=pathlib.Path)
    p.add_argument("--out", required=True, type=pathlib.Path)
    p.add_argument("--cache-note", required=True)
    p.add_argument("--native-util", type=pathlib.Path)
    p.add_argument("--timeout", type=float, default=1200)
    a = p.parse_args()
    tool, source = a.tool.resolve(), a.source_dir.resolve()
    inputs = [source / n for n in ("blocks.log", "blocks.index")]
    before = {str(f): stamp(f) for f in inputs}
    required = sum(f.stat().st_size for f in inputs) + (512 << 20)
    require(shutil.disk_usage(a.out.parent).free >= required, "insufficient copy space")
    a.out.mkdir(mode=0o700)
    out = a.out.resolve()
    blocks = out / "blocks"
    blocks.mkdir(mode=0o700)
    for f in inputs:
        shutil.copyfile(f, blocks / f.name)
        require(stamp(f)[:2] != stamp(blocks / f.name)[:2], "copy aliases the source")
    result = {"tool_sha256": digest(tool), "source_stamps": before,
              "cache_note": a.cache_note, "runs": {}, "source_unchanged": False}

    def run(name, *args):
        print(name, flush=True)
        command = [str(tool), "--json", *map(str, args)]
        timing = out / (name + ".time.json")
        with (out / (name + ".json")).open("x") as stdout, \
                (out / (name + ".stderr.txt")).open("x") as stderr:
            completed = subprocess.run([
                "/usr/bin/time", "-o", str(timing), "-f",
                '{"seconds":%e,"user_seconds":%U,"system_seconds":%S,'
                '"max_rss_kib":%M,"filesystem_inputs":%I,"filesystem_outputs":%O}',
                *command], stdout=stdout, stderr=stderr, timeout=a.timeout)
        require(completed.returncode == 0, name + " failed; inspect stderr and preserve journals")
        report = json.loads((out / (name + ".json")).read_text())
        progress = (out / (name + ".stderr.txt")).read_text()
        phases = sorted(set(line.split("]: ")[0] + "]" for line in progress.splitlines()
                            if line.startswith("recover-tail [")))
        result["runs"][name] = {"resources": json.loads(timing.read_text()),
                                "phases": phases, "report": report}
        (out / "progress.json").write_text(json.dumps(result, indent=2) + "\n")
        return report

    log = blocks / "blocks.log"
    baseline = out / "baseline-journal"
    run("ordinary_trim", "trim-tail", "--log", log, "--journal", baseline, "--node-stopped")
    run("ordinary_rollback", "resume-tail-trim", "--journal", baseline, "--rollback", "--node-stopped")
    saved = out / "inspection"
    inspection = run("save_inspection", "recover-tail", "--log", log,
                     "--save-inspection", saved, "--node-stopped")
    journal = out / "checkpoint-journal"
    trimmed = run("checkpoint_trim", "trim-tail", "--log", log, "--inspection", saved,
                  "--journal", journal, "--node-stopped")
    require(trimmed["inspection_reused"] is True, "inspection was not reused")
    require(trimmed["kept_blocks"][1] == inspection["prefix"]["last_block"], "wrong retained head")
    require(len(result["runs"]["ordinary_trim"]["phases"]) == 2, "ordinary phase count")
    require(len(result["runs"]["checkpoint_trim"]["phases"]) == 1, "checkpoint phase count")
    if a.native_util:
        with (out / "native-smoke.txt").open("x") as f:
            subprocess.run([str(a.native_util), "block-log", "smoke-test", "--blocks-dir",
                            str(blocks)], stdout=f, stderr=subprocess.STDOUT,
                           check=True, timeout=a.timeout)
    run("checkpoint_rollback", "resume-tail-trim", "--journal", journal, "--rollback", "--node-stopped")
    actual = {f.name: digest(blocks / f.name) for f in inputs}
    source_hashes = {f.name: digest(f) for f in inputs}
    require(actual == source_hashes, "rollback differs from frozen source")
    require({str(f): stamp(f) for f in inputs} == before, "source changed during qualification")
    result.update(source_unchanged=True, restored_sha256=actual,
                  ordinary_trim_seconds=result["runs"]["ordinary_trim"]["resources"]["seconds"],
                  checkpoint_trim_seconds=result["runs"]["checkpoint_trim"]["resources"]["seconds"])
    (out / "complete.json").write_text(json.dumps(result, indent=2) + "\n")
    print(json.dumps({k: result[k] for k in ("source_unchanged", "ordinary_trim_seconds",
                                           "checkpoint_trim_seconds")}), flush=True)


if __name__ == "__main__":
    main()
