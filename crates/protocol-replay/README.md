# Protocol replay

Decode raw Wire transport frames and ABI/value records through the internal Rust component
from the same operator package as Node. Select the protocol before consuming input:

```sh
cargo build --locked -p protocol-replay
target/debug/protocol-replay --protocol wire-sysio-7dede884-engineering-v1 < operations.ndjson > decoded.ndjson
```

Each line is one JSON operation. The output is `{ "result": ... }` or `{ "error": ... }`.
Any failed operation makes the process exit unsuccessfully. No checkpoint or successful
coverage marker is written. Preserve the input alongside the output when building an archive.
This tool does not write Elasticsearch or change an existing archive.

| Operation | Input fields beyond `operation` | Result |
|---|---|---|
| `decodeBundle` | `abi` advertised SHiP JSON; `hex` full result frame | Complete envelope, block, traces, deltas, decoded delta rows, framing provenance, all original bytes |
| `decodeAbi` | `hex` binary contract ABI | Lossless Wire meta-ABI including numeric table IDs, enum and protobuf metadata |
| `decodeValue` | `abi`, `typeName`, `hex` | Protocol-owned nested value decoding |
| `encodeValue` | `abi`, `typeName`, `value` | Canonical packed hex |
| `normalizeKvRow` | `abi`, `stream`, `present`, `hex`, `source` | Full physical identity, raw key/value bytes, payer, decoded row and source |
| `decodeKvRecord` | `record`, `source`, `deployments` | The above logical record with deployment-valid table/secondary mapping and decoded present-row value |
| `slotToIso` | `slot` uint32 | The selected profile's timestamp |

`source` is `{chainId, blockId, blockNum, ordinal, abiDeployment}`. `record` and deployment
fields follow the Wire package's `createWireDeploymentRegistry` contract. Positions are
inclusive `from` / exclusive `until`, including same-block ordinal transitions. Missing,
ambiguous or mismatched selections fail; there is no current/head ABI fallback. Deletes
retain complete identities and mapping even when no value bytes remain. Secondary keys stay
opaque full bytes and resolve numeric IDs through deployment metadata.

The caller supplies the canonical view; replay does not establish fork canonicality. Input
lines are bounded before allocation and binary/value traversal has the package's separate
limits. These profiles are engineering pins, described in the
[integration record](../../docs/qualification/wire-rust-provider.md).
