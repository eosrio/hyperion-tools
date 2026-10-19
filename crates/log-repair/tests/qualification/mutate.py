"""Time recovery operations separately on new damaged copies of a frozen block log."""
import json
import os
import pathlib
import shutil
import statistics

from .bench import Corpus
from .common import Pinned, event, publish, require, stamp, sync_dir
from .corpus import copy_prefix
from .process import run


def fingerprint(path):
    pinned = Pinned(path)
    try:
        digest = pinned.digest(pinned.before.st_size, "oracle-hash")
        require(pinned.check(pinned.before.st_size) == stamp(pinned.before), "oracle file changed")
        return {"sha256": digest,
                "stamp": stamp(pinned.before), "path": str(pinned.path)}
    finally:
        pinned.close()


def retire(root, entries, label):
    """Remove only individually verified files created by this invocation."""
    for entry in entries:
        path = pathlib.Path(entry["path"])
        require(path.resolve(strict=True).is_relative_to(root)
                and path.name in ("blocks.log", "blocks.index"), "retirement path escapes test output")
        value = path.stat()
        require(stamp(value) == entry["stamp"] and value.st_nlink == 1,
                "verified retirement file changed or has hard links")
    for entry in entries:
        path = pathlib.Path(entry["path"])
        path.unlink()
        sync_dir(path.parent)
    publish(root / ("retired-" + label + ".json"), {"data_files_retired": entries})


def mutations(args):
    require(1 <= args.repeats <= 100, "select 1..100 repeats")
    require(0 < args.timeout <= 86400, "invalid operation timeout")
    root = args.out.resolve()
    require(not root.is_relative_to(args.corpus.resolve()), "output must be outside corpus")
    root.mkdir(mode=0o700)
    corpus, tool = None, Pinned(args.tool)
    results = []
    try:
        corpus = Corpus(args.corpus, {"blocks"})
        pair = corpus.pairs["blocks"]
        metadata = pair["manifest"]
        require(metadata["last_block"] - metadata["first_block"] >= 2,
                "recovery benchmark needs at least three complete entries")
        keep = metadata["last_position"]
        require(1 <= metadata["log_bytes"] - 4 - keep + 8 <= 256 << 20,
                "selected final entry exceeds in-place suffix limit")
        total = metadata["log_bytes"] + metadata["index_bytes"]
        copies = 2 if args.retire_data else 3 * args.repeats
        require(shutil.disk_usage(root).free >= total * copies + max(1 << 30, total // 8),
                "insufficient capacity for independent recovery copies and margin")
        tool_hash = tool.digest(tool.before.st_size)
        expected = {
            "salvage": (pair["log"].digest(keep), pair["index"].digest(metadata["index_bytes"] - 8)),
            "restore": (metadata["log"]["sha256"], metadata["index"]["sha256"]),
        }
        publish(root / "plan.json", {"tool_sha256": tool_hash, "corpus": str(corpus.root),
                                     "corpus_log_sha256": expected["restore"][0],
                                     "corpus_index_sha256": expected["restore"][1],
                                     "repeats": args.repeats, "retire_data": args.retire_data,
                                     "mutation": "Remove the final four trailer bytes; retain the original index.",
                                     "cache": "Ordinary shared-host cache; copy/hash preparation is outside timed operations.",
                                     "phase_timing": "Polling observations, not exact internal boundaries."})
        for repeat in range(args.repeats):
            directory = root / str(repeat + 1)
            directory.mkdir(mode=0o700)
            log, index = directory / "blocks.log", directory / "blocks.index"
            log_copy = copy_prefix(pair["log"], log, metadata["log_bytes"] - 4)
            index_copy = copy_prefix(pair["index"], index, metadata["index_bytes"])
            for path in (log, index):
                path.chmod(0o600)
            publish(directory / "inputs.json", {"log": log_copy, "index": index_copy})
            damaged_hashes = (log_copy["sha256"], index_copy["sha256"])

            def measure(name, argv):
                corpus.check()
                before = [stamp(p.stat()) for p in (log, index)]
                value = run([tool.path, "--json", *argv], directory / (name + "-run"),
                            {"tool_sha256": tool_hash, "repeat": repeat + 1,
                             "operation": name}, interval=0.05, timeout=args.timeout)
                require(value["returncode"] == 0 and not value["timed_out"],
                        f"{name} failed; retain every file and journal")
                report = json.loads((directory / (name + "-run") / "stdout.json").read_text())
                require(report.get("ok") is True, "tool did not report success")
                after = [stamp(p.stat()) for p in (log, index)]
                require(all((a["device"], a["inode"]) == (b["device"], b["inode"])
                            for a, b in zip(before, after)), "operation replaced an input")
                if name in ("salvage", "restore", "resume"):
                    require(before == after, "read-only/no-op operation changed its inputs")
                corpus.check()
                require(tool.check(tool.before.st_size) == stamp(tool.before), "binary changed")
                results.append({"operation": name, "repeat": repeat + 1, **value})
                return report

            def check_pair(paths, hashes, label):
                entries = [fingerprint(p) for p in paths]
                require(tuple(p["sha256"] for p in entries) == hashes,
                        f"{label} differs from independent source-byte oracle")
                publish(directory / (label + "-oracle.json"), {"ok": True, "files": entries})
                return entries

            journal = directory / "journal"
            measure("trim", ["trim-tail", "--log", log, "--journal", journal, "--node-stopped"])
            check_pair((log, index), expected["salvage"], "trim")
            resume = ["resume-tail-trim", "--journal", journal, "--node-stopped"]
            measure("resume", resume)
            check_pair((log, index), expected["salvage"], "resume")
            measure("rollback", [*resume, "--rollback"])
            check_pair((log, index), damaged_hashes, "rollback")
            for mode in ("salvage", "restore"):
                output = directory / (mode + "-output")
                argv = ["recover-tail", "--log", log, "--mode", mode, "--out", output,
                        "--node-stopped"]
                if mode == "restore":
                    argv.extend(["--blocks-log", pair["log"].path,
                                 "--through-block", str(metadata["last_block"])])
                measure(mode, argv)
                outputs = check_pair((output / "recovered/blocks.log",
                                      output / "recovered/blocks.index"), expected[mode], mode)
                check_pair((log, index), damaged_hashes, mode + "-input")
                if args.retire_data:
                    retire(directory, outputs, mode)
            inputs = check_pair((log, index), damaged_hashes, "final-input")
            if args.retire_data:
                retire(directory, inputs, "input")
        groups = []
        for operation in ("trim", "resume", "rollback", "salvage", "restore"):
            times = [r["elapsed_seconds"] for r in results if r["operation"] == operation]
            groups.append({"operation": operation, "runs": len(times),
                           "median_seconds": statistics.median(times),
                           "min_seconds": min(times), "max_seconds": max(times)})
        publish(root / "complete.json", {"ok": True, "groups": groups, "results": results,
                                         "tool_sha256": tool_hash, "retired_data": args.retire_data})
        event("mutation-benchmark-complete", groups=groups)
    finally:
        tool.close()
        if corpus is not None:
            corpus.close()


def add_parser(subparsers):
    parser = subparsers.add_parser("mutate", help="Time trim/resume/rollback/salvage/restore on NEW copies")
    parser.add_argument("--tool", type=pathlib.Path, required=True)
    parser.add_argument("--corpus", type=pathlib.Path, required=True)
    parser.add_argument("--out", type=pathlib.Path, required=True)
    parser.add_argument("--repeats", type=int, default=3)
    parser.add_argument("--timeout", type=float, default=21600)
    parser.add_argument("--retire-data", action="store_true",
                        help="After independent verification, remove only this run's log/index copies; keep evidence/journals")
    parser.set_defaults(run=mutations)
