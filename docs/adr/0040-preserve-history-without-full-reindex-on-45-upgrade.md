---
status: accepted
date: 2026-10-17
---

# Preserve usable history without a full reindex when upgrading to 4.5

Upgrading to Hyperion 4.5 must not require a full history reindex. Keep existing Elasticsearch
history usable and resume indexing against the retained history. This is a mandatory release
requirement, including if 4.5 launches with MongoDB as its only state backend.

Initialize or rebuild new or changed state projections separately. Their initialization or backfill
requirements must not turn the upgrade into a mandatory full Elasticsearch history reindex.
Preserving existing history does not establish that every existing state projection already
satisfies the new plugin/backend contracts.

History schema, query, indexing, and plugin changes must preserve this requirement. Qualification
must demonstrate querying pre-upgrade history and resuming indexing with the retained history;
existing index-version selection alone is not evidence of compatibility.

Supported source releases/index schemas, migration mechanics, configuration conversion, rollback,
and exact acceptance scenarios remain to be defined. This decision does not by itself require
zero-downtime upgrades or establish that the upgrade path is already implemented or qualified.
