#![deny(clippy::all)]
#![deny(missing_docs)]
//! # `tracing-perfetto-sdk-layer`
//! A suite of tracing layers that reports traces via the C++ Perfetto SDK
//!
//! There are currently two flavors of layers:
//!
//!   * [`NativeLayer`]: Writes tracing data using Rust code, but can
//!     additionally fetch further tracing data using the Perfetto SDK, e.g. via
//!     the `traced` daemon.
//!   * [`SdkLayer`]: Writes tracing data using the C++ SDK subroutines.  The
//!     SDK then decides how/if/when it interacts with other things in the
//!     Perfetto ecosystem.
//!
//! You can easily add a layer into your existing `tracing-subscriber` stack:
//!
//! ```no_run
//! use std::{fs, thread, time};
//! use tracing_subscriber::layer::SubscriberExt as _;
//! use tracing_perfetto_sdk_layer::NativeLayer;
//!
//! let out_file = "native-layer-example.pftrace";
//! let trace_config = todo!();
//! let out_file = fs::File::create(out_file).unwrap();
//! let layer = NativeLayer::from_config(trace_config, out_file).build().unwrap();
//!
//! let subscriber = tracing_subscriber::registry().with(layer);
//!
//! tracing::subscriber::with_default(subscriber, || {
//!     tracing::info!(foo = "baz", int = 7, "hello trace!");
//!
//!     let span = tracing::info_span!("hi", foo = "bar", int = 3);
//!     let guard = span.enter();
//!     thread::sleep(time::Duration::from_secs(1));
//!     drop(guard);
//! });
//! ```
//!
//! ## Trace config
//!
//! The layer constructor expects a `TraceConfig` protobuf message to configure
//! tracing settings. This can either be supplied as a `prost`-generated struct
//! or as raw (protobuf-encoded) bytes. The raw bytes option might be useful if
//! you want to embed a config with your binary that doesn't have to change, and
//! you want to save on space/overhead/... In the end the config will
//! always be encoded to bytes anyway to be sent to the C++ SDK.
//!
//! Since the protobuf schema also implements `serde` traits, you can load/store
//! the config file in any format you like, as long as there is a `serde` codec
//! for it.  To activate the `tracing` data source, you need to add at least the
//! `rust_tracing` data source in the relevant config section (here, the config
//! is in YAML format):
//!
//! ```yaml
//! buffers:
//!   - size_kb: 1024
//! data_sources:
//!   - config:
//!       name: "rust_tracing"
//! ```
//!
//! This is a good starting point for a minimal config file.  If you want to
//! activate additional data sources, you can add them here; just remember that
//! most of them rely on the `traced daemon running on the host, and requires
//! you to use system mode (Explained further below in the section "Controlling
//! output destinations").
//!
//! ## Using counters
//!
//! When logging trace events, such as when calling `tracing::info!()` and
//! similar macros, it is possible to additionally specify counters that will be
//! reported in the trace.
//!
//! Any field within the trace event can be a counter if it starts with the
//! `counter.` prefix and has a numeric value:
//!
//! ```no_run
//! tracing::info!(counter.mem_usage=42, counter.cpu_usage=23.2, "hi!");
//! ```
//!
//! It is recommended to specify the unit of the counter as the last
//! `.`-separated part of the field:
//!
//! ```no_run
//! tracing::info!(counter.mem_usage.bytes=42, counter.cpu_usage.percent=23.2, "hi!");
//! ```
//!
//! The unit will be reported properly in the Perfetto UI at the track-level
//! next to the sidebar of each line chart.
//!
//! **NOTE**: at the time of writing, counters are only implemented by the
//! [`NativeLayer`].
//!
//! ## Suppressing events
//!
//! Sometimes, you want to log a tracing event but have it be suppressed in the
//! Perfetto trace in particular. You can set the `perfetto.suppress_event =
//! true` field for that to happen:
//!
//! ```no_run
//! tracing::info!(perfetto.suppress_event=true, "won't be included in the Perfetto trace");
//! ```
//!
//! ## Controlling output destinations
//!
//! For [`NativeLayer`], traces can be written to anything that implements the
//! [`tracing_subscriber::fmt::MakeWriter`] trait.  It might be a good idea to
//! use the [`tracing_appender::non_blocking`] output if you want to defer the
//! writing to actual disk to a background thread.
//!
//! For [`SdkLayer`], since the write operation happens in C++ code, a real file
//! with an open file descriptor must be used instead.
//!
//! ## Tokio support
//!
//! If the `tokio` feature is enabled, the layers will be aware of spans coming
//! from the `tokio` runtime vs. normal OS threads.  Spans coming from the
//! `tokio` runtime will be reported to a special `tokio-runtime` track.
//!
//! ## Forcing flavors
//!
//! The layers will report spans to Perfetto using either a sync or async
//! flavor:
//!
//!  * The sync flavor implies that spans are reported when they are entered and
//!    exited.
//!  * The async flavor implies that spans are reported using RAII semantics, ie
//!    upon construction and until they are dropped.
//!
//! By default, a heuristic is used to determine whether a span is sync or
//! async. A specific flavor can instead be forced using the
//! `.with_force_flavor` method on layer builders.
//!
//! ## In-process vs system mode
//!
//! By default, layers are configured to run in-process mode only. That means
//! that the only data sources that are consulted are the ones embedded in the
//! actual application binary.
//!
//! In addition, (or instead of) in-process mode, the system mode can be
//! activated too.  This will (in the case of [`SdkLayer`]) write traces to the
//! system's `traced` daemon, or (in the case of [`NativeLayer`]) poll for
//! system-wide traces from the `traced` daemon and include them in the trace
//! file.
//!
//! ## Sharing
//!
//! Layers are cloneable and will internally keep a reference count to keep the
//! layer internals alive.  This means you can make a clone of the layer to
//! control it from many places at once. For example, it can be useful to have a
//! clone of the layer to be able to call `.flush()` or `.stop()` from somewhere
//! else.
//!
//! ## Timeouts
//!
//! The layers allow you to configure various timeouts that will be used
//! throughout the lifecycle of a layer. These are:
//!
//!   * `drop_flush_timeout`: When the layer is dropped: how long to wait for a
//!     final flush to complete before destroying the layer.  If you want to
//!     avoid this cost during `drop()`, call `.stop()` manually before the
//!     layer is dropped.
//!   * `background_flush_timeout`: In the case of [`NativeLayer`], a background
//!     thread is used to fetch data from the SDK into the Rust world.  This
//!     controls how long we wait for each flush to complete; however, if it
//!     doesn't complete in time, another attempt will soon be made.
//!   * `background_poll_timeout`: How long to wait for new data to arrive
//!     during each attempt of the background thread.
//!   * `background_poll_interval`: How long to wait between attempts to poll
//!     for new data in the background thread.

// Internal modules:
mod debug_annotations;
mod ffi_utils;
mod ids;
mod init;

// Public modules:
pub mod error;
pub mod flavor;
pub mod native_layer;
pub mod sdk_layer;

// Convenience re-exports:
pub use error::Error;
pub use flavor::Flavor;
pub use native_layer::NativeLayer;
pub use sdk_layer::SdkLayer;
