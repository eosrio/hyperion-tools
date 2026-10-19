"""Repeated, source-pinned command benchmarks with independent result oracles."""
import json
import pathlib
import platform
import statistics
import struct

from . import cache
from .common import Pinned, event, publish, require, stamp
from .process import run


class Corpus:
    def __init__(self, path, kinds=None, verify_hashes=True):
        self.root = path.resolve(strict=True)
        ready = self.root / "ready.json"
        require(ready.is_file() and ready.stat().st_size <= 1 << 20,
                "a complete bounded snapshot manifest is required")
        self.manifest = json.loads(ready.read_text())
        require(self.manifest.get("ok") is True and self.manifest.get("schema") == 1,
                "snapshot is incomplete or unsupported")
        self.pairs, self.pins = {}, []
        try:
            for value in self.manifest["pairs"]:
                kind = value["kind"]
                require(kind in ("blocks", "trace_history", "chain_state_history")
                        and kind not in self.pairs, "unknown or duplicate corpus kind")
                if kinds is not None and kind not in kinds:
                    continue
                pair = {"manifest": value}
                for name, suffix in (("log", ".log"), ("index", ".index")):
                    entry = value[name]
                    folder = "blocks" if kind == "blocks" else "state-history"
                    require(entry["path"] == f"{folder}/{kind}{suffix}", "unexpected corpus path")
                    source = Pinned(self.root / entry["path"])
                    self.pins.append(source)
                    require(source.path.is_relative_to(self.root), "corpus file escapes root")
                    require(not self.root.is_relative_to(pathlib.Path(entry["source"]).parent),
                            "corpus must be separate from live sources")
                    require([source.before.st_dev, source.before.st_ino] == entry["identity"]
                            and source.before.st_size == entry["bytes"]
                            and source.before.st_nlink == 1, "corpus identity/size/hard links changed")
                    require(entry["sha256"] == entry["source_prefix_sha256"], "snapshot oracle differs")
                    if verify_hashes:
                        require(source.digest(entry["bytes"], "validate-corpus") == entry["sha256"],
                                "corpus fingerprint mismatch")
                    pair[name] = source
                self.pairs[kind] = pair
            if kinds is not None:
                require(set(kinds) <= self.pairs.keys(), "requested kind is absent from corpus")
        except BaseException:
            self.close()
            raise

    def check(self):
        for source in self.pins:
            require(source.check(source.before.st_size) == stamp(source.before),
                    f"immutable corpus changed: {source.path}")

    def close(self):
        for source in self.pins:
            source.close()


def oracle(command, value, pair, block_range=None, expected_bytes=None):
    require(value.get("ok") is True, "tool did not report success")
    expected = pair["manifest"]
    if command == "scan":
        require(value["damaged"] is False and value["regions"] == [], "clean scan found damage")
    elif command == "verify":
        require(value["corrupt"] is False, "clean verify reported corruption")
        first, last = block_range or (expected["first_block"], expected["last_block"])
        require(value["blocks"] == [first, last] and value["entries"] == last - first + 1
                and value["skipped_entries"] == 0 and value["truncated"] is False,
                "verify did not cover every declared entry")
        if expected_bytes is not None:
            require(value["bytes"] == expected_bytes, "verify byte coverage differs from indexed span")
    else:
        inspection = value["inspection"]
        require(value["prefix"]["last_block"] == expected["last_block"]
                and inspection["entries"] == expected["last_block"] - expected["first_block"] + 1,
                "inspection did not validate the declared complete range")
        require(inspection["failure"] is None and inspection["index_discrepancies"] == 0,
                "inspection stopped or found index discrepancies")
        require(inspection["input_sha256"] == expected["log"]["sha256"]
                and inspection["index_sha256"] == expected["index"]["sha256"],
                "inspection fingerprints differ from independent corpus oracle")


def benchmark(args):
    require(1 <= args.repeats <= 100, "select 1..100 repeats")
    require(0 < args.timeout <= 86400 and 0.05 <= args.sample_interval <= 5,
            "invalid timeout or sampling interval")
    require(len(args.kinds) == len(set(args.kinds)) and len(args.commands) == len(set(args.commands)),
            "duplicate kinds or commands would reuse evidence paths")
    block_range = None
    if args.block_range:
        parts = args.block_range.split("-")
        require(len(parts) == 2 and all(p.isdecimal() for p in parts), "range must be FIRST-LAST")
        block_range = tuple(map(int, parts))
        require(0 < block_range[0] <= block_range[1] < 1 << 32 and args.commands == ["verify"],
                "explicit ranges currently require --commands verify")
    root = args.out.resolve()
    root.mkdir(mode=0o700)
    # SHIP inspection uses canonical blocks, even when only SHIP is timed.
    kinds = set(args.kinds)
    if "inspect" in args.commands and kinds - {"blocks"}:
        kinds.add("blocks")
    corpus, tool = None, Pinned(args.tool)
    results = []
    try:
        tool_hash = tool.digest(tool.before.st_size, "hash-tool")
        corpus = Corpus(args.corpus, kinds)
        publish(root / "plan.json", {"schema": 1, "source_revision": args.source_revision,
                                     "tool_sha256": tool_hash, "tool": str(tool.path),
                                     "corpus": str(corpus.root), "kinds": args.kinds,
                                     "commands": args.commands, "repeats": args.repeats,
                                     "block_range": block_range,
                                     "cache": args.cache, "host": platform.uname()._asdict(),
                                     "profile": args.profile})
        for kind in args.kinds:
            pair = corpus.pairs[kind]
            if block_range:
                require(pair["manifest"]["first_block"] <= block_range[0] <= block_range[1]
                        <= pair["manifest"]["last_block"], "requested range exceeds corpus coverage")
            expected_bytes = None
            if "verify" in args.commands:
                first, last = block_range or (pair["manifest"]["first_block"],
                                              pair["manifest"]["last_block"])
                slot = first - pair["manifest"]["first_block"]
                start = struct.unpack("<Q", pair["index"].read(slot * 8, 8))[0]
                end = pair["log"].before.st_size
                if last < pair["manifest"]["last_block"]:
                    slot = last + 1 - pair["manifest"]["first_block"]
                    end = struct.unpack("<Q", pair["index"].read(slot * 8, 8))[0]
                require(0 <= start < end <= pair["log"].before.st_size,
                        "requested range has an invalid indexed span")
                expected_bytes = end - start
            for command in args.commands:
                for repeat in range(args.repeats):
                    corpus.check()
                    name = f"{kind}-{command}-{repeat + 1}"
                    argv = [str(tool.path), "--json",
                            "recover-tail" if command == "inspect" else command,
                            "--log", str(pair["log"].path)]
                    if block_range:
                        argv.extend(["--block-range", args.block_range])
                    pins = [pair["log"], pair["index"]]
                    if command == "inspect" and kind != "blocks":
                        reference = corpus.pairs["blocks"]
                        argv.extend(["--blocks-log", str(reference["log"].path)])
                        pins.extend([reference["log"], reference["index"]])
                    cache_state = cache.prepare(pins, args.cache)
                    if args.profile == "strace":
                        argv = ["strace", "-f", "-c", "-o", str(root / name / "strace.txt"), *argv]
                    elif args.profile == "perf":
                        argv = [str(args.perf), "record", "--no-buildid-cache", "-q", "-e", "cpu-clock:u", "-F", "99",
                                "--call-graph", "dwarf,8192", "-o", str(root / name / "perf.data"),
                                "--", *argv]
                    result = run(argv, root / name, {"kind": kind, "command": command,
                                 "repeat": repeat + 1, "tool_sha256": tool_hash,
                                 "cache_state": cache_state}, timeout=args.timeout,
                                 interval=args.sample_interval)
                    require(result["returncode"] == 0 and not result["timed_out"],
                            f"benchmark command failed: {name}; retain all evidence")
                    report = json.loads((root / name / "stdout.json").read_text())
                    oracle(command, report, pair, block_range, expected_bytes)
                    corpus.check()
                    require(tool.check(tool.before.st_size) == stamp(tool.before), "binary changed")
                    publish(root / name / "oracle.json", {"ok": True, "corpus_unchanged": True,
                                                         "tool_unchanged": True})
                    results.append({"kind": kind, "command": command, "repeat": repeat + 1,
                                    "corpus_log_bytes": pair["manifest"]["log_bytes"],
                                    "measured_bytes": report["bytes"] if command == "verify"
                                    else pair["manifest"]["log_bytes"],
                                    "tool_reported_seconds": report.get("seconds"), **result})
        groups = []
        for kind in args.kinds:
            for command in args.commands:
                values = [r["elapsed_seconds"] for r in results
                          if r["kind"] == kind and r["command"] == command]
                groups.append({"kind": kind, "command": command, "runs": len(values),
                               "median_seconds": statistics.median(values),
                               "min_seconds": min(values), "max_seconds": max(values),
                               "median_mib_per_second": statistics.median(
                                   r["measured_bytes"] / (1 << 20) / r["elapsed_seconds"]
                                   for r in results if r["kind"] == kind and r["command"] == command)})
        publish(root / "complete.json", {"schema": 1, "ok": True, "tool_sha256": tool_hash,
                                         "results": results, "groups": groups,
                                         "profile": args.profile, "cache": args.cache})
        event("benchmark-complete", path=str(root), groups=groups)
    finally:
        tool.close()
        if corpus is not None:
            corpus.close()


def add_parser(subparsers):
    parser = subparsers.add_parser("benchmark", help="Repeated read-only command baselines")
    parser.add_argument("--tool", type=pathlib.Path, required=True)
    parser.add_argument("--corpus", type=pathlib.Path, required=True)
    parser.add_argument("--out", type=pathlib.Path, required=True)
    parser.add_argument("--source-revision", required=True)
    parser.add_argument("--kinds", nargs="+", choices=("blocks", "trace_history", "chain_state_history"),
                        default=["blocks"])
    parser.add_argument("--commands", nargs="+", choices=("scan", "verify", "inspect"),
                        default=["scan", "verify", "inspect"])
    parser.add_argument("--repeats", type=int, default=3)
    parser.add_argument("--cache", choices=("ordinary", "file-dontneed"), default="ordinary")
    parser.add_argument("--profile", choices=("none", "strace", "perf"), default="none")
    parser.add_argument("--perf", type=pathlib.Path, default=pathlib.Path("perf"))
    parser.add_argument("--timeout", type=float, default=3600)
    parser.add_argument("--sample-interval", type=float, default=0.05,
                        help="Linux sample/poll interval in seconds (0.05..5)")
    parser.add_argument("--block-range", help="FIRST-LAST; only with --commands verify")
    parser.set_defaults(run=benchmark)
