Real state-history log entries, copied unchanged from `crates/log-repair/testdata/ship/`
(the core does not depend on that crate). Each file is one entry's 48-byte header and its
payload, without the position trailer, named after its log kind, encoding, and block number:

- `eos-*-deflate-*`: EOS mainnet entries written compressed (zlib `78 9c`, level 6) by a
  Leap 4/5 node, at blocks 50,000,085, 100,000,002 (a 30 KB entry), and 300,000,054.
- `eos-*-stored-*`: EOS mainnet entries written by Spring as stored zlib (`78 01`): a
  65 KB trace at block 480,000,070 (a full 32 KiB stored block and a second one of the
  remainder), and a small delta at block 525,000,000.
- `wax-*-deflate-*`: WAX testnet entries written compressed by the WAX Leap 5 build.

All carry the Type A preamble (`1`, then the u64 uncompressed size). The `shiplog` tests
wrap each in a one-entry log with its index, read it back through the strict reader, and
require the stored entries to re-encode byte for byte with the fixture writer's Spring
encoding.
