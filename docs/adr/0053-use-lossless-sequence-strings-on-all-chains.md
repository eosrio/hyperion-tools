---
status: accepted
date: 2026-10-20
---

# Use exact decimal strings for uint64 sequence identifiers on all chains

For Hyperion 4.5, represent uint64 sequence identifiers as canonical decimal JSON strings on every
supported chain, including Antelope and Wire. Apply this to global and receiver sequences,
authorization sequences and their public endpoint aliases. Queries, cache entries, streams and
archive exchanges must preserve the same exact values and comparison semantics.

The operator approved this direction on 2026-10-20 UTC and explicitly included Antelope in response
to a Wire-only proposal. This is an intentional public response-type change for the 4.5 upgrade;
the proposal to preserve numeric Antelope sequence responses is superseded. It does not establish
that any particular live chain has already exceeded JavaScript's safe integer range.

Canonical text uses ordinary base-ten digits without signs, whitespace, exponents, fractions or
leading zeroes, except the value `0`. Validate the uint64 range and each field's own source rules.
Do not reconstruct an exact value from an unsafe JavaScript Number. Preserve the original source
identity before parsing, grouping, sorting, caching or JSON serialization. Endpoint-specific receipt
or authorization tuple semantics and positive committed-action requirements still apply.

This decision does not implicitly change uint32 code/ABI counters, block numbers, action ordinals
or unrelated counts to strings. Overflow behavior for derived counts and the ambiguous legacy
`parent` filter require their own concrete contract; sequence-string approval does not settle them.

Keep existing Elasticsearch history usable without a mandatory full reindex under ADR 0040. Use
additive/versioned storage, exact metadata or qualified retained inputs to recover precision where
possible. Rounded retained numbers alone do not prove their original identity. Missing exact
coverage leaves only the affected precision-dependent query scope explicitly incomplete; unrelated
history and indexing continue. Do not coerce, clamp, ignore overflow or silently return rounded values.

Qualify writes, exact/range filters, counts, tuple matching, sorting, pages/PIT, caches, streams and
Node/Rust/archive boundaries, including neighboring values at 2^53, 2^63 and 2^64-1 and mixed old/new
partitions. Document client migration and preserve exact sort/cursor tokens. Physical Elasticsearch
field choices and migration mechanisms remain implementation decisions requiring real evidence.

Implementation and release qualification are tracked in
[eosrio/hyperion-history-api#219](https://github.com/eosrio/hyperion-history-api/issues/219), under
[release coordination #24](https://github.com/eosrio/hyperion-tools/issues/24). This accepted contract
is not a claim that those changes or retained-history qualification are implemented.
