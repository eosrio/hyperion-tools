# AtomicAssets HTTP benchmark

Load: 60s steady-state per target, concurrency 20. Corpus: 200 ids / 7 owners / 5 collections / 8 (coll,schema) pairs sampled from `http://127.0.0.1:6390`. Latency = client-observed served-HTTP time (ms); req/s = successful only.

| target | req/s | p50 | p95 | p99 | max | errors | cpu avg/peak | rss avg/peak (MiB) |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| wormdb | 1475.67 | 13.27 | 18.66 | 23.89 | 55.82 | 0 | — | — |
| atomic | 140.72 | 84.57 | 372.33 | 1251.71 | 2736.35 | 0 | — | — |

## wormdb — per query type
| type | n | p50 | p95 | p99 | max |
|---|---:|---:|---:|---:|---:|
| point | 31182 | 11.91 | 16.72 | 18.77 | 25.94 |
| coll | 21879 | 13.81 | 18.60 | 20.55 | 35.85 |
| owner | 13352 | 13.88 | 18.59 | 20.67 | 31.58 |
| faceted | 8873 | 13.71 | 18.44 | 20.45 | 38.70 |
| browse | 7081 | 13.61 | 18.08 | 20.10 | 31.85 |
| account | 6188 | 16.16 | 29.38 | 33.04 | 55.82 |

## atomic — per query type
| type | n | p50 | p95 | p99 | max |
|---|---:|---:|---:|---:|---:|
| point | 3023 | 54.84 | 196.89 | 274.56 | 374.86 |
| coll | 2173 | 87.37 | 247.84 | 327.10 | 421.35 |
| owner | 1217 | 105.85 | 268.74 | 362.11 | 482.55 |
| faceted | 874 | 89.88 | 234.81 | 310.01 | 398.26 |
| browse | 686 | 82.50 | 236.34 | 289.46 | 387.92 |
| account | 614 | 517.22 | 2076.89 | 2294.54 | 2736.35 |
