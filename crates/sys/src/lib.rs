//! # `tracing-perfetto-sdk-sys`: C++ bindings to the raw Perfetto SDK
//!
//! This crate only contains low-level bindings to the C++ library.  While the
//! interface is safe, it is recommended to use a higher level API, for example
//! via the `tracing-perfetto-sdk-layer` crate.
#![deny(clippy::all)]

use std::sync::mpsc;

#[cfg(feature = "async")]
use futures::channel::oneshot;

/// FFI bridge: Definitions of functions and types that are shared across the
/// C++ boundary.
#[cxx::bridge]
pub mod ffi {
    pub enum LogLev {
        Debug = 0,
        Info = 1,
        Important = 2,
        Error = 3,
    }

    pub struct DebugStringAnnotation {
        pub key: &'static str,
        pub value: String,
    }

    pub struct DebugBoolAnnotation {
        pub key: &'static str,
        pub value: bool,
    }

    pub struct DebugIntAnnotation {
        pub key: &'static str,
        pub value: i64,
    }

    pub struct DebugDoubleAnnotation {
        pub key: &'static str,
        pub value: f64,
    }

    pub struct DebugAnnotations<'a> {
        pub strings: &'a [DebugStringAnnotation],
        pub bools: &'a [DebugBoolAnnotation],
        pub ints: &'a [DebugIntAnnotation],
        pub doubles: &'a [DebugDoubleAnnotation],
    }

    extern "Rust" {
        // Opaque type passed to C++ code to be sent back during callbacks; essentially
        // like `void *context` but type-safe
        type PollTracesCtx;
        type FlushCtx;
    }

    unsafe extern "C++" {
        include!("src/perfetto-bindings.h");

        /// Initialize the global tracing infrastructure.
        ///
        /// Must be called once before all other functions in this module.
        fn perfetto_global_init(
            log_callback: fn(level: LogLev, line: i32, filename: &str, message: &str),
            name: &str,
            enable_in_process_backend: bool,
            enable_system_backend: bool,
        );

        /// The native C++ class that is safe to be passed across the C++/Rust
        /// boundary via a `unique_ptr`.
        type PerfettoTracingSession;

        /// Create a new tracing session using the provided Protobuf-encoded
        /// TraceConfig.
        ///
        /// If `output_fd` is non-negative, we will write traces directly to the
        /// provided file descriptor, in which case `poll_traces` will never
        /// return any data.
        fn new_tracing_session(
            trace_config_bytes: &[u8],
            output_fd: i32,
        ) -> Result<UniquePtr<PerfettoTracingSession>>;

        /// Create a trace event signifying that a new slice (aka span) begins.
        ///
        /// Make sure to call this in the hot path instead of delaying the call
        /// to some background queue, etc., as the call will collect additional
        /// timing data at the time of the actual call.
        ///
        /// This call is very cheap and will be a no-op if trace collection is
        /// not yet started/enabled.
        fn trace_track_event_slice_begin<'a>(
            track_uuid: u64,
            name: &str,
            location_file: &str,
            location_line: u32,
            debug_annotations: &DebugAnnotations<'a>,
        );

        /// Create a trace event signifying that a slice (aka span) ends.
        ///
        /// Make sure to call this in the hot path instead of delaying the call
        /// to some background queue, etc., as the call will collect additional
        /// timing data at the time of the actual call.
        ///
        /// This call is very cheap and will be a no-op if trace collection is
        /// not yet started/enabled.
        fn trace_track_event_slice_end(
            track_uuid: u64,
            name: &str,
            location_file: &str,
            location_line: u32,
        );

        /// Create a trace event signifying that an instant event has happened,
        /// similar to a log message or something else that takes ~zero time.
        ///
        /// Make sure to call this in the hot path instead of delaying the call
        /// to some background queue, etc., as the call will collect additional
        /// timing data at the time of the actual call.
        ///
        /// This call is very cheap and will be a no-op if trace collection is
        /// not yet started/enabled.
        fn trace_track_event_instant<'a>(
            track_uuid: u64,
            name: &str,
            location_file: &str,
            location_line: u32,
            debug_annotations: &DebugAnnotations<'a>,
        );

        /// Create a track descriptor for a process.
        ///
        /// Make sure to call this in the hot path instead of delaying the call
        /// to some background queue, etc., as the call will collect additional
        /// timing data at the time of the actual call.
        ///
        /// This call is very cheap and will be a no-op if trace collection is
        /// not yet started/enabled.
        fn trace_track_descriptor_process(
            parent_uuid: u64,
            track_uuid: u64,
            process_name: &str,
            process_pid: u32,
        );

        /// Create a track descriptor for a thread.
        ///
        /// Make sure to call this in the hot path instead of delaying the call
        /// to some background queue, etc., as the call will collect additional
        /// timing data at the time of the actual call.
        ///
        /// This call is very cheap and will be a no-op if trace collection is
        /// not yet started/enabled.
        fn trace_track_descriptor_thread(
            parent_uuid: u64,
            track_uuid: u64,
            process_pid: u32,
            thread_name: &str,
            thread_tid: u32,
        );

        /// Get the current trace time according to Perfetto's managed monotonic
        /// clock(s).
        fn trace_time_ns() -> u64;

        /// Get the id of the clock that is used by `trace_time_ns`.
        ///
        /// Valid values correspond to enum variants of the `BuiltinClock`
        /// protobuf enum.
        fn trace_clock_id() -> u32;

        /// Start collecting traces from all data sources.
        fn start(self: Pin<&mut PerfettoTracingSession>);

        /// Stop collecting traces from all data sources.
        fn stop(self: Pin<&mut PerfettoTracingSession>);

        /// Flush buffered traces.
        ///
        /// The passed-in callback is called with `true` on success; `false`
        /// indicates that some data source didn't ack before the
        /// timeout, or because something else went wrong (e.g. tracing
        /// system wasn't initialized).
        fn flush(
            self: Pin<&mut PerfettoTracingSession>,
            timeout_ms: u32,
            ctx: Box<FlushCtx>,
            done: fn(ctx: Box<FlushCtx>, success: bool),
        );

        /// Poll for new traces, and call the provided `done` callback with new
        /// trace records.
        ///
        /// Polling will only return data if an `output_fd` was *not* specified
        /// when creating the session.
        ///
        /// If there are no more records, the callback will be called with an
        /// empty slice and `has_more == true` as a special case to signal EOF.
        ///
        /// The data in the callback buffer is guaranteed to consist of complete
        /// trace records; in other words, there will not be any partial records
        /// that cross buffer boundaries.
        fn poll_traces(
            self: Pin<&mut PerfettoTracingSession>,
            ctx: Box<PollTracesCtx>,
            done: fn(ctx: Box<PollTracesCtx>, data: &[u8], has_more: bool),
        );
    }
}

// Safe to use session from all threads
unsafe impl Send for ffi::PerfettoTracingSession {}
unsafe impl Sync for ffi::PerfettoTracingSession {}

/// A context that will be passed-in in a call to [`ffi::poll_traces`] and later
/// passed back in the `done` callback when the async operation has completed.
pub struct PollTracesCtx(CallbackSender<PolledTraces>);

/// A context that will be passed-in in a call to [`ffi::flush`] and later
/// passed back in the `done` callback when the async operation has completed.
pub struct FlushCtx(CallbackSender<bool>);

/// Traces returned from `poll_traces`/the channel returned by
/// `PollTracesCtx::new`.
pub struct PolledTraces {
    pub data: bytes::BytesMut,
    pub has_more: bool,
}

enum CallbackSender<A> {
    Sync(mpsc::Sender<A>),
    #[cfg(feature = "async")]
    Async(Option<oneshot::Sender<A>>),
}

/// A context to be passed into `poll_traces`.
///
/// Intended to be called like:
///
/// ```no_run
/// # use std::pin::Pin;
/// # use tracing_perfetto_sdk_sys::ffi::PerfettoTracingSession;
/// # use tracing_perfetto_sdk_sys::{PollTracesCtx, PolledTraces};
/// let session: Pin<&mut PerfettoTracingSession> = todo!();
/// let (ctx, rx) = PollTracesCtx::new_sync();
/// session.poll_traces(Box::new(ctx), PollTracesCtx::callback);
/// let polled_traces: PolledTraces = rx.recv().unwrap(); // Should return polled traces
/// ```
impl PollTracesCtx {
    pub fn new_sync() -> (Self, mpsc::Receiver<PolledTraces>) {
        let (tx, rx) = mpsc::channel();
        let tx = CallbackSender::Sync(tx);
        (Self(tx), rx)
    }

    #[cfg(feature = "async")]
    pub fn new_async() -> (Self, oneshot::Receiver<PolledTraces>) {
        let (tx, rx) = oneshot::channel();
        let tx = CallbackSender::Async(Some(tx));
        (Self(tx), rx)
    }

    #[allow(clippy::boxed_local)]
    pub fn callback(mut self: Box<Self>, data: &[u8], has_more: bool) {
        let data = bytes::BytesMut::from(data);
        self.0.send(PolledTraces { data, has_more });
    }
}

/// A context to be passed into `flush`.
///
/// Intended to be called like:
///
/// ```no_run
/// # use std::pin::Pin;
/// # use tracing_perfetto_sdk_sys::ffi::PerfettoTracingSession;
/// # use tracing_perfetto_sdk_sys::FlushCtx;
/// let session: Pin<&mut PerfettoTracingSession> = todo!();
/// let (ctx, rx) = FlushCtx::new_sync();
/// let timeout_ms = 100;
/// session.flush(timeout_ms, Box::new(ctx), FlushCtx::callback);
/// let success: bool = rx.recv().unwrap();
/// ```
impl FlushCtx {
    pub fn new_sync() -> (Self, mpsc::Receiver<bool>) {
        let (tx, rx) = mpsc::channel();
        let tx = CallbackSender::Sync(tx);
        (Self(tx), rx)
    }

    #[cfg(feature = "async")]
    pub fn new_async() -> (Self, oneshot::Receiver<bool>) {
        let (tx, rx) = oneshot::channel();
        let tx = CallbackSender::Async(Some(tx));
        (Self(tx), rx)
    }

    #[allow(clippy::boxed_local)]
    pub fn callback(mut self: Box<Self>, success: bool) {
        self.0.send(success);
    }
}

impl<A> CallbackSender<A> {
    fn send(&mut self, value: A) {
        match self {
            CallbackSender::Sync(ref mut tx) => {
                let _ = tx.send(value);
            }
            #[cfg(feature = "async")]
            CallbackSender::Async(ref mut tx) => {
                if let Some(tx) = tx.take() {
                    let _ = tx.send(value);
                }
            }
        }
    }
}
