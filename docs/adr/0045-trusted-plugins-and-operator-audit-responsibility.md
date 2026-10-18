---
status: accepted
date: 2026-10-17
---

# Treat executable plugins as trusted code audited by the operator

Hyperion 4.5 treats operator-installed executable plugins, including projection hooks, as trusted
code. Operators are responsible for auditing the plugins they choose to use. A hardened sandbox
for running untrusted plugins is follow-up work, not an additional 4.5 launch requirement.

Shared SDK/interface contracts, compatibility and capability checks, conformance/replay
qualification, contract-failure isolation, and the accepted correctness requirements still apply.
This trust model does not permit supported portable projections to bypass their backend interfaces
or waive EOS Rio's first-party plugin maintenance and tested-version obligations.

Automatically mapping an allowed contract account does not authorize installing or executing
arbitrary off-chain plugin code. Contract data remains input to validate. Trusted executable code
is distinct from data supplied by indexed contracts and from a guarantee that arbitrary code is
confined by a security sandbox.

Exact process placement, resource isolation, SDK restrictions, and qualification mechanisms remain
open. The qualified replay-determinism target in
[ADR 0044](0044-target-deterministic-replay-safe-projection-hooks.md) retains its feasibility limits.
