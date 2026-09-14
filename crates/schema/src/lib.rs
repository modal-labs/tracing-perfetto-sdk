//! # `tracing-perfetto-sdk-schema`: Internal crate containing the raw Perfetto proto schemata.
// This crate is generated in its entirety by `prost` and `pbjson` from
// upstream Perfetto protos. Clippy findings here are reports on Google's
// comment formatting and field naming, none of which we can act on, and the
// set of them changes with every clippy release.
#![allow(clippy::all)]
include!(concat!(env!("OUT_DIR"), "/perfetto.protos.rs"));
#[cfg(feature = "serde")]
mod serde_impls {
    use super::*;
    // Only contains trait impls so no need to re-export
    include!(concat!(env!("OUT_DIR"), "/perfetto.protos.serde.rs"));
}
