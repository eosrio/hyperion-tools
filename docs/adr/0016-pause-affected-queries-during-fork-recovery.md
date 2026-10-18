---
status: accepted
date: 2026-10-16
---

# Pause affected queries during fork recovery

Pause affected queries while a fork is being resolved, until a consistent canonical view is ready.
Do not continue serving the previous published branch after it is known to be orphaned, and do
not expose intermediate rollback/replay mutations.

Requests may wait within their request timeout. If recovery exceeds that timeout, return a
retryable unavailable error using the API's supported error contract. Unaffected queries and
other chains may continue serving.

This accepts a brief availability interruption for affected queries in order to preserve canonical
state correctness. The precise error status/envelope, in-flight requests, caches, subscriptions,
and cross-tier read-view rules remain to be specified. Both backends must satisfy this behavior.
