---
status: accepted direction
date: 2026-10-17
---

# Target deterministic, replay-safe projection hooks where feasible

Pursue deterministic, replay-safe projection hooks. The operator agreed with the qualification
"if possible," so feasibility, enforcement, supported coverage, and any additional launch obligation
remain to be established.

The intended contract is that the same versioned source inputs, ABI/mapping/hook revision, and
declared prior projection state produce equivalent logical projection effects during live reading,
restart recovery, and rebuilding. Uncaptured wall-clock time, randomness, or changing external
responses should not determine persisted projection results. External enrichment affecting those
results would need captured, versioned inputs or a separate path.

This direction applies to indexed projection effects; API handlers and notification delivery have
separate requirements. It does not establish an unconditional guarantee for arbitrary executable
hooks or require a universal untrusted-code sandbox for the 4.5 launch. Existing idempotency,
live/bulk logical equivalence, and fork-recovery requirements remain in force.

The SDK restrictions, captured-input mechanisms, and conformance/enforcement details remain to be
defined and qualified. The executable plugin trust model was subsequently accepted in
[ADR 0045](0045-trusted-plugins-and-operator-audit-responsibility.md).
