# Rust baseline and rs_abieos 0.7.0

This build-policy slice belongs to [Tools #48](https://github.com/eosrio/hyperion-tools/issues/48).
The supported minimum is Rust 1.95; current stable remains the development compiler. The workspace
pins `rs_abieos = "=0.7.0"` with `default-features = false` and `features = ["rust-backend"]`.
No C++ backend or Node addon is introduced.

## Exact published input

The [0.7.0 registry artifact](https://crates.io/crates/rs_abieos/0.7.0) is 2,529,665 bytes, SHA-256
`07aa4bb2bea77da65de03366a33369021d23963453406ef8d7d5135c278a0f70`, as recorded in Cargo.lock.
Its `.cargo_vcs_info.json` identifies tag source
`9216217dbf4877953134f20ace5a8c9c6cdbdfbb`; the
[release](https://github.com/eosrio/rs-abieos/releases/tag/v0.7.0) declares Rust 1.95 and the pure-Rust
default. Its source differs from merge `311b9c593df6215c4af98fa0eb4f0eec4a3cedb0` only in the
changelog. The downloaded crate checksum and embedded source revision were independently verified.

Before this change, Tools `94f7a39adf1644dcb473fe602b0fa7dd528dc362` declared Rust 1.88 but locked
rs_abieos 0.6.0, checksum `1a4fb604abee4ceedfdb793b3439e9f5f1ca63fb06ac95c47be81a33854c2d0f`.
That artifact also requires Rust 1.95. An unchanged Windows checkout reproduced the failure with
`cargo +1.88.0 check --locked -p hyperion-ship -p protocol-replay`: Cargo rejected the engine's
minimum compiler. This pre-existing conflict is resolved by declaring and checking the actual
supported minimum, rather than downgrading codec behavior. The historical
[Wire integration record](wire-rust-provider.md) retains the original experiment and limitations.

## Compatibility gates

- Linux CI runs locked workspace release build/tests and all-target Clippy with Rust 1.95.0 and
  current stable. Windows checks the complete workspace and all targets on both compilers.
- Archive CI retains Windows/Linux core archive, chain-API and server conformance, now on both
  compilers, including features that are disabled by default.
- Wire integration compares complete Node/Rust results against the unchanged
  `5e94b871691f4919f060dd0a6a603f2d40e2fa36` package with Node 22 and 24 and Rust 1.95.0. It retains
  the preflight checks that reject unsupported disk/bulk/archive/snapshot paths before writes.
- Release builds retain the existing x86_64 musl, aarch64 musl and x86_64 GNU targets and now
  require the committed lockfile. Their published binary/image acceptance remains a release gate.

For focused local validation:

```sh
cargo +1.95.0 test --locked -p hyperion-ship --features ship,archive,chain-api,testkit -p abi-scanner -p antelope-ship-archive
cargo +1.95.0 check --locked --workspace --all-targets
cargo +stable test --locked --workspace
cargo +stable clippy --locked --workspace --all-targets -- -D warnings
```

## Archive reservation completion during validation

The [initial Node 22 CI job](https://github.com/eosrio/hyperion-tools/actions/runs/39730166208/job/119663132315)
passed the Node/Rust comparison and preflight checks, then exposed an unchanged archive HTTP test
race at implementation `5fc48a0b7e113e8cbb44652fc340943d1ae8d5f3`: its immediate budget assertion
saw 712 bytes after the client had read the response. The corresponding Node 24 job passed.
The raw failed-job log is retained with SHA-256
`993f3fe269f96c37e8ffcba3c7526b03f73d80b7e195fe2172ba9b7f733ab3be`.

Reading Content-Length bytes does not synchronize with Hyper dropping its final server-side
response owner. One hundred unmodified Windows stress attempts passed; a temporary 20 ms delay
at that owner's final drop reproduced the exact 712-versus-zero assertion. The corrected test
uses its existing bounded completion helper to require that reservations return to zero, and
passes under the same delayed cleanup. A diagnostic mutation retaining the reservation still
fails at the helper's 20-second deadline. All diagnostic server changes were removed; only the
test's completion assertions changed. This test race is separate from the engine update and
does not establish a reservation leak or alter production memory accounting.

The engine update preserves its Antelope name, ABI/value and historical-version APIs. New sysio
metadata support does not replace the custom Wire codec: its 2025 timestamp epoch, slug/protobuf
values, extra crypto families, complete keys and strict framing remain behind the selected provider.
The Wire Cargo revision and provider constant remain identical. SDK/Node package pins and the
TypeScript 7/Bun policy are unchanged. There is no catalog or executable upgrade triggered by this
build-policy change.

Passing these checks does not qualify native distribution, pending Rust paths in
[Tools #41](https://github.com/eosrio/hyperion-tools/issues/41), selected-testnet/fork/recovery work
in [Tools #22](https://github.com/eosrio/hyperion-tools/issues/22), or the single operator package
in [Wire #3](https://github.com/eosrio/hyperion-wire-plugin/issues/3). Actual-source system-action
decoding remains [Wire #15](https://github.com/eosrio/hyperion-wire-plugin/issues/15).
