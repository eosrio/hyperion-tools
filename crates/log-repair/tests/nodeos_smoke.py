#!/usr/bin/env python3
"""Optional real-nodeos smoke test; Python stdlib only, isolated development chain.

Creates ~80 empty blocks using nodeos's built-in development key, takes an early
snapshot, then exercises scan, multi-region fetch-ship, build, prepare (network and
local blocks.log sources), a verified no-op apply, and sliced snapshot replay against
the real on-disk block and SHIP entries. Never opens an existing chain directory. No
media damage is injected here: see eio_smoke.py and the Rust tests.
"""
import argparse
import hashlib
import json
import pathlib
import shutil
import socket
import subprocess
import time
import urllib.request


def port():
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--tool", required=True, type=pathlib.Path)
    parser.add_argument("--nodeos", default="/usr/bin/nodeos", type=pathlib.Path)
    parser.add_argument("--out", required=True, type=pathlib.Path)
    args = parser.parse_args()
    args.out.mkdir()  # refuse existing paths
    root = args.out.resolve()
    tool = str(args.tool.resolve())
    nodeos = str(args.nodeos.resolve())
    data = root / "source"
    config = root / "config"
    config.mkdir()
    (config / "config.ini").write_text("# Local development-chain smoke test\n")
    http, ship = port(), port()

    def api(route):
        request = urllib.request.Request(
            f"http://127.0.0.1:{http}/v1/{route}", data=b"{}",
            headers={"Content-Type": "application/json"})
        with urllib.request.urlopen(request, timeout=5) as response:
            return json.load(response)

    base = [nodeos, "--data-dir", str(data), "--config-dir", str(config),
            "--http-server-address", f"127.0.0.1:{http}",
            "--p2p-listen-endpoint", "", "--plugin", "eosio::chain_api_plugin",
            "--plugin", "eosio::state_history_plugin", "--trace-history",
            "--chain-state-history", "--state-history-endpoint", f"127.0.0.1:{ship}",
            "--chain-state-db-size-mb", "256", "--chain-state-db-guard-size-mb", "8"]

    def start(extra, name):
        output = open(root / name, "w")
        child = subprocess.Popen(base + extra, stdout=output, stderr=subprocess.STDOUT)
        output.close()
        return child

    def stop(child):
        child.terminate()
        try:
            child.wait(timeout=20)
        except subprocess.TimeoutExpired:
            child.kill()
            child.wait()
            raise RuntimeError("owned nodeos did not stop cleanly")

    child = start(["--plugin", "eosio::producer_api_plugin", "--producer-name", "eosio",
                   "--enable-stale-production"], "produce.log")
    snapshot = None
    try:
        deadline = time.monotonic() + 90
        while time.monotonic() < deadline:
            if child.poll() is not None:
                raise RuntimeError("nodeos exited:\n" + (root / "produce.log").read_text()[-8000:])
            try:
                info = api("chain/get_info")
            except (OSError, ValueError):
                time.sleep(0.2)
                continue
            if info["head_block_num"] >= 10 and snapshot is None:
                snapshot = api("producer/create_snapshot")
                (root / "snapshot.json").write_text(json.dumps(snapshot, indent=2))
            if info["last_irreversible_block_num"] >= 80:
                break
            time.sleep(0.2)
        else:
            raise RuntimeError("timed out producing local chain")
    finally:
        stop(child)


    target_dir = root / "target"
    shutil.copytree(data / "state-history", target_dir)
    shutil.copytree(data / "blocks", target_dir / "blocks")

    def run(*argv, name):
        result = subprocess.run([tool, "--json", *map(str, argv)], capture_output=True,
                                text=True, timeout=120)
        (root / name).write_text(result.stdout + "\nSTDERR:\n" + result.stderr)
        if result.returncode:
            raise RuntimeError(f"tool failed; see {name}: {result.stdout} {result.stderr}")
        return json.loads(result.stdout)

    def entry_pos(path, block):
        first = int.from_bytes(path.read_bytes()[8:12], "big") if path.stem != "blocks" \
            else int.from_bytes(path.read_bytes()[4:8], "little")
        index = path.with_suffix(".index").read_bytes()
        return int.from_bytes(index[(block-first)*8:(block-first)*8+8], "little")

    def two_records(path):
        # Two separate damaged areas, chosen by entry index (the initial full-state delta
        # can dominate a chain_state_history file's size).
        count = len(path.with_suffix(".index").read_bytes()) // 8
        first = int.from_bytes(path.read_bytes()[4:8], "little") if path.stem == "blocks" \
            else int.from_bytes(path.read_bytes()[8:12], "big")
        return [entry_pos(path, first + count * k // 5) // 512 for k in (2, 4)]

    def expected_sha(path, records):
        data = path.read_bytes()
        return hashlib.sha256(b"".join(data[r*512:(r+1)*512] for r in sorted(records))).hexdigest()

    def selection(path, records):
        return ["--log", path, "--record-size", "512",
                *[x for r in records for x in ("--record-index", str(r))]]

    def check_built(path, records, built, original):
        assert all(s == "identical" for s in built["extent_states"]), built["extent_states"]
        assert built["replacement_sha256"] == expected_sha(path, records)
        assert path.read_bytes() == original

    blocks_log = target_dir / "blocks" / "blocks.log"
    child = start([], "serve.log")
    try:
        deadline = time.monotonic() + 15
        while time.monotonic() < deadline:
            try:
                api("chain/get_info")
                break
            except OSError:
                time.sleep(0.2)
        for path in (blocks_log, target_dir / "trace_history.log",
                     target_dir / "chain_state_history.log"):
            stem, original = path.stem, path.read_bytes()
            clean = run("scan", "--log", path, "--record-size", "512", name=f"{stem}-scan.json")
            assert clean["damaged"] is False
            records = two_records(path)
            sel = selection(path, records)
            report = run("scan", *sel, name=f"{stem}-scan-records.json")
            assert len(report["regions"]) == 2 and all(r["repairable"] for r in report["regions"])
            assert snapshot["head_block_num"] <= report["regions"][0]["first_block"] - 2
            source = root / f"{stem}-websocket"
            fetched = run("fetch-ship", *sel, "--ship", f"ws://127.0.0.1:{ship}", "--out", source,
                          name=f"{stem}-fetch.json")
            assert len(fetched["regions"]) == 2
            built = run("build", *sel, "--sources", source, "--out", root / f"{stem}-bundle",
                        name=f"{stem}-build.json")
            check_built(path, records, built, original)
            prepared = run("prepare", *sel, "--ship", f"ws://127.0.0.1:{ship}",
                           "--out", root / f"{stem}-prepare", name=f"{stem}-prepare.json")
            check_built(path, records, prepared["build"], original)
            applied = run("apply", "--bundle", root / f"{stem}-prepare" / "bundle", "--approve",
                          prepared["build"]["plan_sha256"], "--node-stopped",
                          name=f"{stem}-apply.json")
            assert applied["applied"] is False
            assert path.read_bytes() == original
    finally:
        stop(child)

    # Another node's intact blocks.log as the block source, without any network.
    records = two_records(blocks_log)
    local = run("prepare", *selection(blocks_log, records), "--blocks-log",
                data / "blocks" / "blocks.log", "--out", root / "blocks-local",
                name="blocks-local-prepare.json")
    check_built(blocks_log, records, local["build"], blocks_log.read_bytes())

    # Replay from the early snapshot for each SHIP kind, through the first 512 B record at or
    # after head + 2: as close to the documented boundary as the record grid allows. The
    # oversized full-state anchor case itself is covered by the Rust tests.
    head = snapshot["head_block_num"]
    for stem in ("chain_state_history", "trace_history"):
        path = target_dir / f"{stem}.log"
        records = [-(-entry_pos(path, head + 2) // 512)]
        sel = selection(path, records)
        report = run("scan", *sel, name=f"{stem}-replay-scan.json")
        assert report["snapshot_latest_head_block"] >= head
        replay = root / f"{stem}-replay"
        run("replay-ship", *sel, "--nodeos", nodeos, "--snapshot", snapshot["snapshot_name"],
            "--snapshot-block", head, "--blocks-dir", data / "blocks", "--out", replay,
            name=f"{stem}-replay-result.json")
        replay_json = json.loads((replay / "replay.json").read_text())
        assert replay_json["blocklog_slice"][0] == head
        built = run("build", *sel, "--sources", replay, "--out", root / f"{stem}-replayed",
                    name=f"{stem}-replayed-build.json")
        check_built(path, records, built, path.read_bytes())
    print(json.dumps({"ok": True, "evidence": str(root), "checks": [
        "clean whole-file scans of real block, trace and chain-state logs",
        "two-region WebSocket acquisition for every log kind matches disk records",
        "prepare via --ship and via another node's blocks.log; verified no-op apply",
        "sliced-blocklog snapshot replay generates compatible trace and chain-state donors",
        "target logs unchanged"]}, indent=2))


if __name__ == "__main__":
    main()
