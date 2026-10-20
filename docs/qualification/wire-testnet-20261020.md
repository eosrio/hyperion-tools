# Actual Wire testnet input qualification — 2026-10-20

The operator reported that the new testnet and its synced SHiP node are available on Hyperion-3.
Direct observation confirms an advancing node and supplies immutable real genesis-block inputs.
This advances [Tools #22](https://github.com/eosrio/hyperion-tools/issues/22) and
[History API #212](https://github.com/eosrio/hyperion-history-api/issues/212), coordinated through
[release #24](https://github.com/eosrio/hyperion-tools/issues/24).

This record identifies the observed running network. It does not select unverified contract/CDT
artifacts or establish the working-Wire release acceptance required by
[ADR 0032](../adr/0032-qualify-wire-on-new-testnet-with-wire-contracts.md).

## Observed source identity

| Input | Exact observation |
|---|---|
| Host | Hyperion-3 (`x570-hyperion-3`), Ubuntu 24.04.4, Ryzen 5950X |
| Reported node version | `v1.0.0-6508c9e8f4044df8d5acb082a7af3be6a80cd9a0` |
| Running node binary SHA-256 | `c556257473bb460278208bd081cf370511ec9f1705a9c55d7535aa3a469d0fd0` |
| Matching official converter SHA-256 | `7485be43e7fd441f6a6e52f57b14f8859a768a59ae1226f80fbaad3c906f298e` |
| Chain ID | `9ccd94e823fda51363bd4265ad618a909c4985c445078c79f8d030fe9697b71d` |
| Public genesis file SHA-256 | `a96c0f376ee4f5e5c36e885fe740a1e4b0f22548854457a1f374c2ab68deab47` |
| Selected nonsecret config SHA-256 | `b293744ead6133926e5c92838394e6ca7de125793f6fb46b5d638c6bacb20b63` |
| Captured SHiP schema SHA-256 | `94c5661306467f8c49f22180213e390db10d290be9ebf512fb63314f440ef6f9` |
| Existing codec profile | `wire-sysio-6508c9e8-engineering-v1` |
| Node component source revision | `8b7d8a39f098bde77fa8d8f22a20a4fffc3e457b` |

The node digest is bound to its actual RPC full version and process/listener identity. Reproducible
build provenance and CDT inputs remain pending. The config hash covers an explicit public allowlist,
not the complete configuration. The genesis hash covers the public JSON file; no independent packed
genesis-to-chain-ID computation is claimed. The existing codec profile retains its engineering name;
observing the same source revision does not promote the package to a release support declaration.

Two original RPC responses at 14:51:41–43 UTC show head `134388 → 134391` and LIB `134386 → 134389`.
SHiP status at 14:53:37 UTC reports head `134619`, LIB `134617`, traces `[2,134620)` and state
`[1,134620)`. These are dated observations rather than continuous sync or retention guarantees.

## Original block-1 input and existing codec behavior

One bounded irreversible-only request fetched `[1,2)` with one message in flight, block/deltas
enabled and traces disabled. The returned block ID matches independent RPC:
`000000012fdf78889e4c7210b63f56607f294a9b9d90058c053a218103192187`.
The block section is 120 bytes and has the actual zero predecessor. The delta section is 3,474
bytes. Traces are absent as requested; the status also advertises no retained block-1 trace input.
Exactly one result was acknowledged before closing. No source service or configuration was changed.

The official converter's declared `table_delta[]` view contains eight streams. Re-encoding that
view reproduces a 3,320-byte prefix and leaves 154 original bytes. The frozen preflight report
preserves this result and all original bytes. A subsequent offline check of the existing installed
Node component consumes all nine physical streams through its already implemented, source-pinned
`wire-resource-index-count-v1` compatibility path. No product change or source-byte rewrite was
needed. The compatibility path requires the selected profile, complete consumption and the expected
ordered stream identities; generic trailing bytes are not accepted.

| Decoded stream | Rows |
|---|---:|
| account | 3 |
| account_metadata | 1 |
| global_property | 1 |
| protocol_state | 1 |
| permission | 9 |
| resource_limits | 3 |
| resource_usage | 3 |
| resource_limits_state | 1 |
| resource_limits_config | 1 |
| Total | 23 |

Native Node 22.23.3 and 24.19.0 pass the offline check with all 68 component file hashes verified
and zero socket connection attempts. Requests re-encode to their original bytes; result envelopes
match the official converter; raw result bytes are retained unchanged. The Wire codec's documented
ABI `bool` representation uses uint8 carriers: only the four schema-declared boolean request inputs
are normalized for semantic comparison, and both forms must produce the original encoded request.

The initial account input includes `sysio`, `sysio.null` and `sysio.prods`; only `sysio` carries a
nonempty native ABI. This is a source-backed bootstrap candidate, pending initialization semantics
and canonical ABI coverage validation. Current-head `sysio` ABI/code availability was also observed
and retained; those RPC values do not authorize a historical ABI seed.

## Reproduction and remaining acceptance

The [evidence directory](evidence/wire-testnet-20261020/README.md) preserves the original archive,
manifest, capture and measured component reports. Verify provenance and rerun against the exact
built Wire package:

```sh
python scripts/check-wire-testnet-evidence.py
node scripts/check-wire-testnet-capture.mjs /path/to/exact-built-hyperion-wire-plugin
```

The latter requires source revision `8b7d8a39f098bde77fa8d8f22a20a4fffc3e457b`, all 67 other
component file hashes, and the generated provenance receipt with all source/output hashes unchanged.
Only its enumerated Git-checkout versus npm-archive origin label may differ. The original installed
component's complete 68-file hash record remains unchanged. The check performs no network access.
CI rebuilds that exact revision separately
for Node 22 and 24. This is a Node codec regression over actual network input, not a worker run,
performance result, complete state inventory, canonical ABI frontier or Rust acceptance.

Next, validate the initial state and ABI inventory against the pinned genesis/delta writer, then
capture a bounded contiguous irreversible range from block 2 with actual deployment inputs. The
shared runtime must establish genuine initialization coverage and durable progress rather than
substituting an empty checkpoint or a current-head ABI. Finish source/CDT/WASM/ABI/deployment pins
for the selected Wire contracts. Applicable native Node/Rust worker, state/API, restart, live/bulk,
fork/reconnect, backpressure and recovery exercises remain open under the original release gates.

The older `7dede884` isolated engineering-chain results remain separate dated evidence. In
particular, History API PRs #236/#239 qualify that earlier application corpus; this new preflight
does not relabel those runs as actual-testnet worker acceptance.
