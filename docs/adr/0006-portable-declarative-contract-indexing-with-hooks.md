---
status: accepted
date: 2026-10-15
---

# Use portable declarative contract indexing with code hooks

Contract indexing is declarative first, with executable hooks for complex behavior. The same
definitions must work with MongoDB and WormDB, and adding or changing contract indexing through
supported definitions/hooks must not require changes to Hyperion core. This keeps contract behavior
separate from storage mechanics while leaving room for specialized decoding and domain calculations.
The definition language, hook execution model, and capability/version contracts remain to be defined.
