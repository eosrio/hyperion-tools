`eos-mainnet-480385480.hex` is a packed signed block from EOS mainnet, block
480385480. Its expected block ID, obtained independently from nodeos, is
`1ca219c8aaf55cb9d7b6521f5470c667292a904996c03e190a0cd913d4714bb4`.

`ship/` holds real SHIP log entries (the 48-byte header and the payload, without the
position trailer), each named after its log kind, encoding, and block number:

- `eos-*-deflate-*`: EOS mainnet entries written compressed (zlib `78 9c`, level 6) by a
  Leap 4/5 node, at blocks 50,000,085, 100,000,002 (a 30 KB entry), and 300,000,054.
- `eos-*-stored-*`: EOS mainnet entries written by Spring as stored zlib (`78 01`): a
  65 KB trace at block 480,000,070 (two 32 KiB blocks and a remainder), and a small delta
  at block 525,000,000.
- `wax-*-deflate-*`: WAX testnet entries written compressed by the WAX Leap 5 build on a
  host with zlib 1.3.

The tests decode each payload and require the matching encoder to reproduce the entry
byte for byte.
