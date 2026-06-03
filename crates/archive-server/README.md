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
