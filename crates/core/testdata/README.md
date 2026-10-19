Fixtures for the core crate's tests.

`blocks/eos-mainnet-480385480.hex` is a packed signed block from EOS mainnet, block
480385480, copied from `crates/log-repair/testdata/`. Its expected block ID, obtained
independently from nodeos, is
`1ca219c8aaf55cb9d7b6521f5470c667292a904996c03e190a0cd913d4714bb4`. The `blocklog` tests
require `block_header` to reproduce that ID.
