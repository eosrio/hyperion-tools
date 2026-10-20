"""Deterministic CLI damage cases over committed, provenance-recorded real bodies."""
import hashlib
import json
import os
import pathlib
import struct
import zlib

from .common import Pinned, event, publish, require, stamp
from .formats import block_header, ship_payload
from .process import run


def digest(data):
    return hashlib.sha256(data).hexdigest()


def mutation(original, actual):
    if actual is None:
        return {"operation": "remove-file", "original_bytes": len(original)}
    if actual == original:
        return {"operation": "unchanged"}
    if original.startswith(actual):
        return {"operation": "truncate", "offset": len(actual), "removed_bytes": len(original) - len(actual)}
    if actual.startswith(original):
        return {"operation": "append", "offset": len(original), "bytes": len(actual) - len(original),
                "appended_sha256": digest(actual[len(original):])}
    if len(original) != len(actual):
        start = next(n for n, (a, b) in enumerate(zip(original, actual)) if a != b)
        suffix = 0
        while suffix < min(len(original), len(actual)) - start and original[-suffix - 1] == actual[-suffix - 1]:
            suffix += 1
        removed = original[start:len(original) - suffix]
        inserted = actual[start:len(actual) - suffix]
        return {"operation": "splice", "offset": start,
                "removed_bytes": len(removed), "inserted_bytes": len(inserted),
                "removed_sha256": digest(removed), "inserted_sha256": digest(inserted)}
    offsets, changed = [], 0
    for n, (a, b) in enumerate(zip(original, actual)):
        if a != b:
            changed += 1
            if len(offsets) < 64:
                offsets.append(n)
    return {"operation": "overwrite", "changed_bytes": changed,
            "first_changed_offsets": offsets, "offset_list_truncated": changed > 64}


def groups(records):
    result = []
    for record in records:
        if not result or result[-1][-1]["number"] + 1 != record["number"]:
            result.append([])
        result[-1].append(record)
    return result


def reference(directory, records, chain_id, kinds=("blocks", "trace_history", "chain_state_history")):
    require(len(kinds) == len(set(kinds)) and set(kinds) <=
            {"blocks", "trace_history", "chain_state_history"}, "unknown or duplicate reference kind")
    directory.mkdir(mode=0o700)
    result = {}
    first = records[0]["number"]
    for kind in kinds:
        raw = bytearray(struct.pack("<II", 3, first) + bytes.fromhex(chain_id) + bytes([255]) * 8
                        if kind == "blocks" else b"")
        positions = []
        for record in records:
            body = bytes.fromhex(record[kind]["body_hex"])
            require(digest(body) == record[kind]["body_sha256"], "real fixture body checksum differs")
            if kind == "blocks":
                header = block_header(body)
                require(header["number"] == record["number"] and header["id"] == record[kind]["id"],
                        "real block differs from independent following-block ID oracle")
            else:
                payload = ship_payload(body)
                require(payload["block_id"] == record["blocks"]["id"]
                        and payload["uncompressed_sha256"] == record[kind]["uncompressed_sha256"],
                        "real SHIP payload differs from independent oracle")
            at = len(raw)
            positions.append(at)
            raw.extend(body)
            raw.extend(struct.pack("<Q", at))
        index = b"".join(struct.pack("<Q", at) for at in positions)
        log = directory / (kind + ".log")
        log.write_bytes(raw)
        log.with_suffix(".index").write_bytes(index)
        os.chmod(log, 0o400)
        os.chmod(log.with_suffix(".index"), 0o400)
        result[kind] = {"log": log, "bytes": bytes(raw), "index": index, "positions": positions}
    return result


def cases(kind, gold, records):
    raw, index, positions = gold["bytes"], gold["index"], gold["positions"]
    first, last = records[0]["number"], records[-1]["number"]
    head, end = positions[-1], len(raw)
    result = []

    def add(name, data=raw, idx=index, prefix=last, restore=True, salvage=True,
            inspect=True, trim=False, trim_prefix=None, limitation=False):
        result.append({"name": name, "data": data, "index": idx, "prefix": prefix,
                       "restore": restore, "salvage": salvage, "inspect": inspect,
                       "trim": trim, "trim_prefix": trim_prefix,
                       "limitation": limitation})

    cuts = {head + 1, head + 7, head + 47, head + (end - head) // 2, end - 8, end - 1}
    if kind == "blocks" and records[-1][kind].get("transactions", 0) >= 128:
        # A classic producer signature occupies one variant byte plus 65 bytes.
        # Leave only the first byte of the real multibyte transaction-count varuint.
        require(raw[head + records[-1][kind]["header_bytes"]] in (0, 1),
                "transaction-count cut requires a classic signature")
        cuts.add(head + records[-1][kind]["header_bytes"] + 67)
    for cut in sorted(c for c in cuts if head < c < end):
        add(f"torn-at-{cut - head}", raw[:cut], prefix=last - 1,
            trim=kind == "blocks", trim_prefix=last - 1)
    add("aligned-tail-loss", raw[:head], prefix=last - 1,
        trim=kind == "blocks", trim_prefix=last - 1)
    add("trailing-zeros", raw + bytes(64), trim=kind == "blocks", trim_prefix=last)
    add("trailing-garbage", raw + bytes([255]) * 73, trim=kind == "blocks", trim_prefix=last)
    add("bad-trailer", raw[:-8] + bytes(8), prefix=last - 1,
        trim=kind == "blocks", trim_prefix=last - 1)
    add("missing-index", idx=None)
    add("empty-index", idx=b"")
    add("partial-index-slot", idx=index[:-3])
    add("behind-index", idx=index[:-8])
    add("ahead-index", idx=index + struct.pack("<Q", len(raw) + 123), restore=False,
        trim=kind == "blocks", trim_prefix=last)
    add("duplicate-index-position", idx=index[:-8] + index[-16:-8])
    add("index-position-past-eof", idx=index[:-8] + struct.pack("<Q", len(raw) + 1))
    add("wrong-first-index-position", idx=struct.pack("<Q", positions[0] + 1) + index[8:])
    altered = bytearray(raw)
    if kind == "blocks":
        altered[head + records[-1][kind]["header_bytes"] + 33] ^= 1
        add("signature-bitflip", bytes(altered), prefix=last - 1, limitation=True)
        altered = bytearray(raw)
        altered[head + 24] ^= 1
        add("previous-id-bitflip", bytes(altered), prefix=last - 1, trim=True, trim_prefix=last - 1)
        signature_at = records[-1][kind]["header_bytes"]
        require(raw[head + signature_at] in (0, 1), "this fixture requires a classic signature layout")
        altered = bytearray(raw)
        tx_count = head + signature_at + 66
        altered[tx_count:tx_count + 2] = bytes([128, 0])
        add("noncanonical-transaction-count", bytes(altered), prefix=last - 1,
            trim=True, trim_prefix=last - 1)
    else:
        altered[end - 9] ^= 1
        add("zlib-checksum-bitflip", bytes(altered), prefix=last - 1)
        altered = bytearray(raw)
        altered[head + 18] ^= 1
        add("noncanonical-entry-id", bytes(altered), prefix=last - 1, limitation=True)
        altered = bytearray(raw)
        altered[head + 40:head + 48] = bytes([255]) * 8
        add("overflowed-payload-size", bytes(altered), prefix=last - 1)
        for length, name in [((1 << 64) - 56, "wrapped-zero-entry-size"),
                             ((1 << 64) - 57, "maximum-framed-entry-size")]:
            altered = bytearray(raw)
            altered[head + 40:head + 48] = struct.pack("<Q", length)
            add(name, bytes(altered), prefix=last - 1)
        old = raw[positions[-2]:head - 8]
        for latest in (False, True):
            repeated = bytearray(raw[:head])
            orphan = bytearray(old)
            orphan[39] ^= 1
            repeated.extend(orphan)
            repeated.extend(struct.pack("<Q", head))
            duplicate_at = len(repeated)
            repeated.extend(old)
            repeated.extend(struct.pack("<Q", duplicate_at))
            last_at = len(repeated)
            repeated.extend(raw[head:-8])
            repeated.extend(struct.pack("<Q", last_at))
            new_index = index[:-16] + struct.pack(
                "<QQ", duplicate_at if latest else positions[-2], last_at)
            add("canonical-repeat-index-" + ("latest" if latest else "first"),
                bytes(repeated), idx=new_index)
        layout = ship_payload(old)
        payload = zlib.decompress(old[layout["zlib_offset"]:])
        for changed in (False, True):
            data = payload[:-1] + bytes([payload[-1] ^ 1]) if changed else payload
            compressed = zlib.compress(data)
            marker = 0 if layout["marker"] == 1 else 1
            encoded = struct.pack("<I", marker)
            if marker == 1:
                encoded += struct.pack("<Q", len(data))
            encoded += compressed
            different = old[:40] + struct.pack("<Q", len(encoded)) + encoded
            require(different != old, "repeat conflict must differ from original bytes")
            repeated = raw[:head] + different + struct.pack("<Q", head)
            add("canonical-repeat-" + ("payload-conflict" if changed else "encoding-conflict"),
                repeated, prefix=last - 1)
    middle = len(positions) // 2
    altered = bytearray(raw)
    altered[positions[middle]:positions[middle + 1] - 8] = bytes(
        positions[middle + 1] - positions[middle] - 8)
    add("interior-zeroed-entry", bytes(altered), prefix=first + middle - 1,
        trim=kind == "blocks", trim_prefix=first + middle - 1)
    no_anchor = positions[0] + (1 if kind == "blocks" else 49)
    add("no-complete-anchor", raw[:no_anchor], prefix=None, restore=False, salvage=False)
    add("partial-preamble", raw[:16 if kind == "blocks" else 8],
        restore=False, salvage=False, inspect=False)
    altered = bytearray(raw)
    altered[3 if kind == "blocks" else 2] ^= 0x80
    add("unsupported-preamble", bytes(altered), restore=False, salvage=False, inspect=False)
    return result


def command(tool, argv, directory, success):
    result = run([tool, "--json", *map(str, argv)], directory, timeout=60, interval=0.05, announce=False)
    require((result["returncode"] == 0) == success and not result["timed_out"],
            f"unexpected command status in {directory.name}: {result['returncode']}")
    value = json.loads((directory / "stdout.json").read_text())
    require(value.get("ok") is success, "JSON success does not match expected status")
    return value


def prefix_bytes(gold, first, last):
    count = last - first + 1
    end = gold["positions"][count] if count < len(gold["positions"]) else len(gold["bytes"])
    return gold["bytes"][:end], gold["index"][:count * 8]


def execute_case(tool, root, kind, case, gold, refs, records, trim):
    root.mkdir(mode=0o700)
    inputs = root / "input"
    inputs.mkdir()
    log = inputs / (kind + ".log")
    index = log.with_suffix(".index")
    log.write_bytes(case["data"])
    if case["index"] is not None:
        index.write_bytes(case["index"])
    before = (digest(log.read_bytes()), digest(index.read_bytes()) if index.exists() else None)
    before_stamps = {p.name: stamp(p.stat()) for p in (log, index) if p.exists()}
    output_oracles = {}
    first, last = records[0]["number"], records[-1]["number"]
    publish(root / "mutation.json", {"name": case["name"], "kind": kind,
                                     "input_hashes": before, "expected_prefix": case["prefix"],
                                     "log_mutation": mutation(gold["bytes"], case["data"]),
                                     "index_mutation": mutation(gold["index"], case["index"]),
                                     "expected_restore": case["restore"],
                                     "expected_salvage": case["salvage"],
                                     "expected_trim": trim and case["trim"],
                                     "known_intrinsic_validation_limit": case["limitation"]})
    source_args = ["--blocks-log", refs["blocks"]["log"]]
    if kind != "blocks":
        source_args += ["--donor-log", refs[kind]["log"]]
    common = ["recover-tail", "--log", log, "--through-block", last, *source_args]
    inspection = command(tool, common, root / "inspect", case["inspect"])
    if case["inspect"]:
        require(inspection["prefix"]["last_block"] == case["prefix"], "unexpected retained prefix")
        for mode in ("restore", "salvage"):
            require(inspection["options"][mode]["available"] is case[mode],
                    f"unexpected {mode} eligibility")
    for mode in ("restore", "salvage"):
        output = root / (mode + "-output")
        command(tool, [*common, "--mode", mode, "--out", output, "--node-stopped"],
                root / (mode + "-run"), case[mode])
        if case[mode]:
            expected = (gold["bytes"], gold["index"]) if mode == "restore" else prefix_bytes(
                gold, first, case["prefix"])
            recovered = output / "recovered" / (kind + ".log")
            actual = (recovered.read_bytes(), recovered.with_suffix(".index").read_bytes())
            require(actual == expected,
                    f"{mode} output differs from independent original-byte oracle")
            output_oracles[mode] = {"expected_hashes": list(map(digest, expected)),
                                    "actual_hashes": list(map(digest, actual))}
            require((output / "complete.json").is_file(), "missing completion evidence")
        else:
            require(not output.exists(), "refused recovery created an output directory")
        require({p.name: stamp(p.stat()) for p in (log, index) if p.exists()} == before_stamps,
                "copy-out command rewrote or replaced a damaged input")
    require(before == (digest(log.read_bytes()), digest(index.read_bytes()) if index.exists() else None),
            "copy-out command changed its damaged originals")
    if case["limitation"]:
        unchecked = command(tool, ["verify", "--log", log], root / "intrinsic-verify", True)
        checked = command(tool, ["verify", "--log", log, "--against", refs["blocks"]["log"]],
                          root / "reference-verify", True)
        require(unchecked["corrupt"] is False and checked["corrupt"] is True,
                "reference-only corruption boundary changed; inspect the oracle")
    if case["name"] in ("overflowed-payload-size", "wrapped-zero-entry-size",
                         "maximum-framed-entry-size"):
        value = command(tool, ["verify", "--log", log], root / "overflow-verify", True)
        require(value["corrupt"] is True and value["skipped_entries"] == 0,
                "overflowing framing must produce findings, never wrap, panic or skip")
    if trim:
        journal = root / "journal"
        eligible = case["trim"] and os.name == "posix" and os.uname().sysname == "Linux"
        command(tool, ["trim-tail", "--log", log, "--journal", journal, "--node-stopped"],
                root / "trim-run", eligible)
        if eligible:
            expected = prefix_bytes(gold, first, case["trim_prefix"])
            require((log.read_bytes(), index.read_bytes()) == expected, "trim changed retained bytes")
            require((journal / "log.tail").read_bytes() == case["data"][len(expected[0]):]
                    and (journal / "index.tail").read_bytes() == case["index"][len(expected[1]):],
                    "saved suffix bytes differ")
            command(tool, ["resume-tail-trim", "--journal", journal, "--rollback", "--node-stopped"],
                    root / "rollback-run", True)
        else:
            require(not journal.exists(), "refused trim created a journal")
        require(before == (digest(log.read_bytes()), digest(index.read_bytes()) if index.exists() else None),
                "trim refusal/rollback did not preserve the damaged originals")
        require(all((p.stat().st_dev, p.stat().st_ino) ==
                    (before_stamps[p.name]["device"], before_stamps[p.name]["inode"])
                    for p in (log, index) if p.exists()), "trim/rollback replaced a target inode")
    publish(root / "oracle.json", {"ok": True, "copyout_originals_preserved": True,
                                   "copyout_output_hashes": output_oracles,
                                   "damaged_input_hashes": before,
                                   "input_stamps": before_stamps,
                                   "trim_rollback_or_refusal_exact": trim})


def damage(args):
    fixture, tool = Pinned(args.fixture), None
    root, results = args.out.resolve(), []
    try:
        require(len(args.kinds) == len(set(args.kinds)), "duplicate damage kinds")
        tool = Pinned(args.tool)
        root.mkdir(mode=0o700)
        require(fixture.before.st_size <= 16 << 20, "fixture JSON exceeds input bound")
        fixture_bytes = fixture.read(0, fixture.before.st_size)
        value = json.loads(fixture_bytes)
        require(value["schema"] == 1, "unknown real-entry fixture")
        records = value["records"]
        require(0 < len(records) <= 10000 and all(a["number"] < b["number"]
                for a, b in zip(records, records[1:])), "fixture records are not ordered")
        tool_hash = tool.digest(tool.before.st_size, "hash-tool")
        publish(root / "plan.json", {"schema": 1, "operation": "real_entry_damage_matrix",
                                     "fixture_sha256": digest(fixture_bytes), "tool_sha256": tool_hash,
                                     "kinds": args.kinds, "trim": args.trim,
                                     "source_provenance": value["corpus_sha256"]})
        for group in groups(records):
            require(len(group) >= 3, "each real range needs complete anchors")
            label = str(group[0]["number"])
            needed = set(args.kinds) | {"blocks"}
            refs = reference(root / ("reference-" + label), group, value["chain_id"],
                             tuple(k for k in ("blocks", "trace_history", "chain_state_history")
                                   if k in needed))
            for kind in args.kinds:
                for case in cases(kind, refs[kind], group):
                    directory = root / (label + "-" + kind + "-" + case["name"])
                    passed, error = True, None
                    try:
                        execute_case(tool.path, directory, kind, case, refs[kind], refs, group, args.trim)
                    except Exception as failure:
                        passed, error = False, str(failure)
                        publish(directory / "failure.json", {"ok": False, "error": error})
                    results.append({"range_start": group[0]["number"], "kind": kind,
                                    "case": case["name"], "passed": passed, "error": error})
                    event("damage-case", **results[-1])
                    require(all(p["log"].read_bytes() == p["bytes"] and
                                p["log"].with_suffix(".index").read_bytes() == p["index"]
                                for p in refs.values()), "immutable reference fixture changed; stop")
        require(fixture.check(fixture.before.st_size) == stamp(fixture.before), "fixture input changed")
        require(tool.check(tool.before.st_size) == stamp(tool.before), "binary changed during damage matrix")
        ok = all(r["passed"] for r in results)
        publish(root / "results.json", {"schema": 1, "ok": ok, "cases": results,
                                       "tool_sha256": tool_hash, "fixture_sha256": digest(fixture_bytes)})
        require(ok, "damage matrix has failures; keep every failed case and its evidence")
        publish(root / "complete.json", {"ok": True, "cases": len(results), "tool_sha256": tool_hash})
    finally:
        fixture.close()
        if tool is not None:
            tool.close()


def add_parser(subparsers):
    parser = subparsers.add_parser("damage", help="Deterministic CLI damage cases on real-entry copies")
    parser.add_argument("--fixture", type=pathlib.Path, required=True)
    parser.add_argument("--tool", type=pathlib.Path, required=True)
    parser.add_argument("--out", type=pathlib.Path, required=True)
    parser.add_argument("--kinds", nargs="+", choices=("blocks", "trace_history", "chain_state_history"),
                        default=["blocks", "trace_history", "chain_state_history"])
    parser.add_argument("--trim", action="store_true")
    parser.set_defaults(run=damage)
