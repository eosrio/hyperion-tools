//! delta-proto's Wire delta producer as a library: the `delta-proto` binary and the range
//! service drive the same [`wire::DeltaWorker`], so they write the same documents.

pub mod wire;
