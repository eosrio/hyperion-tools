---
status: accepted
date: 2026-10-20
---

# Return the action usage count as an exact decimal string

For Hyperion 4.5, `/v2/stats/get_action_usage` returns `action_count` as a canonical decimal JSON
string at every magnitude, including `"0"`. The operator explicitly approved: "Always return
action_count as a decimal string" on 2026-10-20 UTC.

Compute the difference between the endpoint's exact sequence bounds with integer arithmetic before
encoding the result. Large bounds can produce a small count; do not convert either bound to a
JavaScript Number or make the response type depend on the result's size. Missing, invalid or
incomplete exact source coverage must produce an explicit error rather than an invented count.

This is an intentional public response-type change in addition to the identifier change in
[ADR 0053](0053-use-lossless-sequence-strings-on-all-chains.md). Version or invalidate cached
responses so a retained numeric `action_count` cannot bypass the new response contract. Document
the client migration and qualify both fresh and cached responses using independently expected
values at the uint64 boundaries.

This decision concerns this endpoint's derived `action_count`. It does not stringify `tx_count`,
`unique_actors`, other counts, uint32 ABI/code counters, block numbers or action ordinals, and does
not settle the unresolved legacy `parent` filter semantics. Preserve existing Elasticsearch history
and report affected precision-dependent coverage limitations under ADR 0040 and ADR 0053.

Implementation is tracked in
[eosrio/hyperion-history-api#219](https://github.com/eosrio/hyperion-history-api/issues/219), with
cross-language work in
[eosrio/hyperion-tools#51](https://github.com/eosrio/hyperion-tools/issues/51) and release coordination
in [#24](https://github.com/eosrio/hyperion-tools/issues/24). Acceptance of this contract does not
claim that implementation, retained-history coverage or Wire testnet qualification is complete.
