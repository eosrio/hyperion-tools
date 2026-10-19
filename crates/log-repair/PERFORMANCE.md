# Tail inspection performance

Measured on 2026-10-19 against a frozen copy of the real Libre mainnet block log.
This measures `recover-tail` inspection without sources or recovery output, including
whole-entry framing, header links, index comparisons and complete log/index hashes.
It does not measure the media-error `scan` command or copy-out restoration.

## Input and host

- 56,409,713,202 log bytes (52.5 GiB), 2,159,186,224 index bytes.
- Blocks 1 through 269,898,278; no framing failures or index discrepancies.
- AMD Ryzen 9 7950X, 128 GiB RAM, Ubuntu 22.04, kernel 6.1.10, ext4 on NVMe.
- Static x86_64 musl release binaries, identical release settings. Baseline source:
  `291752f5e03290744f9f7eb4385ddac6e7e1c87c`; optimized source adds 64 KiB index read-ahead.
- The live node continued running. A complete immutable prefix was copied to a new
  temporary directory; source files were only read. Copying took 181.6 seconds and
  is excluded from the scan timings.

## Results

| Run | Elapsed | Log throughput | User CPU | System CPU | Peak RSS |
|---|---:|---:|---:|---:|---:|
| Baseline, first pass | 136.55 s | 413.1 MB/s | 73.10 s | 63.33 s | 19.5 MiB |
| Buffered index | 83.94 s | 672.0 MB/s | 62.08 s | 21.71 s | 19.6 MiB |
| Baseline, repeat | 134.00 s | 421.0 MB/s | 74.04 s | 58.45 s | 20.3 MiB |

The buffered run took 37.4-38.5% less elapsed time (1.60-1.63 times the throughput)
than the two baseline passes. All three inspection objects matched exactly. Ordinary page-cache conditions were used; no host-wide cache flush
was performed. Physical read counts differed between passes. These are observed
end-to-end results on this host, not a controlled cold-disk speedup or a disk ceiling.

The baseline made about 270.8 million read calls; the buffered run about 895 thousand
(last process samples, collected once per second). Previously each block allocated
and read a separate 8-byte index slot. Read-ahead batches slots into a bounded 64 KiB
buffer while retaining every comparison. Both runs returned identical inspection
objects, including log/index hashes, ranges, failure status and discrepancy counts.
The frozen input identities, sizes and modification times remained unchanged.

Log SHA-256: `418c9147be9b74beb57b2f1cb1a59cdc47de4efb6c11942a1a46dea5feb032eb`.

Index SHA-256: `cdccfa2408075d9c92ab97cdeb352ea29a769ef977e5263e82216312cc5e7378`.

Baseline binary SHA-256: `8e7b810c933944e37b2dd506083491bcf2d4249325d6d2fff54c053bae90998b`.

Buffered binary SHA-256: `d6e210082114cdd4b9ebe59fe84585c9f2d1a611658b73819e337f59a1b0f045`.

## Reproduce

Use a stable retained log or an independent copy whose index and final entry are
complete. Build both revisions with the same target, release profile and compiler.
Run them sequentially against the same files, using a new output directory each time:

```sh
python3 crates/log-repair/tests/tail_bench.py \
  --tool /scratch/antelope-log-repair \
  --log /scratch/frozen/blocks.log \
  --out /scratch/new-benchmark-result \
  --cache-note "ordinary page cache; no global eviction"
```

The benchmark writes `run.json`, `samples.jsonl`, `report.json`, `stderr.txt` and
`result.json`. Check exit status, unchanged input stamps and matching `inspection`
objects before comparing speed. The recorder's `--out` names benchmark evidence;
no `--out` is passed to the repair tool. Keep cache conditions and background load
with the results; throughput depends on the block mix, CPU, filesystem and storage.
