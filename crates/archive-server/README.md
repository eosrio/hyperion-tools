# archive-server — the tiered-storage archive

The HTTP server behind [Hyperion](https://github.com/eosrio/hyperion-history-api) v4.5 **tiered storage**.
Cold-tier ES documents drop the heavy payloads (`act.data`, `contract_row` values); the API hydrates them
on read by asking the archive, which decodes them on demand from the frozen state-history logs:

```bash
archive-server --from-disk /data/nodeos/state-history \
  --abi-index abi.ndjson --port 8088
```

## Endpoints

| endpoint | purpose |
|---|---|
| `GET /action?block_num=<N>&global_sequence=<G>` | one action's decoded `act.data` |
| `GET /block/<N>` | a block's decoded traces |
| `POST /actions` | batch-hydrate many actions in one round-trip (request order preserved) |
| `POST /deltas` | batch-hydrate many `contract_row` delta values |
| `GET /health` | status + the archived block ranges served (actions, and deltas or `null`) |

`GET /health` reports the coverage this node can serve, so integrators can discover the range instead of
probing for it — `deltas` is `null` on a node with no `chain_state_history` log:

```json
{"status":"ok","actions":{"first_block":190373745,"last_block":190374244},"deltas":{"first_block":190373745,"last_block":190374244}}
```

The batch endpoints group requested positions by block so each block is read and decoded exactly once per
request (shared per-thread cache), and process blocks in ascending order for sequential disk reads and
deterministic results under the per-request work cap. See the Hyperion **tiered-storage** docs for the full
wire contract and the API-side hydration flow.

Uint64 sequence metadata always uses canonical decimal JSON strings in `/action`, `/block` receipts
and `/actions` responses, including `found:false` entries. Send `global_sequence` as canonical
decimal text, such as `"18446744073709551615"` in a batch. The GET parameter is the same unquoted
decimal text. Signs, whitespace, leading zeroes, fractions, exponents, overflow and duplicate
metadata keys are refused. Legacy integer JSON numbers are accepted only through 9007199254740991;
their responses still use strings. Larger numbers are rejected even if their token looks exact,
because a JavaScript sender may already have rounded them.

Block numbers, ordinals and uint32 ABI/code counters remain numeric. Contract data and the existing
delta primary-key contract are unchanged. See the
[sequence JSON checks](../../docs/qualification/lossless-sequence-json.md).
