# es-load — Elasticsearch write-ceiling benchmark

Fast, multi-threaded `_bulk` loader (real OS-thread posters) that applies Hyperion's `_id`/`_index` rules —
fast enough that ES, not the loader, is the bottleneck, so it measures the true write ceiling (typically
~3–4× slower than decode).

**Loopback-only by default** — it refuses a non-local ES target unless `BENCH_ALLOW_EXTERNAL_ES=1` is set,
so a benchmark can never accidentally hit production.

```bash
# load decoded action NDJSON into a local ES (_id/_index per the Hyperion rules)
es-load --file actions.ndjson --mode action --chain wax --workers 8 --batch 4000

# delta docs key their _id as block-code-scope-table-pk
es-load --file deltas.ndjson --mode delta --chain wax
```

(`bench/` also ships a small Python `bulk-load.py` for quick checks, but it's GIL-bound — use `es-load`
for the actual ceiling.)

Action input validates root and receipt/auth uint64 sequence metadata before deriving `_id`.
Canonical decimal strings cover the full uint64 range. Legacy integer JSON numbers are accepted
only through 9007199254740991 and are converted to strings at the known metadata paths. Malformed,
unsafe or ambiguous duplicate metadata is rejected and makes the command exit unsuccessfully;
valid rows in the same file can still be posted. Contract JSON tokens are forwarded unchanged,
including integer/float/exponent tokens. Delta payload/key behavior is unchanged.

The selected Elasticsearch generation must separately qualify full-range sequence storage and
queries under [History API #219](https://github.com/eosrio/hyperion-history-api/issues/219). JSON
validation alone does not make a legacy signed-long mapping support uint64. See the
[sequence JSON checks](../../docs/qualification/lossless-sequence-json.md).
