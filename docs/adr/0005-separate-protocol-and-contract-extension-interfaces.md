---
status: accepted
date: 2026-10-15
---

# Separate protocol adaptation from contract indexing

Hyperion 4.5's plugin architecture has distinct interfaces for chain protocol adapters and contract
indexing plugins. Protocol adapters interpret binary/ABI conventions and produce common events;
contract plugins consume those events to define projections, indexes, and domain behavior. This
separation improves maintainability by allowing protocol changes such as Wire's to be handled without
requiring ordinary contract plugins to implement the underlying protocol. The decision covers the
Node and Rust paths; packaging, process layout, and execution mechanisms remain to be defined.
