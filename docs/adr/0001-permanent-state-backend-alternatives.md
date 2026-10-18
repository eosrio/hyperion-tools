---
status: accepted
date: 2026-10-15
---

# Permanently support MongoDB and WormDB for state queries

The target architecture retains MongoDB and WormDB as permanently supported alternative state backends. The
operator is actively developing WormDB and proving it in production, while retaining MongoDB as a
long-term supported option. Implementation agents must preserve both paths; backend choice does not
imply a migration toward retiring either one. Selection granularity is defined in ADR 0003; the
shared conformance contract remains to be defined in the architecture definition. ADR 0007 clarifies
that the first 4.5 release may ship with MongoDB alone if qualifying WormDB requires too much effort.
