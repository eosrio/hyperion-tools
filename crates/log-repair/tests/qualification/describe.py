"""Stream indexed sizes to select real large-entry workloads without guessing density."""
import heapq
import pathlib
import struct

from .bench import Corpus
from .common import CHUNK, event, publish, require
from .formats import FC, block_header


def describe(args):
    root = args.out.resolve()
    root.mkdir(mode=0o700)
    corpus = Corpus(args.corpus, {"blocks"})
    try:
        pair = corpus.pairs["blocks"]
        index, log = pair["index"], pair["log"]
        first, last = pair["manifest"]["first_block"], pair["manifest"]["last_block"]
        previous, number = None, first
        top, histogram, sample = [], {}, []
        minimum, count = None, 0

        def entry(start, end):
            nonlocal minimum, count, number
            size = end - start
            require(size > 8 and end <= log.before.st_size, "invalid indexed entry span")
            minimum = size if minimum is None else min(minimum, size)
            bucket = size.bit_length() - 1
            histogram[bucket] = histogram.get(bucket, 0) + 1
            if count % 1024 == 0:
                sample.append(size)
            item = (size, number, start)
            if len(top) < 32:
                heapq.heappush(top, item)
            elif item > top[0]:
                heapq.heapreplace(top, item)
            count += 1
            number += 1

        for at in range(0, index.before.st_size, CHUNK):
            raw = index.read(at, min(CHUNK, index.before.st_size - at))
            for (position,) in struct.iter_unpack("<Q", raw):
                if previous is not None:
                    entry(previous, position)
                previous = position
            event("describe-index", bytes=at + len(raw), total=index.before.st_size, entries=count)
        require(previous is not None, "empty corpus index")
        entry(previous, log.before.st_size)
        require(count == last - first + 1, "indexed entry count differs")
        largest = []
        for size, number, at in sorted(top, reverse=True):
            prefix = log.read(at, min(size - 8, 1 << 20))
            header = block_header(prefix)
            require(header["number"] == number, "selected large entry has wrong block number")
            reader = FC(prefix)
            reader.at = header["header_bytes"]
            variant = reader.var()
            transactions = None
            if variant in (0, 1):
                reader.take(65)
                transactions = reader.var()
            largest.append({"block": number, "offset": at, "bytes": size,
                            "transactions": transactions, "signature_variant": variant, **header})
        sample.sort()
        corpus.check()
        publish(root / "complete.json", {
            "ok": True, "entries": count, "minimum_bytes": minimum,
            "size_histogram_log2": histogram, "largest": largest,
            "sampled_quantiles_bytes": {str(q): sample[((len(sample) - 1) * q) // 100]
                                        for q in (50, 95, 99)},
            "quantile_note": "Systematic sample of every 1024th indexed entry; histogram and maximum cover all entries.",
            "sample_count": len(sample), "source_log_sha256": pair["manifest"]["log"]["sha256"],
            "source_index_sha256": pair["manifest"]["index"]["sha256"],
        })
        event("description-complete", entries=count, largest=largest[:3])
    finally:
        corpus.close()


def add_parser(subparsers):
    parser = subparsers.add_parser("describe", help="Measure indexed sizes and identify real large entries")
    parser.add_argument("--corpus", type=pathlib.Path, required=True)
    parser.add_argument("--out", type=pathlib.Path, required=True)
    parser.set_defaults(run=describe)
