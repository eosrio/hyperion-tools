"""Freeze checked, indexed prefixes without changing a running node's files."""
import errno
import os
import pathlib
import shutil
import struct
import time

from .common import CHUNK, Pinned, event, publish, require, stamp, sync_dir

SHIP_MAGIC = 0xC35D500000000000
KINDS = ("blocks", "trace_history", "chain_state_history")


def indexed_plan(log, index, kind, through):
    header = log.read(0, 48)
    if kind == "blocks":
        version, first = struct.unpack_from("<II", header)
        require(version == 3, "snapshot supports only non-pruned block-log v3")
    else:
        require(struct.unpack_from("<Q", header)[0] == SHIP_MAGIC,
                "snapshot supports only non-pruned SHIP v0")
        first = int.from_bytes(header[8:12], "big")
    require(first > 0 and through >= first, "selected range precedes the log")
    index_size = (through - first + 1) * 8
    require(index.before.st_size >= index_size, "source index lacks selected head")
    at = struct.unpack("<Q", index.read(index_size - 8, 8))[0]
    if kind == "blocks":
        require(index.before.st_size >= index_size + 8,
                "choose a historical block with a following indexed position")
        end = struct.unpack("<Q", index.read(index_size, 8))[0]
    else:
        last = log.read(at, 48)
        require(struct.unpack_from("<Q", last)[0] == SHIP_MAGIC,
                "selected final SHIP entry has unsupported framing")
        require(int.from_bytes(last[8:12], "big") == through,
                "selected SHIP index points at another block")
        end = at + 56 + struct.unpack_from("<Q", last, 40)[0]
    require(0 <= at < end <= log.before.st_size, "selected entry exceeds source log")
    require(struct.unpack("<Q", log.read(end - 8, 8))[0] == at,
            "selected entry has an inconsistent position trailer")
    return {"kind": kind, "first_block": first, "last_block": through,
            "log_bytes": end, "index_bytes": index_size, "last_position": at}


def copy_prefix(source, target, size):
    started = time.monotonic()
    source.check(size)
    kernel_copy = True
    at, next_progress = 0, 1 << 30
    with target.open("xb", buffering=0) as output:
        os.fchmod(output.fileno(), 0o600)
        while at < size:
            count = min(CHUNK, size - at)
            if kernel_copy:
                try:
                    copied = os.copy_file_range(source.file.fileno(), output.fileno(), count,
                                                offset_src=at, offset_dst=at)
                    require(copied > 0, "source ended during bounded copy")
                except OSError as error:
                    if error.errno not in (errno.EXDEV, errno.ENOSYS, errno.EOPNOTSUPP):
                        raise
                    kernel_copy = False
                    output.seek(at)
                    continue
            else:
                data = source.read(at, count)
                copied = output.write(data)
                require(copied == len(data), "short destination write; keep incomplete corpus")
            at += copied
            if at >= next_progress:
                event("copy", path=str(target), copied=at, total=size)
                next_progress = at + (4 << 30)
        os.fsync(output.fileno())
    sync_dir(target.parent)
    copied_seconds = time.monotonic() - started
    destination = Pinned(target)
    try:
        require((destination.before.st_dev, destination.before.st_ino)
                != (source.before.st_dev, source.before.st_ino), "copy aliases the source")
        require(destination.before.st_nlink == 1, "copy has unexpected hard links")
        copied_hash = destination.digest(size, "hash-copy")
        source_hash = source.digest(size, "hash-source-prefix")
        require(copied_hash == source_hash, "copied bytes differ from pinned source prefix")
        require(destination.check(size) == stamp(destination.before), "copy changed during verification")
        os.chmod(target, 0o400)
        return {"source": str(source.path), "source_initial": stamp(source.before),
                "source_after": source.check(size), "bytes": size,
                "sha256": copied_hash, "source_prefix_sha256": source_hash,
                "copy_seconds": copied_seconds, "kernel_copy": kernel_copy,
                "identity": [destination.before.st_dev, destination.before.st_ino]}
    finally:
        destination.close()


def snapshot(args):
    require(os.name == "posix" and hasattr(os, "copy_file_range"), "snapshot requires Linux")
    require(len(args.kinds) == len(set(args.kinds)), "duplicate log kinds")
    root = args.out.resolve()
    require(not root.exists(), "output exists; incomplete snapshots are never reused")
    opened, plans = [], []
    try:
        for kind in args.kinds:
            directory = args.blocks if kind == "blocks" else args.ship
            require(directory is not None, f"source directory required for {kind}")
            directory = directory.resolve(strict=True)
            require(not root.is_relative_to(directory), "output must be outside live source directories")
            log = Pinned(directory / f"{kind}.log")
            opened.append(log)
            index = Pinned(directory / f"{kind}.index")
            opened.append(index)
            require((log.before.st_dev, log.before.st_ino)
                    != (index.before.st_dev, index.before.st_ino), "log/index alias")
            plan = indexed_plan(log, index, kind, args.through_block)
            plans.append((plan, log, index))
        total = sum(p[0]["log_bytes"] + p[0]["index_bytes"] for p in plans)
        require(root.parent.is_dir(), "output parent must already exist")
        require(shutil.disk_usage(root.parent).free >= total + max(total // 8, 1 << 30),
                "insufficient capacity for snapshot plus safety margin")
        root.mkdir(mode=0o700)
        sync_dir(root.parent)
        started = time.monotonic()
        publish(root / "plan.json", {
            "schema": 1, "operation": "snapshot_indexed_prefixes", "planned_bytes": total,
            "plans": [p[0] for p in plans], "created_unix": time.time(),
            "source_revision": args.source_revision,
            "live_sources": "Read only. Append-only growth is allowed; historical prefixes are hash-compared.",
            "cache_note": "Ordinary host cache; no global cache eviction.",
        })
        results = []
        for plan, log, index in plans:
            subdir = "blocks" if plan["kind"] == "blocks" else "state-history"
            directory = root / subdir
            if not directory.exists():
                directory.mkdir(mode=0o700)
                sync_dir(root)
            result = dict(plan)
            for name, source, size in [("log", log, plan["log_bytes"]),
                                       ("index", index, plan["index_bytes"])]:
                target = directory / source.path.name
                result[name] = {"path": str(target.relative_to(root)),
                                **copy_prefix(source, target, size)}
            results.append(result)
            publish(root / f"{plan['kind']}-ready.json", result)
            event("pair-ready", kind=plan["kind"], log_bytes=plan["log_bytes"])
        publish(root / "ready.json", {"schema": 1, "ok": True, "pairs": results,
                                      "elapsed_seconds": time.monotonic() - started})
        event("snapshot-complete", path=str(root), elapsed=time.monotonic() - started)
    finally:
        for source in opened:
            source.close()


def add_parser(subparsers):
    parser = subparsers.add_parser("snapshot", help="Copy and hash-compare indexed historical prefixes")
    parser.add_argument("--blocks", type=pathlib.Path, required=True)
    parser.add_argument("--ship", type=pathlib.Path)
    parser.add_argument("--kinds", nargs="+", choices=KINDS, default=list(KINDS))
    parser.add_argument("--through-block", type=int, required=True)
    parser.add_argument("--source-revision", required=True)
    parser.add_argument("--out", type=pathlib.Path, required=True)
    parser.set_defaults(run=snapshot)
