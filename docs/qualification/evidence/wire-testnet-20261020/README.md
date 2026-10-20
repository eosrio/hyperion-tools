# Actual Wire testnet source and Node codec evidence

See the [qualification record](../../wire-testnet-20261020.md) for observations and limits.
This is a bounded read-only preflight and subsequent offline Node component check. Initial state
completeness, canonical ABI bootstrap, native workers, Rust, API and release acceptance remain open.

- `wire-testnet-h3-preflight-20261020.tar.gz` is the unchanged 84,081-byte original archive,
  SHA-256 `c3faa29f3980ab7309e5648177c5637dcda58543a3f7086f4eec9cffa409fcbb`.
- `evidence-manifest.json` hashes every archived member other than itself. The archive contains
  44 ordinary members, including that manifest, and 32 detached original byte captures. It contains
  only the selected public configuration, genesis, RPC/SHiP bytes, read-only scripts and preflight
  report. Signing keys, process environment, full private configuration, live databases and node
  executables are excluded.
- `ship-preflight-20261020.json` is byte-identical to the archived capture, retained separately
  for the offline regression. Its SHA-256 is
  `b8edf652dd0ad265917bf0111ca48789ec7a3f1976f2113783202cc3dd38acb4`.
- `installed-codec-node22.json` and `installed-codec-node24.json` retain the measured Windows native
  runtime checks. `component-probe-original.mjs` preserves that original local probe, including its
  historical workspace paths; use the portable script below for reproduction.
- `protocol-node-artifact.json` preserves all 68 exact component file hashes and source revision.
  Fresh Git builds may have a different source-origin metadata field; their complete file list and
  file hashes must still match. `verification.json` records the parent's independent artifact,
  archive, member and report verification.

The original preflight note intentionally retains its incomplete official-vector view. The later
Node reports show that the existing source-pinned compatibility path consumes the ninth physical
stream and all 23 rows without changing the original capture. This does not certify complete
initialization or turn absent block-1 traces into a fabricated empty trace section.

From the repository root:

```sh
python scripts/check-wire-testnet-evidence.py
node scripts/check-wire-testnet-capture.mjs /path/to/exact-built-hyperion-wire-plugin
```

From this directory, `sha256sum -c SHA256SUMS` verifies the retained outer files. All evidence is
marked binary in Git attributes so the recorded raw bytes survive cross-platform checkout.
