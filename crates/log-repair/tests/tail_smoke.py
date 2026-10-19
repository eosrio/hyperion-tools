#!/usr/bin/env python3
"""Recover torn copies of a disposable nodeos_smoke.py chain, then replay the result.

Python stdlib only. Run nodeos_smoke.py first. This refuses an existing output
directory and never changes its source fixture. It simulates torn files, not a
power failure or storage write reordering.
"""
import argparse
import hashlib
import json
import pathlib
import shutil
import socket
import struct
import subprocess


def sha(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--tool", type=pathlib.Path, required=True)
    p.add_argument("--source-smoke", type=pathlib.Path, required=True)
    p.add_argument("--nodeos", type=pathlib.Path, default=pathlib.Path("/usr/bin/nodeos"))
    p.add_argument("--out", type=pathlib.Path, required=True)
    args = p.parse_args()
    source = args.source_smoke.resolve()
    assert (source / "snapshot.json").is_file() and (source / "produce.log").is_file(), \
        "source must be an isolated nodeos_smoke.py fixture"
    args.out.mkdir()
    root = args.out.resolve()
    tool, nodeos = str(args.tool.resolve()), str(args.nodeos.resolve())
    source_blocks = source / "source/blocks/blocks.log"
    block_bytes = source_blocks.read_bytes()
    first = struct.unpack_from("<I", block_bytes, 4)[0]
    last = first + source_blocks.with_suffix(".index").stat().st_size // 8 - 1
    original_files = [source_blocks, source_blocks.with_suffix(".index")]
    for kind in ("trace_history", "chain_state_history"):
        log = source / f"source/state-history/{kind}.log"
        original_files.extend([log, log.with_suffix(".index")])
    original_hashes = {str(p): sha(p) for p in original_files}
    stages = {}

    def run(argv, name):
        cp = subprocess.run([tool, "--json", *map(str, argv)], text=True,
                            capture_output=True, timeout=120)
        (root / f"{name}.json").write_text(cp.stdout)
        (root / f"{name}.stderr").write_text(cp.stderr)
        assert cp.returncode == 0, (name, cp.stdout, cp.stderr)
        return json.loads(cp.stdout)

    for kind in ("blocks", "trace_history", "chain_state_history"):
        src = source_blocks if kind == "blocks" else source / f"source/state-history/{kind}.log"
        raw, idx = src.read_bytes(), src.with_suffix(".index").read_bytes()
        begin = first if kind == "blocks" else int.from_bytes(raw[8:12], "big")
        count = last - begin + 1
        idx = idx[:count * 8]
        at = struct.unpack_from("<Q", idx, (count - 1) * 8)[0]
        end = len(raw) if kind == "blocks" else at + 56 + struct.unpack_from("<Q", raw, at + 40)[0]
        raw = raw[:end]  # only the independently known irreversible range
        donor_dir = root / f"donor-{kind}"
        donor_dir.mkdir()
        donor = donor_dir / f"{kind}.log"
        donor.write_bytes(raw)
        donor.with_suffix(".index").write_bytes(idx)
        damaged_dir = root / f"damaged-{kind}"
        damaged_dir.mkdir()
        damaged = damaged_dir / f"{kind}.log"
        damaged.write_bytes(raw[:at + 20])
        damaged.with_suffix(".index").write_bytes(idx[:-3])  # independently torn index
        before = (sha(damaged), sha(damaged.with_suffix(".index")))
        sources = ["--blocks-log", source_blocks]
        if kind != "blocks":
            sources.extend(["--donor-log", donor])
        common = ["recover-tail", "--log", damaged, "--through-block", last, *sources]
        inspected = run(common, f"{kind}-inspect")
        assert inspected["prefix"]["last_block"] == last - 1
        assert all(inspected["options"][m]["available"] for m in ("restore", "salvage"))
        restored_dir = root / f"restored-{kind}"
        restored = run([*common, "--out", restored_dir, "--node-stopped"], f"{kind}-restore")
        assert restored["output"]["last_block"] == last
        assert (restored_dir / "complete.json").is_file()
        recovered = restored_dir / f"recovered/{kind}.log"
        checked = run(["verify", "--log", recovered, "--against", source_blocks], f"{kind}-verify")
        assert checked["corrupt"] is False
        salvaged_dir = root / f"salvaged-{kind}"
        salvaged = run([*common, "--mode", "salvage", "--out", salvaged_dir, "--node-stopped"], f"{kind}-salvage")
        assert salvaged["output"]["last_block"] == last - 1
        assert before == (sha(damaged), sha(damaged.with_suffix(".index")))
        stages[kind] = recovered

    # Let a real nodeos read and replay COPIES of the recovered block/SHIP pairs.
    data, config = root / "restart", root / "config"
    (data / "blocks").mkdir(parents=True)
    (data / "state-history").mkdir()
    config.mkdir()
    (config / "config.ini").write_text("# Isolated recovery acceptance fixture\n")
    for kind, log in stages.items():
        dest = data / ("blocks" if kind == "blocks" else "state-history")
        shutil.copy2(log, dest / log.name)
        shutil.copy2(log.with_suffix(".index"), dest / log.with_suffix(".index").name)
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        port = sock.getsockname()[1]
    argv = [nodeos, "--data-dir", str(data), "--config-dir", str(config),
            "--p2p-listen-endpoint", "", "--http-server-address", "",
            "--plugin", "eosio::state_history_plugin", "--trace-history", "--chain-state-history",
            "--state-history-endpoint", f"127.0.0.1:{port}", "--chain-state-db-size-mb", "256",
            "--chain-state-db-guard-size-mb", "8", "--replay-blockchain", "--terminate-at-block", str(last)]
    cp = subprocess.run(argv, capture_output=True, text=True, timeout=90)
    (root / "restart.log").write_text(cp.stdout + cp.stderr)
    assert cp.returncode == 0, (cp.returncode, (cp.stdout + cp.stderr)[-8000:])
    assert f"configured maximum block {last}" in cp.stdout + cp.stderr, "nodeos did not replay through the intended block"
    assert {str(p): sha(p) for p in original_files} == original_hashes
    for kind, log in stages.items():
        complete = json.loads((log.parent.parent / "complete.json").read_text())
        assert sha(log) == complete["output"]["log_sha256"]
        assert sha(log.with_suffix(".index")) == complete["output"]["index_sha256"]
    result = {"ok": True, "through_block": last, "evidence": str(root), "restart_argv": argv,
              "tool_sha256": sha(pathlib.Path(tool)), "nodeos_sha256": sha(pathlib.Path(nodeos)),
              "checks": ["both recovery choices available for all three kinds",
                         "torn last entry and independently torn index restored",
                         "explicit salvage ends one block earlier", "reference verify passes",
                         "real nodeos replay accepts copies of all recovered pairs",
                         "original source, damaged copies and staged results unchanged"]}
    (root / "result.json").write_text(json.dumps(result, indent=2))
    print(json.dumps(result, indent=2))


if __name__ == "__main__":
    main()
