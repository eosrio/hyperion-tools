# Datasets

Public, periodically-refreshed snapshots produced by abi-scanner, published as **GitHub Release assets** (kept out of git history so clones stay lean).

## WAX contract-ABI history — `wax-abi.ndjson.zst`

Every contract ABI version (`setabi`) on WAX mainnet, in the Hyperion `<chain>-abi-v1` NDJSON shape:

```json
{"account":"eosio.token","block":49,"abi":"{...}","abi_hex":"0e…","actions":["transfer","…"],"tables":["accounts","stat"]}
```

- **Latest:** <https://github.com/eosrio/hyperion-tools/releases/latest/download/wax-abi.ndjson.zst> (small zstd asset, hundreds of MB decompressed)
- Each release is **stamped by the last indexed block** — tag `wax-abi-<block>` — so the tag tells you exactly where to resume.

### Download & use

```bash
curl -L -o wax-abi.ndjson.zst \
  https://github.com/eosrio/hyperion-tools/releases/latest/download/wax-abi.ndjson.zst
zstd -d wax-abi.ndjson.zst   # -> wax-abi.ndjson
```

Seed it into Hyperion's `wax-abi-v1` index, or use it as the ABI source for off-disk delta/action decoding.

### Catch up to head

The snapshot is indexed through the block in its release tag (e.g. `wax-abi-437429274` → through block 437,429,274). On your own WAX state-history node, scan only the new blocks forward — resumable and append-safe:

```bash
abi-scanner --from-disk /data/nodeos/state-history --start <last_block+1> --end 999999999 \
  --out wax-abi.ndjson --checkpoint wax-abi.ckpt
```

A week of WAX is ~1.2 M blocks, so catching up from any recent snapshot is quick.

## Refreshing (maintainers)

[`scripts/publish-wax-abi.sh`](../scripts/publish-wax-abi.sh) does it end-to-end on a host with a WAX state-history node: (re)scan/resume → clean + de-dupe → zstd → publish a release stamped by the last indexed block.
