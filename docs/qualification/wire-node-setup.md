# Isolated Wire node setup — 2026-10-19

Tracked by [tools #42](https://github.com/eosrio/hyperion-tools/issues/42) under
[Wire qualification #22](https://github.com/eosrio/hyperion-tools/issues/22).
The operator requested starting node setup before selecting final testnet launch pins.

## Running engineering baseline

The actual Wire `nodeop` binary, upstream's nodeos equivalent, is running on `x570-hyperion-1`
(`100.115.47.83`, Ubuntu 24.04.4, Ryzen 5950X). Chronos and direct host inspection preceded setup.
It uses the official portable `v1.0.0` release at node revision
`7dede884e0150d36fa788e85a119e10653ade8ca`, whose source manifest identifies CDT
`11c0599a6503437c04889510284780b078f24dad`.

The downloaded archive's SHA-256 is
`25210d3d80d72292dbb7d33cc2601832edabaeed27b6499d43f27ef3b4fe823f`;
the executed node binary's SHA-256 is
`ad275fff367f903b0a4a2a8d402f1d52e4a709ae9a4d362654b5315617db189e`.
Official checksums, source manifest, binary/version/dependency observations, config and public
genesis are retained in the [evidence directory](evidence/wire-node-20261019/summary.json).

The engineering chain ID is
`0b99f90492dab673bcf7e2c9ed1cabab6019de897a90996f00d9717b84ed89d2`.
This is a fresh empty single-producer chain with native genesis proposer/finalizer initialization.
The final Wire contract set has not been deployed; the Hyperion Wire provider is not integrated.
This candidate is not an accepted 4.5 launch pin or a working-Wire release qualification.

## Ownership and operation

| Resource | Value |
|---|---|
| Service | `hyperion-wire-node42.service`; installed and started, not enabled at boot |
| Owned root | `/srv/hyperion-wire-testnet/20261019-node-setup` |
| Binary | `runtime/wire-sysio/bin/nodeop` under that root |
| Account | Dedicated unprivileged `wire-hyperion42` |
| Configuration and state | Separate `config/` and `data/` under the owned root |
| RPC | `127.0.0.1:18888` |
| SHiP | `127.0.0.1:18080` |
| P2P | `127.0.0.1:19876`, `allowed-connection=none`, no peers configured |
| Resource bounds | 2 CPU quota cores, 4 GiB memory maximum, 512 MiB chain-state database, 8 GiB free-space guard |
| Lifetime | Automatic stop after six hours per start; `Restart=no`; not enabled at boot |

The service isolates filesystem writes to its own config/data and uses a private temporary
directory. Existing `/usr/bin/nodeos` and `/usr/bin/cleos` hashes were checked before and after
setup and restart; both remained unchanged. Existing production services were not restarted.

On the host:

```sh
systemctl status hyperion-wire-node42.service
systemctl start hyperion-wire-node42.service
systemctl stop hyperion-wire-node42.service
journalctl -u hyperion-wire-node42.service --since '10 minutes ago'
curl --fail http://127.0.0.1:18888/v1/chain/get_info
```

For workstation access, keep the listeners private and use an SSH tunnel:

```sh
ssh -N -L 18888:127.0.0.1:18888 -L 18080:127.0.0.1:18080 root@100.115.47.83
```

The node generated matching K1/BLS signature providers on first startup; their file is owner-only
mode `0600` in the owned configuration directory. Keep `config/default_signature_providers.json`
and `data/` together across restarts. Signing keys are absent from the published receipts.

## Observed acceptance and limits

- The verified binary reports `v1.0.0-7dede884e0150d36fa788e85a119e10653ade8ca` and resolves its
  runtime dependencies. This used official binaries, not a source build of current master.
- RPC reports active production/finality. The retained clean restart advances head `160 → 162`
  and LIB `158 → 160` while preserving chain identity. The service stops normally with exit zero.
- A real WebSocket receives the node's SHiP ABI, `sysio::abi/1.1`, with 78 structures. Primary
  records advertise code, payer, uint16 table ID, key and value; secondary records advertise code,
  payer, uint16 table ID and both key sequences. The capture SHA-256 is
  `c67517833717ff43d170c19a31c395afbf6d3aa347d162265eaae33731ecd060`.
- Public genesis extraction succeeds. Wire terminates that export with its specific
  `extract_genesis_state_exception`/exit `254`; the harness accepts that only after checking the
  exact exception and validating the resulting public genesis keys. This is distinct from the
  service's clean-stop result.
- The public archive retains 23 explicitly allowlisted files with per-member hashes, plus an
  outer checksum manifest. It excludes signing keys, live state and downloaded executables.

The captured ABI is evidence of the running endpoint's advertised layout. No encoded block request,
full frame/contract-value decoding, contract mutation, API/backend conformance, fork rollback,
crash/power-loss durability or throughput has been qualified by this setup.

Release `7dede884` represents `slug_name` through a uint64 structure in its `sysio.chains` ABI;
the newer engineering reference `6508c9e8` adds a built-in string-valued codec. Their captures and
codec expectations must remain revision-specific. Current source-derived Wire primitive fixtures
do not silently qualify this older executable, or the reverse.

Next, use [the implementation frontier](../roadmap/hyperion-4.5-wire-implementation.md) to select
the target source/build/CDT/contracts profile, produce matching artifact digests and node-captured
codec workloads, implement the provider paths, then run integrated live/replay/fork/recovery tests.
Changing this engineering node to the final testnet profile requires an explicit recorded transition.

Verify the retained outer checksums from the evidence directory:

```sh
sha256sum -c SHA256SUMS
```

The files preserve a dated observation, not a continuous uptime or current-head assertion.
