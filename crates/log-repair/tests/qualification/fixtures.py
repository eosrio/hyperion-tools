"""Rebase short canonical real-history ranges, retaining independent byte oracles."""
import hashlib
import json
import os
import pathlib
import struct

from .bench import Corpus
from .common import Pinned, event, publish, require, stamp, sync_dir
from .formats import LIMIT, block_header, extent, ship_payload
from .process import run


def positions(pair, first, last):
    start = (first - pair["manifest"]["first_block"]) * 8
    data = pair["index"].read(start, (last - first + 1) * 8)
    found = [n[0] for n in struct.iter_unpack("<Q", data)]
    if last < pair["manifest"]["last_block"]:
        found.append(struct.unpack("<Q", pair["index"].read(start + len(data), 8))[0])
    else:
        found.append(pair["log"].before.st_size)
    require(all(a < b for a, b in zip(found, found[1:])), "non-increasing fixture positions")
    return found


def file_evidence(path):
    os.chmod(path, 0o400)
    pinned = Pinned(path)
    try:
        digest = pinned.digest(pinned.before.st_size, "hash-fixture")
        return {"sha256": digest, "bytes": pinned.before.st_size, "stamp": stamp(pinned.before)}
    finally:
        pinned.close()


def extract(args):
    require(1 < args.first <= args.last and args.last - args.first < 10000,
            "select 1..10000 blocks after genesis")
    root = args.out.resolve()
    root.mkdir(mode=0o700)
    corpus, utility = None, Pinned(args.utility)
    try:
        corpus = Corpus(args.corpus)
        require(all(pair["manifest"]["first_block"] <= args.first <= args.last
                    <= pair["manifest"]["last_block"] for pair in corpus.pairs.values()),
                "selected range is absent from a corpus kind")
        source = corpus.pairs["blocks"]
        require(args.first > source["manifest"]["first_block"],
                "fixture needs a predecessor in the source to check its first header link")
        result = run([utility.path, "block-log", "extract-blocks", "--blocks-dir",
                      source["log"].path.parent, "--first", args.first, "--last", args.last,
                      "--output-dir", root / "blocks"], root / "utility-extract", interval=0.05)
        require(result["returncode"] == 0, "independent utility extraction failed")
        old_stem = f"blocks-{args.first}-{args.last}"
        for suffix in (".log", ".index"):
            (root / "blocks" / (old_stem + suffix)).rename(root / "blocks" / ("blocks" + suffix))
        copied_log, copied_index = Pinned(root / "blocks/blocks.log"), Pinned(root / "blocks/blocks.index")
        entries, headers = [], {}
        try:
            require(copied_log.read(0, 8) == struct.pack("<II", 3, args.first),
                    "utility produced unexpected block preamble")
            require(copied_log.read(40, 8) == bytes([255]) * 8, "utility separator differs")
            chain_id = copied_log.read(8, 32).hex()
            require(copied_index.before.st_size == (args.last - args.first + 1) * 8,
                    "utility did not produce the requested number of slots")
            source_positions = positions(source, args.first, args.last)
            output_positions = [n[0] for n in struct.iter_unpack(
                "<Q", copied_index.read(0, copied_index.before.st_size))]
            output_positions.append(copied_log.before.st_size)
            previous = None
            if args.first > source["manifest"]["first_block"]:
                prior = positions(source, args.first - 1, args.first - 1)
                previous = block_header(extent(source["log"], prior[0], prior[1] - prior[0] - 8))["id"]
            for j, number in enumerate(range(args.first, args.last + 1)):
                a, b = source_positions[j:j + 2]
                body = extent(source["log"], a, b - a - 8)
                require(source["log"].read(b - 8, 8) == struct.pack("<Q", a),
                        "source block trailer differs")
                oa, ob = output_positions[j:j + 2]
                require(copied_log.read(ob - 8, 8) == struct.pack("<Q", oa)
                        and extent(copied_log, oa, ob - oa - 8) == body,
                        "utility output differs from selected source body")
                header = block_header(body)
                require(header["number"] == number and header["previous"] == previous,
                        "selected block header chain does not join its predecessor")
                previous = header["id"]
                headers[number] = header
                entries.append({"block": number, "source_offset": a, "offset": oa,
                                "bytes": ob - oa, "body_sha256": hashlib.sha256(body).hexdigest(),
                                **header})
        finally:
            copied_log.close()
            copied_index.close()
        pairs = {"blocks": {"entries": entries}}
        for kind in ("trace_history", "chain_state_history"):
            pair = corpus.pairs[kind]
            source_positions = positions(pair, args.first, args.last)
            directory = root / "state-history"
            if not directory.exists():
                directory.mkdir(mode=0o700)
            log_path, index_path = directory / f"{kind}.log", directory / f"{kind}.index"
            entries, omitted_bytes = [], 0
            with log_path.open("xb") as log, index_path.open("xb") as index:
                for j, number in enumerate(range(args.first, args.last + 1)):
                    a, b = source_positions[j:j + 2]
                    header = pair["log"].read(a, 48)
                    length = 48 + struct.unpack_from("<Q", header, 40)[0]
                    require(length + 8 <= b - a, "source SHIP entry exceeds its indexed span")
                    require(pair["log"].read(a + length, 8) == struct.pack("<Q", a),
                            "source SHIP trailer differs")
                    body = extent(pair["log"], a, length)
                    payload = ship_payload(body)
                    require(payload["block_id"] == headers[number]["id"],
                            "SHIP record differs from independently computed canonical header ID")
                    at = log.tell()
                    require(at + length + 8 <= LIMIT, "selected fixture exceeds 256 MiB")
                    log.write(body)
                    log.write(struct.pack("<Q", at))
                    index.write(struct.pack("<Q", at))
                    entries.append({"block": number, "source_offset": a, "offset": at,
                                    "bytes": length + 8, "body_sha256": hashlib.sha256(body).hexdigest(),
                                    **payload})
                    omitted_bytes += b - a - length - 8
                for output in (log, index):
                    output.flush()
                    os.fsync(output.fileno())
            sync_dir(directory)
            pairs[kind] = {"entries": entries, "excluded_physical_gap_bytes": omitted_bytes}
        for kind, pair in pairs.items():
            folder = "blocks" if kind == "blocks" else "state-history"
            for name, suffix in (("log", ".log"), ("index", ".index")):
                path = root / folder / (kind + suffix)
                pair[name] = {"path": str(path.relative_to(root)), **file_evidence(path)}
        corpus.check()
        require(utility.check(utility.before.st_size) == stamp(utility.before), "utility changed")
        publish(root / "ready.json", {"schema": 1, "ok": True, "operation": "canonical_fixture",
                                      "chain_id": chain_id, "first_block": args.first,
                                      "last_block": args.last, "pairs": pairs,
                                      "source_corpus": str(corpus.root),
                                      "utility_sha256": utility.digest(utility.before.st_size),
                                      "source_manifest_sha256": hashlib.sha256(
                                          (corpus.root / "ready.json").read_bytes()).hexdigest(),
                                      "selection": "Indexed canonical bodies; SHIP physical gaps are excluded explicitly.",
                                      "validation": "Byte equality, header-ID chain and independent Python zlib checksums; no signature or Merkle authentication."})
        event("fixture-ready", path=str(root), first=args.first, last=args.last)
    finally:
        utility.close()
        if corpus is not None:
            corpus.close()


def add_parser(subparsers):
    parser = subparsers.add_parser("extract", help="Select and rebase a short canonical real-history fixture")
    parser.add_argument("--corpus", type=pathlib.Path, required=True)
    parser.add_argument("--utility", type=pathlib.Path, required=True)
    parser.add_argument("--first", type=int, required=True)
    parser.add_argument("--last", type=int, required=True)
    parser.add_argument("--out", type=pathlib.Path, required=True)
    parser.set_defaults(run=extract)
