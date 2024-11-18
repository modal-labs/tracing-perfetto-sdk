//! # `tracing-perfetto-sdk-schema`: Internal crate containing the raw Perfetto proto schemata.
include!(concat!(env!("OUT_DIR"), "/perfetto.protos.rs"));
#[cfg(feature = "serde")]
mod serde_impls {
    use super::*;
    // Only contains trait impls so no need to re-export
    include!(concat!(env!("OUT_DIR"), "/perfetto.protos.serde.rs"));
}
