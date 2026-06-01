#!/usr/bin/env python3
"""
Load reader NDJSON into a LOCAL Elasticsearch via the `_bulk` API and report write throughput.

This is the write-side benchmark: it measures how fast ES ingests the docs the direct-from-disk
reader (action-proto / delta-proto) produces, with the Hyperion index naming + _id rules so the
result is queryable exactly like a Hyperion index.

  action-proto ... --out actions.ndjson
  python scripts/bulk-load.py --mode action --chain wax actions.ndjson

  delta-proto ... --out deltas.ndjson
  python scripts/bulk-load.py --mode delta --chain wax deltas.ndjson

_id / _index (mirrors Hyperion):
  action: _id = global_sequence ; _index = <chain>-action-<version>-<partition>
  delta:  _id = <block_num>-<code>-<scope>-<table>-<primary_key> ; _index = <chain>-delta-<version>-<partition>
  partition = ceil(block_num / INDEX_PARTITION_SIZE), zero-padded to 6 digits.

SAFETY: refuses a non-loopback ES host unless BENCH_ALLOW_EXTERNAL_ES=1. Local benchmarking only.
"""
import argparse
import concurrent.futures
import json
import math
import os
import sys
import threading
import time
import urllib.request


def cfg(key: str, default: str) -> str:
    """Config value: environment variable > bench/.env > default."""
    v = os.environ.get(key)
    if v is not None:
        return v
    try:
        envp = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", ".env")
        with open(envp) as f:
            for line in f:
                line = line.strip()
                if line.startswith(key + "="):
                    val = line.split("=", 1)[1].strip()
                    if len(val) >= 2 and val[0] == val[-1] and val[0] in "\"'":
                        val = val[1:-1]
                    return val
    except OSError:
        pass
    return default


def partition(block_num: int, size: int) -> str:
    return str(max(1, math.ceil(block_num / size))).zfill(6)


def doc_meta(mode: str, doc: dict, chain: str, version: str, size: int):
    block = int(doc["block_num"])
    part = partition(block, size)
    if mode == "action":
        return f"{chain}-action-{version}-{part}", str(doc["global_sequence"])
    # delta
    _id = f"{block}-{doc['code']}-{doc['scope']}-{doc['table']}-{doc['primary_key']}"
    return f"{chain}-delta-{version}-{part}", _id


def post_bulk(es: str, payload: bytes):
    req = urllib.request.Request(es + "/_bulk", data=payload,
                                 headers={"Content-Type": "application/x-ndjson"}, method="POST")
    with urllib.request.urlopen(req, timeout=120) as r:
        body = r.read()
    # cheap error check without a JSON parse of the whole (large) response
    errors = body.count(b'"error"')
    return errors


def guard_local(es: str):
    host = es.split("://", 1)[-1].split("/", 1)[0].split(":", 1)[0]
    if host in ("localhost", "127.0.0.1", "::1", "") or os.environ.get("BENCH_ALLOW_EXTERNAL_ES") == "1":
        return
    sys.exit(f"REFUSING: ES host '{host}' is not loopback. Local benchmarking only "
             "(set BENCH_ALLOW_EXTERNAL_ES=1 to override — you almost certainly should not).")


def build_batch(lines, args):
    """Turn raw NDJSON byte-lines into a _bulk payload; returns (payload, doc_count)."""
    buf = bytearray()
    n = 0
    for line in lines:
        s = line.strip()
        if not s:
            continue
        doc = json.loads(s)
        index, _id = doc_meta(args.mode, doc, args.chain, args.version, args.partition_size)
        buf += b'{"index":{"_index":"%s","_id":"%s"}}\n' % (index.encode(), _id.encode())
        buf += s + b"\n"
        n += 1
    return bytes(buf), n


def run_parallel(args):
    """N worker threads each read a batch of lines (under a lock), build + POST it. Parallelizes
    both JSON parsing and the _bulk POSTs so the loader can saturate ES."""
    f = open(args.file, "rb")
    rlock = threading.Lock()
    alock = threading.Lock()
    agg = {"docs": 0, "bytes": 0, "errors": 0, "batches": 0}
    stop = threading.Event()

    def worker():
        while not stop.is_set():
            with rlock:
                lines = []
                for _ in range(args.batch):
                    ln = f.readline()
                    if not ln:
                        break
                    lines.append(ln)
            if not lines:
                return
            payload, n = build_batch(lines, args)
            if n == 0:
                continue
            errs = post_bulk(args.es, payload)
            with alock:
                agg["docs"] += n
                agg["bytes"] += len(payload)
                agg["errors"] += errs
                agg["batches"] += 1
                if args.limit and agg["docs"] >= args.limit:
                    stop.set()

    t0 = time.monotonic()
    with concurrent.futures.ThreadPoolExecutor(max_workers=args.workers) as ex:
        for fut in [ex.submit(worker) for _ in range(args.workers)]:
            fut.result()
    dt = max(time.monotonic() - t0, 1e-9)
    mb = agg["bytes"] / 1e6
    print(f"[bulk-load] {agg['docs']} docs in {dt:.1f}s -> {agg['docs']/dt:,.0f} docs/s | "
          f"{mb:.1f} MB ({mb/dt:.1f} MB/s) | {agg['batches']} bulk reqs | {args.workers} workers | "
          f"errors={agg['errors']}", file=sys.stderr)
    if agg["errors"]:
        sys.exit(f"  {agg['errors']} bulk item error(s).")


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("file", nargs="?", help="NDJSON file (default: stdin)")
    ap.add_argument("--es", default=cfg("ES", "http://localhost:9200"))
    ap.add_argument("--chain", default=cfg("CHAIN", "wax"))
    ap.add_argument("--version", default=cfg("INDEX_VERSION", "v1"))
    ap.add_argument("--mode", choices=["action", "delta"], required=True)
    ap.add_argument("--partition-size", type=int, default=int(cfg("INDEX_PARTITION_SIZE", "10000000")))
    ap.add_argument("--batch", type=int, default=4000, help="docs per _bulk request")
    ap.add_argument("--workers", type=int, default=1, help="concurrent _bulk posters (needs a file, not stdin)")
    ap.add_argument("--limit", type=int, default=0, help="stop after N docs (0 = all)")
    args = ap.parse_args()
    guard_local(args.es)

    if args.workers > 1 and args.file:
        run_parallel(args)
        return

    src = open(args.file, "rb") if args.file else sys.stdin.buffer
    t0 = time.monotonic()
    docs = bytes_sent = errors = batches = 0
    buf = bytearray()
    in_batch = 0

    def flush():
        nonlocal buf, in_batch, bytes_sent, errors, batches
        if in_batch == 0:
            return
        bytes_sent += len(buf)
        errors += post_bulk(args.es, bytes(buf))
        batches += 1
        buf = bytearray()
        in_batch = 0

    for line in src:
        line = line.strip()
        if not line:
            continue
        doc = json.loads(line)
        index, _id = doc_meta(args.mode, doc, args.chain, args.version, args.partition_size)
        buf += b'{"index":{"_index":"%s","_id":"%s"}}\n' % (index.encode(), _id.encode())
        buf += line + b"\n"
        docs += 1
        in_batch += 1
        if in_batch >= args.batch:
            flush()
        if args.limit and docs >= args.limit:
            break
    flush()

    dt = max(time.monotonic() - t0, 1e-9)
    mb = bytes_sent / 1e6
    print(f"[bulk-load] {docs} docs in {dt:.1f}s -> {docs / dt:,.0f} docs/s | "
          f"{mb:.1f} MB ({mb / dt:.1f} MB/s) | {batches} bulk reqs | errors={errors}", file=sys.stderr)
    if errors:
        sys.exit(f"  {errors} bulk item error(s) — inspect the ES response / mapping conflicts.")


if __name__ == "__main__":
    main()
