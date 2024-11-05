//! The main tracing layer and related utils exposed by this crate.
use std::sync::atomic;
use std::{borrow, env, fs, process, sync, thread, time};

#[cfg(feature = "tokio")]
use tokio::task;
use tracing::span;
use tracing_perfetto_sdk_schema as schema;
use tracing_perfetto_sdk_sys::ffi;
use tracing_subscriber::{layer, registry};

use crate::{debug_annotations, error, ffi_utils, flavor, ids, init};

/// A layer to be used with `tracing-subscriber` that forwards collected spans
/// to the Perfetto SDK via the C++ API.
#[derive(Clone)]
#[repr(transparent)]
pub struct SdkLayer {
    inner: sync::Arc<Inner>,
}

/// A builder for [`SdkLayer`]; use [`SdkLayer::from_config`] or
/// [`SdkLayer::from_config_bytes`] to create a new instance.
pub struct Builder<'c> {
    config_bytes: borrow::Cow<'c, [u8]>,
    output_file: Option<fs::File>,
    drop_flush_timeout: time::Duration,
    enable_in_process: bool,
    enable_system: bool,
}

struct ThreadLocalCtx {
    descriptor_sent: atomic::AtomicBool,
}

struct Inner {
    // Mutex is only held during start and stop, never otherwise
    ffi_session: sync::Mutex<Option<cxx::UniquePtr<ffi::PerfettoTracingSession>>>,
    output_file: sync::Mutex<Option<fs::File>>,
    drop_flush_timeout: time::Duration,
    process_track_uuid: ids::TrackUuid,
    process_descriptor_sent: atomic::AtomicBool,
    #[cfg(feature = "tokio")]
    tokio_descriptor_sent: atomic::AtomicBool,
    #[cfg(feature = "tokio")]
    tokio_track_uuid: ids::TrackUuid,
    thread_local_ctxs: thread_local::ThreadLocal<ThreadLocalCtx>,
}

impl SdkLayer {
    /// Create a new layer builder from the supplied proto config.
    ///
    /// The proto config is usually read from an external source using the
    /// prototext syntax, or else using one of the `serde` codecs.  The config
    /// will internally be encoded to bytes straight away, so prefer
    /// [`NativeLayer::from_config_bytes`] if you already have the byte
    /// representation.
    ///
    /// The built layer will write traces to the supplied output file.  If no
    /// file is specified, it is probably only useful to run in system mode, so
    /// that the traces are sent to the `traced` daemon instead (else they would
    /// just get lost).
    pub fn from_config(
        config: schema::TraceConfig,
        output_file: Option<fs::File>,
    ) -> Builder<'static> {
        use prost::Message as _;
        Builder::new(config.encode_to_vec().into(), output_file)
    }

    /// Create a new layer builder from the supplied proto config bytes.
    ///
    /// The proto config bytes needs to be an already encoded message of type
    /// [`schema::TraceConfig`].
    ///
    /// The built layer will write traces to the supplied output file.  If no
    /// file is specified, it is probably only useful to run in system mode, so
    /// that the traces are sent to the `traced` daemon instead (else they would
    /// just get lost).
    pub fn from_config_bytes(config_bytes: &[u8], output_file: Option<fs::File>) -> Builder {
        Builder::new(config_bytes.into(), output_file)
    }

    fn build(builder: Builder) -> error::Result<Self> {
        use std::os::fd::AsRawFd as _;

        // Shared global initialization for all layers
        init::global_init(builder.enable_in_process, builder.enable_system);

        let fd = builder
            .output_file
            .as_ref()
            .map(|f| f.as_raw_fd())
            .unwrap_or(-1);
        // We send the config to the C++ code as encoded bytes, because it would be too
        // annoying to have some sort of shared proto struct between the Rust
        // and C++ worlds
        let mut ffi_session = ffi::new_tracing_session(builder.config_bytes.as_ref(), fd)?;
        ffi_session.pin_mut().start();
        let ffi_session = sync::Mutex::new(Some(ffi_session));

        let output_file = sync::Mutex::new(builder.output_file);
        let drop_flush_timeout = builder.drop_flush_timeout;

        let process_track_uuid = ids::TrackUuid::for_process(process::id());
        let process_descriptor_sent = atomic::AtomicBool::new(false);
        #[cfg(feature = "tokio")]
        let tokio_descriptor_sent = atomic::AtomicBool::new(false);
        #[cfg(feature = "tokio")]
        let tokio_track_uuid = ids::TrackUuid::for_tokio();
        let thread_local_ctxs = thread_local::ThreadLocal::new();

        let inner = sync::Arc::new(Inner {
            ffi_session,
            output_file,
            drop_flush_timeout,
            process_track_uuid,
            process_descriptor_sent,
            #[cfg(feature = "tokio")]
            tokio_descriptor_sent,
            #[cfg(feature = "tokio")]
            tokio_track_uuid,
            thread_local_ctxs,
        });
        Ok(Self { inner })
    }

    fn ensure_context_known(&self) {
        self.ensure_process_known();
        self.ensure_thread_known();
        #[cfg(feature = "tokio")]
        self.ensure_tokio_runtime_known();
    }

    fn ensure_process_known(&self) {
        let process_descriptor_sent = self
            .inner
            .process_descriptor_sent
            .fetch_or(true, atomic::Ordering::Relaxed);

        if !process_descriptor_sent {
            let process_name = env::current_exe()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned();

            let process_name = process_name.rsplit("/").next().unwrap_or("");

            ffi::trace_track_descriptor_process(
                0,
                self.inner.process_track_uuid.as_raw(),
                process_name,
                process::id(),
            );
        }
    }

    fn ensure_thread_known(&self) {
        let thread_local_ctx = self.inner.thread_local_ctxs.get_or(|| ThreadLocalCtx {
            descriptor_sent: atomic::AtomicBool::new(false),
        });
        let thread_descriptor_sent = thread_local_ctx
            .descriptor_sent
            .fetch_or(true, atomic::Ordering::Relaxed);
        if !thread_descriptor_sent {
            let tid = thread_id::get();
            ffi::trace_track_descriptor_thread(
                self.inner.process_track_uuid.as_raw(),
                ids::TrackUuid::for_thread(tid).as_raw(),
                process::id(),
                thread::current().name().unwrap_or(""),
                thread_id::get() as u32,
            );
        }
    }

    #[cfg(feature = "tokio")]
    fn ensure_tokio_runtime_known(&self) {
        // Bogus thread ID; this is unlikely to ever be an actually real thread ID.
        const TOKIO_THREAD_ID: u32 = (i32::MAX - 1) as u32;

        let tokio_descriptor_sent = self
            .inner
            .tokio_descriptor_sent
            .fetch_or(true, atomic::Ordering::Relaxed);
        if !tokio_descriptor_sent {
            ffi::trace_track_descriptor_thread(
                self.inner.process_track_uuid.as_raw(),
                self.inner.tokio_track_uuid.as_raw(),
                process::id(),
                "tokio-runtime",
                TOKIO_THREAD_ID,
            );
        }
    }

    fn pick_trace_track(&self) -> (ids::TrackUuid, flavor::Flavor) {
        #[cfg(feature = "tokio")]
        if task::try_id().is_some() {
            return (self.inner.tokio_track_uuid, flavor::Flavor::Async);
        }

        (
            ids::TrackUuid::for_thread(thread_id::get()),
            flavor::Flavor::Sync,
        )
    }

    /// Flush internal buffers, making the best effort for all pending writes to
    /// be visible on this layer's `output_file`.
    pub fn flush(&self, timeout: time::Duration) -> error::Result<()> {
        self.inner.flush(timeout)
    }

    /// Stop the layer and stop collecting traces.
    pub fn stop(&self) -> error::Result<()> {
        self.inner.stop()
    }
}

impl<S> tracing_subscriber::Layer<S> for SdkLayer
where
    S: tracing::Subscriber,
    S: for<'a> registry::LookupSpan<'a>,
{
    fn on_new_span(&self, attrs: &span::Attributes<'_>, id: &span::Id, ctx: layer::Context<'_, S>) {
        let span = ctx.span(id).expect("span to be found (this is a bug)");

        let (track_uuid, flavor) = self.pick_trace_track();
        let meta = span.metadata();

        let mut debug_annotations = debug_annotations::FFIDebugAnnotations::default();
        attrs.record(&mut debug_annotations);

        if flavor == flavor::Flavor::Sync {
            self.ensure_context_known();
            ffi::trace_track_event_slice_begin(
                track_uuid.as_raw(),
                meta.name(),
                meta.file().unwrap_or_default(),
                meta.line().unwrap_or_default(),
                &debug_annotations.as_ffi(),
            );
        }

        span.extensions_mut().insert(debug_annotations);
    }

    fn on_record(&self, id: &tracing::Id, values: &span::Record<'_>, ctx: layer::Context<'_, S>) {
        let span = ctx.span(id).expect("span to be found (this is a bug)");

        let mut extensions = span.extensions_mut();
        if let Some(debug_annotations) =
            extensions.get_mut::<debug_annotations::FFIDebugAnnotations>()
        {
            values.record(debug_annotations);
        } else {
            let mut debug_annotations = debug_annotations::FFIDebugAnnotations::default();
            values.record(&mut debug_annotations);
            extensions.insert(debug_annotations);
        }
    }

    fn on_event(&self, event: &tracing::Event<'_>, _ctx: layer::Context<'_, S>) {
        self.ensure_context_known();

        let mut debug_annotations = debug_annotations::FFIDebugAnnotations::default();
        event.record(&mut debug_annotations);

        if !debug_annotations.suppress_event() {
            let meta = event.metadata();
            let (track_uuid, _) = self.pick_trace_track();
            ffi::trace_track_event_instant(
                track_uuid.as_raw(),
                meta.name(),
                meta.file().unwrap_or_default(),
                meta.line().unwrap_or_default(),
                &debug_annotations.as_ffi(),
            );
        }
    }

    fn on_enter(&self, id: &span::Id, ctx: layer::Context<'_, S>) {
        let span = ctx.span(id).expect("span to be found (this is a bug)");

        let (track_uuid, flavor) = self.pick_trace_track();
        let meta = span.metadata();

        if flavor == flavor::Flavor::Async {
            self.ensure_context_known();
            ffi::trace_track_event_slice_begin(
                track_uuid.as_raw(),
                meta.name(),
                meta.file().unwrap_or_default(),
                meta.line().unwrap_or_default(),
                &span
                    .extensions()
                    .get()
                    .unwrap_or(&debug_annotations::FFIDebugAnnotations::default())
                    .as_ffi(),
            );
        }
    }

    fn on_exit(&self, id: &tracing::Id, ctx: layer::Context<'_, S>) {
        let span = ctx.span(id).expect("span to be found (this is a bug)");

        let meta = span.metadata();
        let (track_uuid, flavor) = self.pick_trace_track();

        if flavor == flavor::Flavor::Async {
            ffi::trace_track_event_slice_end(
                track_uuid.as_raw(),
                meta.name(),
                meta.file().unwrap_or_default(),
                meta.line().unwrap_or_default(),
            );
        }
    }

    fn on_close(&self, id: tracing::Id, ctx: layer::Context<'_, S>) {
        let span = ctx.span(&id).expect("span to be found (this is a bug)");

        let meta = span.metadata();
        let (track_uuid, flavor) = self.pick_trace_track();
        if flavor == flavor::Flavor::Sync {
            ffi::trace_track_event_slice_end(
                track_uuid.as_raw(),
                meta.name(),
                meta.file().unwrap_or_default(),
                meta.line().unwrap_or_default(),
            );
        }
    }
}

impl Inner {
    fn flush(&self, timeout: time::Duration) -> error::Result<()> {
        ffi_utils::with_session_lock(&self.ffi_session, |session| {
            ffi_utils::do_flush(session, timeout)
        })
    }

    fn stop(&self) -> error::Result<()> {
        use std::io::Write as _;

        let mut ffi_session = self
            .ffi_session
            .lock()
            .map_err(|_| error::Error::PoisonedMutex)?;
        if let Some(mut ffi_session) = ffi_session.take() {
            ffi_session.pin_mut().stop();
        }

        let mut output_file = self
            .output_file
            .lock()
            .map_err(|_| error::Error::PoisonedMutex)?;
        if let Some(mut output_file) = output_file.take() {
            output_file.flush()?;
        }

        Ok(())
    }
}

impl Drop for Inner {
    fn drop(&mut self) {
        let _ = self.flush(self.drop_flush_timeout);
        let _ = self.stop();
    }
}

impl<'c> Builder<'c> {
    fn new(config_bytes: borrow::Cow<'c, [u8]>, output_file: Option<fs::File>) -> Self {
        Self {
            config_bytes,
            output_file,
            drop_flush_timeout: time::Duration::from_millis(100),
            enable_in_process: true,
            enable_system: false,
        }
    }

    /// Enable in-process collection, where traces will be collected by buffers
    /// in the Perfetto SDK and spilled to file in-process.
    pub fn with_enable_in_process(mut self, enable_in_process: bool) -> Self {
        self.enable_in_process = enable_in_process;
        self
    }

    /// Enable system collection, where traces will be sent/collected from the
    /// `traced` daemon, and additional system-wide data sources (such as
    /// `ftrace`, `procfs`, `sysfs`, etc.) can be collected too.
    pub fn with_enable_system(mut self, enable_system: bool) -> Self {
        self.enable_system = enable_system;
        self
    }

    /// The timeout of the final flush that will happen when dropping this
    /// layer.
    pub fn with_drop_flush_timeout(mut self, drop_flush_timeout: time::Duration) -> Self {
        self.drop_flush_timeout = drop_flush_timeout;
        self
    }

    /// Turn this builder into a built layer.
    pub fn build(self) -> error::Result<SdkLayer> {
        SdkLayer::build(self)
    }
}
