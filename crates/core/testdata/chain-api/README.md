# nodeos chain API goldens

What Spring nodes answered on the `/v1/chain` block endpoints, kept byte for byte so that the
chain API renderer, its request parsing and its error bodies (`hyperion_ship::chainapi`), and the
server's HTTP layer (`antelope-ship-archive --api`), are checked against nodeos itself:

- `crates/core/tests/chain_api_goldens.rs` parses every request of `dev-chain/requests` as the
  server parses it, answers it from the blocks of `dev-chain/blocks`, and compares the status and
  the body with nodeos's; it also renders the public block below and compares its four bodies.
- `crates/ship-archive/tests/chain_api_nodeos.rs` sends every request of `dev-chain/http`, and
  every request of `dev-chain/requests` that nodeos refused, to a server over a synthetic chain
  and compares the whole answer, status line and headers in their order included, but for the
  value of `Server`.

All of it was captured with `scripts/capture_chain_api_goldens.sh`. No file names a host, an
address other than 127.0.0.1, or a path.

## `dev-chain/`: a Spring 1.2.2 dev chain

A private single-producer chain (`PROVENANCE` has its chain id), bootstrapped with every protocol
feature, switched to Savanna at block 991 and loaded with a varied workload. Its data is
synthetic: dev keys, test accounts and memos. Captured in raw mode (`--url`, with the block logs
of a clean stop as `--blocks-dir`):

- `requests/<case>.req` and `.resp`: a raw HTTP/1.1 request and nodeos's raw answer.
  - The error matrix (G-ERR), every error body once per endpoint and mode, as their source lines
    differ: `*-body-*` (the empty-body rule), `*-len-*` (the multi-line length assert, whose
    `line_number` is the line the assert's call closes on), `*-id-*`, `*-ub-*`, `*-parse`.
  - `raw-v-*`: `block_num_or_id` values as fc converts and resolves them (numbers, signs, 32-bit
    wrapping, escapes as fc's parser reads them, raw control bytes and invalid UTF-8 echoed, ids
    of either case, short and wrong ids); `info-v-*`: `block_num` values; `raw-s-*`: bodies as
    fc's parser reads them (missing and repeated commas, duplicate keys, trailing bytes, depth);
    `hdr-incl-*`: `include_extensions` values.
  - `block-<N>-*`: `get_block_info`, `get_block_header` in both modes and `get_raw_block` of
    block N by number, and the last two by id.
- `blocks/<N>.hex`: block N as its block log stores it (a packed `signed_block`), for
  - 1 (genesis), 2, and 33 (the first protocol feature activation, header extension 0);
  - 38 (header extension 0 with six features) and 46 (the transaction activating SAVANNA);
  - 52 (new accounts with R1 keys), 85 (a zlib packed transaction), 88 (context-free data),
    90 (an R1 signature), 91 (two signatures), 279 (memos with raw control bytes);
  - 602 (a producer schedule change, header extension 1);
  - 991 (the Savanna genesis block, a finality extension with a finalizer policy), 992 (the
    first finality extension without one), 994 (the first QC, block extension 3);
  - 1802 (a finalizer policy change), 1805 (a larger QC, during that change), 2301 (a proposer
    policy change) and 3150 (Savanna blocks with transactions).

  Every receipt in these blocks is a packed transaction with status `executed` (the chain has no
  other kind); the other forms are covered by the renderer's own tests.
- `http/<case>.req` and `.resp`: what the HTTP layer answers whatever the chain holds (block
  4294967295 is never served): headers for each `Connection` and HTTP/1.0 case, any method, a
  chunked body, the 404s, `OPTIONS`, illegal request-targets and a missing or empty `Host`.

Requests nodeos reads differently from strict JSON (fractions, exponents and bare words, which
fc reads as doubles or strings) are not captured: a server of archived ranges answers them as
unparsable, a documented deviation.

## `eos-mainnet/480385480/`: the public block of `testdata/blocks`

The four bodies (`get_block_info`, `get_block_header` without and with `include_extensions`,
`get_raw_block`) of EOS mainnet block 480385480, a Savanna block with transactions, as public
Spring nodes answer them: captured in bodies mode (`--bodies`, no headers kept). Two Spring
1.2.2 and two Spring 1.2.1 nodes gave the same bytes. The block itself is
`../blocks/eos-mainnet-480385480.hex`.
