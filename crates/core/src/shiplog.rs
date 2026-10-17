//! Strict, index-addressed reader for state-history logs (`trace_history`,
//! `chain_state_history`, `finality_data_history`). Every read goes through the `.index`, so
//! orphaned fork entries are never seen, and any inconsistency fails closed.
