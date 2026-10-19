#!/usr/bin/env python3
"""Re-extract the vendored Spring SHiP ABI into crates/core/src/ship_abi.json.

The ABI a Spring state-history node sends as its first (text) message is the raw string
literal `state_history_plugin_abi` in `<spring>/libraries/state_history/abi.cpp`. This copies
that literal byte for byte, with LF line endings: the git blob is LF, which is what a Linux
build compiles, while a Windows checkout may hold the file with CRLF endings. Hashing the
working-tree file as-is would therefore give a different digest.

Run it by hand when moving to a new Spring release (it is not part of CI), then update
`SPRING_SHIP_ABI_SHA256` in crates/core/src/protocol.rs to the digest it prints:

    python3 scripts/extract_ship_abi.py <spring checkout>
    python3 scripts/extract_ship_abi.py <spring checkout> --check   # compare, write nothing
"""

from __future__ import annotations

import argparse
import hashlib
import json
import re
import sys
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
VENDORED = REPO / "crates" / "core" / "src" / "ship_abi.json"
PROTOCOL = REPO / "crates" / "core" / "src" / "protocol.rs"
SOURCE = Path("libraries") / "state_history" / "abi.cpp"

# The C++ raw string `R"delim( ... )delim"` assigned to state_history_plugin_abi. It ends at
# the first `)delim"`, exactly as the compiler ends it.
LITERAL = re.compile(
    rb'state_history_plugin_abi\s*=\s*R"(?P<delim>[^()\\\s"]{0,16})\((?P<abi>.*?)\)(?P=delim)"',
    re.DOTALL,
)

# The protocol codec hard-codes these indices; Spring only ever appends to them.
REQUEST = [
    "get_status_request_v0",
    "get_blocks_request_v0",
    "get_blocks_ack_request_v0",
    "get_blocks_request_v1",
    "get_status_request_v1",
]
RESULT = [
    "get_status_result_v0",
    "get_blocks_result_v0",
    "get_blocks_result_v1",
    "get_status_result_v1",
]


def extract(spring: Path) -> bytes:
    src = spring / SOURCE
    if not src.is_file():
        sys.exit(f"error: {src} not found; pass the root of a Spring source checkout")
    text = src.read_bytes().replace(b"\r\n", b"\n")
    m = LITERAL.search(text)
    if m is None:
        sys.exit(f"error: no state_history_plugin_abi raw literal in {src}")
    abi = m.group("abi")
    if b"\r" in abi:
        sys.exit(f"error: stray carriage return in the literal of {src}")
    try:
        doc = json.loads(abi)
    except ValueError as e:
        sys.exit(f"error: the literal in {src} is not JSON: {e}")
    variants = {v["name"]: v["types"] for v in doc.get("variants", [])}
    for name, known in (("request", REQUEST), ("result", RESULT)):
        got = variants.get(name, [])
        if got[: len(known)] != known:
            sys.exit(
                f"error: the `{name}` variant is {got}; the codec expects it to start with "
                f"{known}. Update crates/core/src/protocol.rs before vendoring this ABI."
            )
        if len(got) > len(known):
            print(
                f"note: `{name}` gained {got[len(known):]}; the codec does not decode them yet",
                file=sys.stderr,
            )
    return abi


def pinned_digest() -> str | None:
    if not PROTOCOL.exists():
        return None
    m = re.search(r'SPRING_SHIP_ABI_SHA256: &str =\s*"([0-9a-f]{64})"', PROTOCOL.read_text(encoding="utf-8"))
    return m.group(1) if m else None


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("spring", type=Path, help="Spring source checkout")
    ap.add_argument("--out", type=Path, default=VENDORED, help=f"output file (default {VENDORED.relative_to(REPO)})")
    ap.add_argument("--check", action="store_true", help="compare with the output file instead of writing it")
    args = ap.parse_args()

    abi = extract(args.spring)
    digest = hashlib.sha256(abi).hexdigest()
    print(f"{len(abi)} bytes, sha256 {digest}")

    if args.check:
        current = args.out.read_bytes() if args.out.exists() else b""
        if current != abi:
            print(f"{args.out} differs from {args.spring / SOURCE}", file=sys.stderr)
            return 1
        print(f"{args.out} is up to date")
    else:
        # Binary mode: no newline translation on any platform.
        args.out.write_bytes(abi)
        print(f"wrote {args.out}")

    pinned = pinned_digest()
    if pinned != digest:
        print(
            f"note: SPRING_SHIP_ABI_SHA256 in {PROTOCOL.relative_to(REPO)} is {pinned}; set it to {digest}",
            file=sys.stderr,
        )
    return 0


if __name__ == "__main__":
    sys.exit(main())
