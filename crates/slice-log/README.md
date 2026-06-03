# slice-log — local test fixtures

Extracts a small, self-contained block-range slice of a real ship/block log with a **rebased index**,
read-only on the source (only ever reads committed blocks well below the head). Copy a slice off a
production node to test the direct-from-disk tools locally, with ground-truth data, free of any node
dependency.

```bash
slice-log --dir /data/nodeos/state-history --stem chain_state_history \
  --start 380000000 --count 500 --out ./fixture
```
