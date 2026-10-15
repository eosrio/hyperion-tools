# AtomicAssets HTTP benchmark

Load: 60s steady-state per target, concurrency 100. Corpus: 200 ids / 7 owners / 5 collections / 8 (coll,schema) pairs sampled from `http://127.0.0.1:6390`. Latency = client-observed served-HTTP time (ms); req/s = successful only.

| target | req/s | p50 | p95 | p99 | max | errors | cpu avg/peak | rss avg/peak (MiB) |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| wormdb | 1489.41 | 66.82 | 77.36 | 83.00 | 163.66 | 0 | — | — |
| atomic | 138.08 | 522.13 | 1807.93 | 4620.75 | 10434.82 | 0 | — | — |

## wormdb — per query type
| type | n | p50 | p95 | p99 | max |
|---|---:|---:|---:|---:|---:|
| point | 31384 | 65.47 | 75.37 | 80.09 | 114.70 |
| coll | 22444 | 67.29 | 77.28 | 82.13 | 156.69 |
| owner | 13228 | 67.34 | 77.40 | 82.37 | 159.42 |
| faceted | 9011 | 67.18 | 77.43 | 82.16 | 156.54 |
| browse | 7177 | 67.17 | 76.86 | 81.59 | 150.63 |
| account | 6203 | 70.08 | 84.47 | 90.76 | 163.66 |

## atomic — per query type
| type | n | p50 | p95 | p99 | max |
|---|---:|---:|---:|---:|---:|
| point | 2954 | 427.28 | 747.47 | 871.59 | 4620.75 |
| coll | 2157 | 540.02 | 916.95 | 1180.08 | 4622.41 |
| owner | 1269 | 583.89 | 942.58 | 1161.12 | 3332.28 |
| faceted | 781 | 528.39 | 916.07 | 1127.79 | 2810.11 |
| browse | 670 | 525.79 | 920.53 | 1286.54 | 4269.28 |
| account | 611 | 2485.30 | 9403.52 | 9962.00 | 10434.82 |
