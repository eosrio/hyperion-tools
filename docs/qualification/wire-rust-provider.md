# Wire Rust provider integration

This is implementation evidence for [tools #41](https://github.com/eosrio/hyperion-tools/issues/41).
The full issue remains open. Selected-testnet qualification and coordinated operator distribution
remain [Wire #3](https://github.com/eosrio/hyperion-wire-plugin/issues/3) and
[tools #22](https://github.com/eosrio/hyperion-tools/issues/22).

## Exact internal artifact

The source-linking experiment consumes `hyperion-wire-protocol` from the one Wire package at
`5e94b871691f4919f060dd0a6a603f2d40e2fa36`, an exact Cargo git revision with a committed lockfile.
It is the development prerequisite in [Wire PR #5](https://github.com/eosrio/hyperion-wire-plugin/pull/5),
unmerged when this integration began. Node conformance checks check out that identical revision.
This experiment establishes ordinary Rust source integration; it does not define a public native
ABI, select dynamic loading/IPC or introduce another operator-managed plugin.

The dependency's exact serde 1.0.228 requires the scoped lock alignment of serde, serde_core and
serde_derive from 1.0.229 to 1.0.228. Existing root manifest ranges allow that alignment. Other
pre-existing dependency versions stay unchanged; added dependencies belong to the Wire codec.

`cargo +1.88.0 check --locked -p protocol-replay` currently fails because the pre-existing locked
`rs_abieos` 0.6.0 artifact requires Rust 1.95 (checksum
`1a4fb604abee4ceedfdb793b3439e9f5f1ca63fb06ac95c47be81a33854c2d0f`). The artifact is unchanged
here. Stable Rust 1.99 is the locally exercised toolchain. The declared workspace 1.88 baseline
must be reconciled separately; this slice neither silently raises it nor claims a passing 1.88
tools gate. The Wire package's own Rust-version checks remain separate evidence.

| Profile | Wire node source | Difference |
|---|---|---|
| `wire-sysio-6508c9e8-engineering-v1` | `6508c9e8f4044df8d5acb082a7af3be6a80cd9a0` | Builtin slug semantics |
| `wire-sysio-7dede884-engineering-v1` | `7dede884e0150d36fa788e85a119e10653ade8ca` | Official v1.0.0 node capture, ABI-defined slug wrapper |

Both validate the 2025-01-01 epoch and 500 ms interval. Only exact known profiles construct a
timestamp profile; altered programmatic fields fail validation. Block and chain-API timestamp
formatters share that implementation with the selected nested Wire value codec parameters.
The default Antelope 2000 epoch and existing output bodies remain unchanged.

## Affected entrypoints and current support

| Entry point | What this integration proves | Remaining mandatory work |
|---|---|---|
| `abi-scanner --ship --protocol <Wire profile>` | Provider encodes requests/ACKs from the handshake, decodes full envelopes and deltas, selects account rows by stream name and decodes the full Wire meta-ABI. Loopback WebSocket tests use the captured official-node frame. Connection/decode/writer failures now return unsuccessful completion. | A selected-testnet range with real contract deployments, restart and fork coverage |
| `protocol-replay --protocol <Wire profile>` | Complete raw frame replay, blocks/traces/deltas, nested values, full primary/secondary records, deletes and deployment/ordinal transitions, matching Node output | Feed these records into qualified history/state consumers with durable effects and recovery |
| `abi-scanner --from-disk` | Explicit Wire refusal before output/checkpoint changes | Provider-aware disk framing, bounded streaming and resumable scan of the selected Wire logs |
| `action-proto` and `delta-proto` | Explicit Wire refusal before legacy trace/account-first/contract_row paths or ES/output startup | Provider-aware disk bulk processing and lossless document schema/IDs for Wire's primary and secondary records |
| `archive-server` | Explicit Wire refusal before legacy action/table HTTP workers | Provider-aware historical ABI selection, full key lookup, payload restoration and compatible host routes |
| `antelope-ship-archive` serve/check/index | Explicit Wire refusal before fixed request/result/block layouts. A `sysio::` ABI override is also refused by the library store preflight. | Wire node block-log/genesis and SHiP record framing, request/result negotiation, block identity/hash validation, chain API bodies and archive conformance |
| `snapshot-load` | Explicit Wire refusal before opening snapshot/output/database | Wire snapshot-format support through a qualified reader/provider; the Antelope-specific snapshot is not reinterpreted |

The full raw `protocol-replay` output is the usable replacement for Wire frame decoding and
bulk semantic comparison. It does not replace the on-disk log catalog, indexer sinks or archive
server. Guard tests establish safe refusal, not successful Wire support for those pending paths.
Domain-specific MongoDB/segment prototypes do not decode Wire input here; their plugin/backend
qualification remains separate. Low-level format-specific core APIs retain their documented
Antelope contracts; new consumers must select a provider before choosing a fast path.

## Retained fixture provenance and executable checks

`crates/core/testdata/wire/` is byte-for-byte copied from the exact Wire package revision above,
including its original `SHA256SUMS`, source/build manifests, committed-ABI artifact hashes and
raw offline-clio/node captures. The files identify two different source revisions and distinguish
committed contract artifacts from regeneration. The capture records `contractDeployment:false`;
neither capture nor source-derived vectors are evidence of the final new testnet launch.

The complete bundle path calls the package's explicit, source-pinned `wire-resource-index-count-v1`
delta compatibility decoder. Generic ABI value decoding continues to reject the original malformed
count; the output records both declared/actual counts and the compatibility identity.

```sh
cargo build --locked -p protocol-replay -p abi-scanner -p action-proto -p delta-proto -p archive-server -p snapshot-load -p antelope-ship-archive
node scripts/check-wire-provider.mjs /path/to/exact-built-wire-package
node scripts/check-wire-preflight.mjs
cargo test --locked -p hyperion-ship --features ship,archive,chain-api,testkit -p abi-scanner -p antelope-ship-archive
cargo clippy --locked --all-targets -- -D warnings
```

The cross-language check compares complete objects, not selected counters or hashes: pinned
contract ABI/value families, captured envelopes/block/traces/deltas and raw rows, full compound
keys, primary/secondary deletes, payer changes, zero numeric IDs and same-block deployment changes.
Malformed data and missing/ambiguous/stale deployment mappings require unsuccessful replay. Six
CLI preflight cases verify that an existing output sentinel and absent checkpoint stay unchanged.

`wire-host-receipts.json` adds an explicitly synthetic, deterministic nonempty block/trace case
because captured block 6350 has no transactions. Its native receipt retains the per-input-action
CPU vector `[23,456]` and direct packed transaction; status and net fields remain absent. The
SHiP trace separately carries aggregate CPU, compatibility status and trace-derived net values.
It is transformed serialization input with unvalidated block roots/signatures/IDs, not a
consensus-valid or executed-contract capture. Regenerate/check it with
`node scripts/generate-wire-receipt-fixture.mjs /path/to/exact-built-wire-package --check`.

The executable engine-gap test invokes the exact Cargo.lock `rs_abieos` 0.6.0 pure Rust backend
on the retained `test_kv_sec_query` binary ABI: it returns
`AbiBinToJson("read datastream of length over by")`. This is scoped evidence that the locked
legacy meta-ABI path cannot replace the Wire provider; it is not a claim about every rs_abieos
feature or another local checkout.

Existing Antelope core fixtures, public chain-API goldens and archive conformance run alongside
the new tests. Passing these checks establishes this integration slice. It does not waive the
pending rows in the support matrix or prove retained Wire archive/recovery acceptance.
