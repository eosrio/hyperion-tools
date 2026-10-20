# Rust sequence JSON boundaries

This implements the Rust JSON portion of [ADR 0053](../adr/0053-use-lossless-sequence-strings-on-all-chains.md)
under [Tools #51](https://github.com/eosrio/hyperion-tools/issues/51) and
[History API #219](https://github.com/eosrio/hyperion-history-api/issues/219). It does not complete
the retained-history, Elasticsearch, history-query, cache/stream or Wire testnet gates.

Binary trace/log formats and exact internal `u64` identities are unchanged. Known global, receiver
and authorization sequence fields are rendered as canonical decimal JSON strings. The rule is
chain-independent, including small values; no broad conversion of contract data is applied.
Block numbers, action ordinals and uint32 ABI/code counters keep numeric JSON types.

The shared core parser accepts only ASCII base-ten canonical uint64 text. Archive GET requests
reject noncanonical text; batch requests accept that text plus explicitly supported legacy integer
JSON numbers at most 9007199254740991. Responses always use strings. Unsafe numeric tokens are
refused even though Rust can represent them, because a JavaScript caller may have rounded their
identity before transmission. Duplicate request-metadata keys and repeated sequence GET parameters
are refused. Delta primary-key semantics are separate and remain unchanged.

Canonical uint64 parsing includes zero. This slice preserves the Rust tools' existing zero source
semantics; it does not relax positive committed-action checks in History API callers.

The NDJSON loader applies the same sequence validation to root and receipt/auth metadata, derives
the exact `_id`, and normalizes safe legacy numbers only at those known paths. Raw JSON keeps all
other values opaque, including contract integer, float and exponent tokens. Duplicate metadata keys
are refused before deriving an ID. Rejected documents are counted and cause a failing exit status;
valid documents in that run may still have been posted.

Tests exercise independent binary trace fixtures through the actual disk worker and archive HTTP
handlers, with literal neighboring values at 2^53, 2^63 and uint64 maximum. They check the public
batch order and exact found/missing identities, rejection of malformed/unsafe requests, and numeric
uint32/ordinal/block fields. A public loader CLI test verifies rejected rows cause failure without
an HTTP request. `scripts/check-sequence-json.mjs` independently parses actual emitted JSON in Node
and compares its strings to its own literal boundary list; it does not reproduce a Rust encoder.

```sh
cargo test -p hyperion-ship --all-features -p action-proto -p archive-server -p es-load --locked
node scripts/check-sequence-json.mjs
```

CI runs the Node 22/24 consumers against release-mode Rust tests alongside the normal Rust gates.
This is JSON interoperability evidence. Synthetic Antelope traces do not qualify Wire binary decoding,
native protocol providers, full-range Elasticsearch storage or a live Wire testnet. Those required
release outcomes remain tracked independently.
