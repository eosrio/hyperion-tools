# hyperion-ship — the SHiP read + decode core

The shared library every binary in this workspace builds on. It provides the Antelope
**State-History (SHiP)** read + decode core:

- the parallel **direct-from-disk** state-history reader,
- the zero-copy **trace / delta hand-walk decoders**,
- the **block-log reader**, and
- **ABI extraction**,

all on the pure-Rust [`rs_abieos`](https://crates.io/crates/rs_abieos) backend — no C++/clang toolchain.

The zero-copy state-history deserializer at its core originated in EOS Rio's
[fleet-router](https://github.com/eosrio/fleet-router).

This crate is a library (`hyperion-ship`); it produces no binary. See the workspace
[README](../../README.md) for the tools that consume it.
