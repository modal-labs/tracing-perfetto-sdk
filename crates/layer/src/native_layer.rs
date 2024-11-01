//! The main tracing layer and related utils exposed by this crate.
use std::sync::atomic;
use std::{borrow, cell, env, process, sync, thread, time};

use prost::encoding;
#[cfg(feature = "tokio")]
use tokio::task;
use tracing::span;
use tracing_perfetto_sdk_schema as schema;
use tracing_perfetto_sdk_schema::{
    counter_descriptor, trace_packet, track_descriptor, track_event,
};
use tracing_perfetto_sdk_sys::ffi;
use tracing_subscriber::{fmt, layer, registry};

use crate::{debug_annotations, error, ffi_utils, flavor, ids, init};

/// A layer to be used with `tracing-subscriber` that natively writes the
/// Perfetto trace packets in Rust code, but also polls the Perfetto SDK for
/// system-level events.
#[derive(Clone)]
#[repr(transparent)]
pub struct NativeLayer<W>
where
    W: for<'w> fmt::MakeWriter<'w>,
{
    inner: sync::Arc<Inner<W>>,
}

/// A builder for [`NativeLayer`]; use [`NativeLayer::from_config`] or
/// [`NativeLayer::from_config_bytes`] to create a new instance.
pub struct Builder<'c, W> {
    config_bytes: borrow::Cow<'c, [u8]>,
    writer: W,
    drop_flush_timeout: time::Duration,
    drop_poll_timeout: time::Duration,
    background_flush_timeout: time::Duration,
    background_poll_timeout: time::Duration,
    background_poll_interval: time::Duration,
    force_flavor: Option<flavor::Flavor>,
    enable_in_process: bool,
    enable_system: bool,
}

#[derive(Default)]
struct ThreadLocalCtx {
    descriptor_sent: atomic::AtomicBool,
    // We probably don't need a fancy data structure like `HashSet` or similar because this is
    // expected to contain a small (<100 entries) set of short (<20 chars) strings, and they are
    // all static (meaning high CPU cache coherence), so a linear scan + equals check is probably
    // faster than hashing strings etc.
    counters_sent_cache: cell::RefCell<Option<Vec<&'static str>>>,
}

struct Inner<W>
where
    W: for<'w> fmt::MakeWriter<'w>,
{
    // Mutex is held during start, stop, flush, and poll
    ffi_session: sync::Arc<sync::Mutex<Option<cxx::UniquePtr<ffi::PerfettoTracingSession>>>>,
    writer: sync::Arc<W>,
    drop_flush_timeout: time::Duration,
    drop_poll_timeout: time::Duration,
    force_flavor: Option<flavor::Flavor>,
    process_track_uuid: ids::TrackUuid,
    process_descriptor_sent: atomic::AtomicBool,
    // Mutex is held very briefly for member check and to insert a string if it is missing.
    // We probably don't need a fancy data structure like `HashSet` or similar because this is
    // expected to contain a small (<100 entries) set of short (<20 chars) strings, and they are
    // all static (meaning high CPU cache coherence), so a linear scan + equals check is probably
    // faster than hashing strings etc.
    counters_sent: sync::Mutex<Vec<&'static str>>,
    #[cfg(feature = "tokio")]
    tokio_descriptor_sent: atomic::AtomicBool,
    #[cfg(feature = "tokio")]
    tokio_track_uuid: ids::TrackUuid,
    thread_local_ctxs: thread_local::ThreadLocal<ThreadLocalCtx>,
}

impl<W> NativeLayer<W>
where
    W: for<'w> fmt::MakeWriter<'w> + Send + Sync + 'static,
{
    /// Create a new layer builder from the supplied proto config.
    ///
    /// The proto config is usually read from an external source using the
    /// prototext syntax, or else using one of the `serde` codecs.  The config
    /// will internally be encoded to bytes straight away, so prefer
    /// [`NativeLayer::from_config_bytes`] if you already have the byte
    /// representation.
    ///
    /// The built layer will write traces to the supplied writer.
    pub fn from_config(config: schema::TraceConfig, writer: W) -> Builder<'static, W> {
        use prost::Message as _;
        Builder::new(config.encode_to_vec().into(), writer)
    }

    /// Create a new layer builder from the supplied proto config bytes.
    ///
    /// The proto config bytes needs to be an already encoded message of type
    /// [`schema::TraceConfig`].
    ///
    /// The built layer will write traces to the supplied writer.
    pub fn from_config_bytes(config_bytes: &[u8], writer: W) -> Builder<W> {
        Builder::new(config_bytes.into(), writer)
    }

    fn build(builder: Builder<'_, W>) -> error::Result<Self> {
        // Shared global initialization for all layers
        init::global_init(builder.enable_in_process, builder.enable_system);

        // We send the config to the C++ code as encoded bytes, because it would be too
        // annoying to have some sort of shared proto struct between the Rust
        // and C++ worlds
        let mut ffi_session = ffi::new_tracing_session(builder.config_bytes.as_ref(), -1)?;
        ffi_session.pin_mut().start();
        let ffi_session = sync::Arc::new(sync::Mutex::new(Some(ffi_session)));

        let writer = sync::Arc::new(builder.writer);

        let drop_flush_timeout = builder.drop_flush_timeout;
        let drop_poll_timeout = builder.drop_poll_timeout;

        let thread_ffi_session = sync::Arc::clone(&ffi_session);
        let thread_writer = sync::Arc::clone(&writer);

        thread::Builder::new()
            .name("tracing-perfetto-poller".to_owned())
            .spawn(move || {
                background_poller_thread(
                    thread_ffi_session,
                    thread_writer,
                    builder.background_flush_timeout,
                    builder.background_poll_timeout,
                    builder.background_poll_interval,
                )
            })?;

        let force_flavor = builder.force_flavor;
        let pid = process::id();
        let process_track_uuid = ids::TrackUuid::for_process(pid);
        let process_descriptor_sent = atomic::AtomicBool::new(false);
        let counters_sent = sync::Mutex::new(Vec::new());
        #[cfg(feature = "tokio")]
        let tokio_descriptor_sent = atomic::AtomicBool::new(false);
        #[cfg(feature = "tokio")]
        let tokio_track_uuid = ids::TrackUuid::for_tokio();
        let thread_local_ctxs = thread_local::ThreadLocal::new();

        let inner = sync::Arc::new(Inner {
            ffi_session,
            writer,
            drop_flush_timeout,
            drop_poll_timeout,
            force_flavor,
            process_track_uuid,
            process_descriptor_sent,
            counters_sent,
            #[cfg(feature = "tokio")]
            tokio_descriptor_sent,
            #[cfg(feature = "tokio")]
            tokio_track_uuid,
            thread_local_ctxs,
        });

        Ok(Self { inner })
    }

    fn pick_trace_track_sequence(&self) -> (ids::TrackUuid, ids::SequenceId, flavor::Flavor) {
        if let Some(flavor) = self.inner.force_flavor.as_ref() {
            match *flavor {
                flavor::Flavor::Sync => {
                    let tid = thread_id::get();
                    (
                        self.inner.process_track_uuid,
                        ids::SequenceId::for_thread(tid),
                        flavor::Flavor::Sync,
                    )
                }
                flavor::Flavor::Async => {
                    #[cfg(feature = "tokio")]
                    if let Some(id) = task::try_id() {
                        return (
                            self.inner.process_track_uuid,
                            ids::SequenceId::for_task(id),
                            flavor::Flavor::Async,
                        );
                    }

                    let tid = thread_id::get();
                    (
                        self.inner.process_track_uuid,
                        ids::SequenceId::for_thread(tid),
                        flavor::Flavor::Async,
                    )
                }
            }
        } else {
            #[cfg(feature = "tokio")]
            if let Some(id) = task::try_id() {
                return (
                    self.inner.tokio_track_uuid,
                    ids::SequenceId::for_task(id),
                    flavor::Flavor::Async,
                );
            }

            let tid = thread_id::get();
            (
                ids::TrackUuid::for_thread(tid),
                ids::SequenceId::for_thread(tid),
                flavor::Flavor::Sync,
            )
        }
    }

    /// Flush internal buffers, making the best effort for all pending writes to
    /// be visible on this layer's `writer`.
    pub fn flush(
        &self,
        flush_timeout: time::Duration,
        poll_timeout: time::Duration,
    ) -> error::Result<()> {
        self.inner.flush(flush_timeout, poll_timeout)
    }

    /// Stop the layer and stop collecting traces.
    pub fn stop(&self) -> error::Result<()> {
        self.inner.stop()
    }

    fn ensure_context_known(&self, meta: &tracing::Metadata) {
        self.ensure_process_known(meta);
        self.ensure_thread_known(meta);
        #[cfg(feature = "tokio")]
        self.ensure_tokio_runtime_known(meta);
    }

    fn ensure_process_known(&self, meta: &tracing::Metadata) {
        let process_descriptor_sent = self
            .inner
            .process_descriptor_sent
            .fetch_or(true, atomic::Ordering::Relaxed);

        if !process_descriptor_sent {
            let process_name = env::current_exe()
                .unwrap_or_default()
                .to_string_lossy()
                .rsplit("/")
                .next()
                .unwrap_or_default()
                .to_owned();
            let cmdline = env::args_os()
                .map(|s| s.to_string_lossy().into_owned())
                .collect();
            let packet = schema::TracePacket {
                data: Some(trace_packet::Data::TrackDescriptor(
                    schema::TrackDescriptor {
                        uuid: Some(self.inner.process_track_uuid.as_raw()),
                        process: Some(schema::ProcessDescriptor {
                            pid: Some(process::id() as i32),
                            cmdline,
                            process_name: Some(process_name.clone()),
                            ..Default::default()
                        }),
                        static_or_dynamic_name: Some(track_descriptor::StaticOrDynamicName::Name(
                            process_name,
                        )),
                        ..Default::default()
                    },
                )),
                ..Default::default()
            };
            self.write_packet(meta, packet);
        }
    }

    fn ensure_thread_known(&self, meta: &tracing::Metadata) {
        let thread_local_ctx = self.inner.thread_local_ctxs.get_or(ThreadLocalCtx::default);
        let thread_descriptor_sent = thread_local_ctx
            .descriptor_sent
            .fetch_or(true, atomic::Ordering::Relaxed);
        if !thread_descriptor_sent {
            let thread_id = thread_id::get();
            let thread_name = thread::current()
                .name()
                .map(|s| s.to_owned())
                .unwrap_or_else(|| format!("(unnamed thread {thread_id})"));
            let packet = schema::TracePacket {
                data: Some(trace_packet::Data::TrackDescriptor(
                    schema::TrackDescriptor {
                        parent_uuid: Some(self.inner.process_track_uuid.as_raw()),
                        uuid: Some(ids::TrackUuid::for_thread(thread_id).as_raw()),
                        thread: Some(schema::ThreadDescriptor {
                            pid: Some(process::id() as i32),
                            tid: Some(thread_id as i32),
                            thread_name: Some(thread_name.clone()),
                            ..Default::default()
                        }),
                        static_or_dynamic_name: Some(track_descriptor::StaticOrDynamicName::Name(
                            thread_name,
                        )),
                        ..Default::default()
                    },
                )),
                ..Default::default()
            };
            self.write_packet(meta, packet);
        }
    }

    #[cfg(feature = "tokio")]
    fn ensure_tokio_runtime_known(&self, meta: &tracing::Metadata) {
        // Bogus thread ID; this is unlikely to ever be an actually real thread ID.
        const TOKIO_THREAD_ID: usize = (i32::MAX - 1) as usize;

        let tokio_descriptor_sent = self
            .inner
            .tokio_descriptor_sent
            .fetch_or(true, atomic::Ordering::Relaxed);
        if !tokio_descriptor_sent {
            let packet = schema::TracePacket {
                data: Some(trace_packet::Data::TrackDescriptor(
                    schema::TrackDescriptor {
                        parent_uuid: Some(self.inner.process_track_uuid.as_raw()),
                        uuid: Some(self.inner.tokio_track_uuid.as_raw()),
                        thread: Some(schema::ThreadDescriptor {
                            pid: Some(process::id() as i32),
                            tid: Some(TOKIO_THREAD_ID as i32),
                            thread_name: Some("tokio-runtime".to_owned()),
                            ..Default::default()
                        }),
                        static_or_dynamic_name: Some(track_descriptor::StaticOrDynamicName::Name(
                            "tokio-runtime".to_owned(),
                        )),
                        ..Default::default()
                    },
                )),
                ..Default::default()
            };
            self.write_packet(meta, packet);
        }
    }

    fn report_counters(&self, meta: &tracing::Metadata, counters: Vec<debug_annotations::Counter>) {
        if !counters.is_empty() {
            self.ensure_counters_known(meta, &counters);
            for counter in counters {
                self.write_counter_event(meta, counter);
            }
        }
    }

    fn ensure_counters_known(
        &self,
        meta: &tracing::Metadata,
        counters: &[debug_annotations::Counter],
    ) {
        let thread_local_ctx = self.inner.thread_local_ctxs.get_or(ThreadLocalCtx::default);
        if let Ok(mut counters_sent_cache) = thread_local_ctx.counters_sent_cache.try_borrow_mut() {
            let counters_sent_cache = counters_sent_cache.get_or_insert_with(|| {
                self.inner
                    .counters_sent
                    .lock()
                    .map(|cs| cs.clone())
                    .unwrap_or_default()
            });
            if counters
                .iter()
                .all(|c| counters_sent_cache.contains(&c.name))
            {
                return;
            }
        }

        // This might seem wasteful, but we want to minimize the time that we hold the
        // `counters_sent` mutex, and we can report the counters after we have released
        // the lock.  Also remember that `Vec::new()` does not allocate until the first
        // push!
        let mut new_counters = Vec::new();

        // Skip counters entirely if Mutex is poisoned -- can't afford to panic here
        if let Ok(mut counters_sent) = self.inner.counters_sent.lock() {
            for counter in counters {
                if !counters_sent.contains(&counter.name) {
                    new_counters.push(counter);
                    counters_sent.push(counter.name);
                }
            }
            if !new_counters.is_empty() {
                if let Ok(mut counters_sent_cache) =
                    thread_local_ctx.counters_sent_cache.try_borrow_mut()
                {
                    *counters_sent_cache = Some((*counters_sent).clone());
                }
            }
        }

        for counter in new_counters {
            self.write_counter_track_descriptor(meta, counter);
        }
    }

    fn write_counter_track_descriptor(
        &self,
        meta: &tracing::Metadata,
        counter: &debug_annotations::Counter,
    ) {
        let (unit, unit_name) = if let Some(unit) = counter.unit {
            Self::pick_unit_repr(unit)
        } else {
            (None, None)
        };

        let packet = schema::TracePacket {
            data: Some(trace_packet::Data::TrackDescriptor(
                schema::TrackDescriptor {
                    parent_uuid: Some(self.inner.process_track_uuid.as_raw()),
                    uuid: Some(ids::TrackUuid::for_counter(counter.name).as_raw()),
                    counter: Some(schema::CounterDescriptor {
                        unit_name,
                        unit,
                        ..Default::default()
                    }),
                    static_or_dynamic_name: Some(track_descriptor::StaticOrDynamicName::Name(
                        counter.name.to_owned(),
                    )),
                    ..Default::default()
                },
            )),
            ..Default::default()
        };

        self.write_packet(meta, packet);
    }

    fn write_counter_event(&self, meta: &tracing::Metadata, counter: debug_annotations::Counter) {
        let packet = schema::TracePacket {
            timestamp: Some(ffi::trace_time_ns()),
            optional_trusted_packet_sequence_id: Some(
                trace_packet::OptionalTrustedPacketSequenceId::TrustedPacketSequenceId(
                    ids::SequenceId::for_counter(counter.name).as_raw(),
                ),
            ),
            data: Some(trace_packet::Data::TrackEvent(schema::TrackEvent {
                r#type: Some(track_event::Type::Counter as i32),
                track_uuid: Some(ids::TrackUuid::for_counter(counter.name).as_raw()),
                counter_value_field: Some(counter.value.to_proto()),
                ..Default::default()
            })),
            ..Default::default()
        };
        self.write_packet(meta, packet);
    }

    /// For a named unit, try to find an existing proto definition for that unit
    /// and return as first return value, or else fall back to naming the unit
    /// by name as the second return value.
    fn pick_unit_repr(unit: &str) -> (Option<i32>, Option<String>) {
        // If there's a defined unit in the proto schema, use that:
        Self::parse_unit(unit)
            .map(|u| (Some(u as i32), None))
            .unwrap_or_else(|| {
                // ...else, send the unit by name:
                (None, Some(unit.to_owned()))
            })
    }

    /// For a named unit, try to find an existing proto definition for that
    /// unit.
    fn parse_unit(name: &str) -> Option<counter_descriptor::Unit> {
        match name {
            "time_ns" | "ns" | "nanos" | "nanoseconds" => Some(counter_descriptor::Unit::TimeNs),
            "count" | "nr" => Some(counter_descriptor::Unit::Count),
            "size_bytes" | "bytes" => Some(counter_descriptor::Unit::SizeBytes),
            _ => None,
        }
    }

    fn write_packet(&self, meta: &tracing::Metadata, packet: schema::TracePacket) {
        // The field tag of `packet` within the `Trace` proto message.
        const PACKET_FIELD_TAG: u32 = 1;

        use std::io::Write as _;

        use prost::Message as _;

        // We will insert a protobuf field header before the written packet, which will
        // take the shape `[0x06, <length varint bytes>]` where the `<length varint
        // bytes>` takes between 1 and 10 bytes depending on the size of the packet.
        let packet_len = packet.encoded_len() as u64;
        let varint_len = encoding::encoded_len_varint(packet_len);
        let mut buf = bytes::BytesMut::with_capacity(1 + varint_len + packet.encoded_len());
        encoding::encode_key(
            PACKET_FIELD_TAG,
            encoding::WireType::LengthDelimited,
            &mut buf,
        );
        encoding::encode_varint(packet_len, &mut buf);
        packet
            .encode(&mut buf)
            .expect("buf should have had sufficient capacity");

        let _ = self.inner.writer.make_writer_for(meta).write_all(&buf);
    }

    fn slice_begin(
        &self,
        meta: &tracing::Metadata,
        track_uuid: ids::TrackUuid,
        sequence_id: ids::SequenceId,
        debug_annotations: debug_annotations::ProtoDebugAnnotations,
    ) {
        let packet = schema::TracePacket {
            timestamp: Some(ffi::trace_time_ns()),
            optional_trusted_packet_sequence_id: Some(
                trace_packet::OptionalTrustedPacketSequenceId::TrustedPacketSequenceId(
                    sequence_id.as_raw(),
                ),
            ),
            data: Some(trace_packet::Data::TrackEvent(schema::TrackEvent {
                r#type: Some(track_event::Type::SliceBegin as i32),
                track_uuid: Some(track_uuid.as_raw()),
                name_field: Some(track_event::NameField::Name(meta.name().to_owned())),
                debug_annotations: debug_annotations.into_proto(),
                source_location_field: source_location_field(meta),
                ..Default::default()
            })),
            ..Default::default()
        };

        self.ensure_context_known(meta);
        self.write_packet(meta, packet);
    }

    fn slice_end(
        &self,
        meta: &tracing::Metadata,
        track_uuid: ids::TrackUuid,
        sequence_id: ids::SequenceId,
        extensions: registry::Extensions,
    ) {
        let entered_track_uuid = extensions.get::<ids::TrackUuid>();
        let track_uuid = *entered_track_uuid.unwrap_or(&track_uuid);
        let entered_sequence_id = extensions.get::<ids::SequenceId>();
        let sequence_id = *entered_sequence_id.unwrap_or(&sequence_id);

        let packet = schema::TracePacket {
            timestamp: Some(ffi::trace_time_ns()),
            optional_trusted_packet_sequence_id: Some(
                trace_packet::OptionalTrustedPacketSequenceId::TrustedPacketSequenceId(
                    sequence_id.as_raw(),
                ),
            ),
            data: Some(trace_packet::Data::TrackEvent(schema::TrackEvent {
                r#type: Some(track_event::Type::SliceEnd as i32),
                track_uuid: Some(track_uuid.as_raw()),
                name_field: Some(track_event::NameField::Name(meta.name().to_owned())),
                source_location_field: source_location_field(meta),
                ..Default::default()
            })),
            ..Default::default()
        };

        self.ensure_context_known(meta);
        self.write_packet(meta, packet);
    }
}

impl<S, W> tracing_subscriber::Layer<S> for NativeLayer<W>
where
    S: tracing::Subscriber,
    S: for<'a> registry::LookupSpan<'a>,
    W: for<'w> fmt::MakeWriter<'w> + Send + Sync + 'static,
{
    fn on_new_span(&self, attrs: &span::Attributes<'_>, id: &span::Id, ctx: layer::Context<'_, S>) {
        let span = ctx.span(id).expect("span to be found (this is a bug)");
        let meta = span.metadata();

        let (track_uuid, sequence_id, flavor) = self.pick_trace_track_sequence();
        span.extensions_mut().insert(track_uuid);
        span.extensions_mut().insert(sequence_id);

        let mut debug_annotations = debug_annotations::ProtoDebugAnnotations::default();
        attrs.record(&mut debug_annotations);
        self.report_counters(meta, debug_annotations.take_counters());
        span.extensions_mut().insert(debug_annotations.clone());

        if flavor == flavor::Flavor::Async {
            self.slice_begin(meta, track_uuid, sequence_id, debug_annotations);
        }
    }

    fn on_record(&self, id: &tracing::Id, values: &span::Record<'_>, ctx: layer::Context<'_, S>) {
        let span = ctx.span(id).expect("span to be found (this is a bug)");
        let meta = span.metadata();

        let mut extensions = span.extensions_mut();
        if let Some(debug_annotations) =
            extensions.get_mut::<debug_annotations::ProtoDebugAnnotations>()
        {
            values.record(debug_annotations);
            self.report_counters(meta, debug_annotations.take_counters());
        } else {
            let mut debug_annotations = debug_annotations::ProtoDebugAnnotations::default();
            values.record(&mut debug_annotations);
            extensions.insert(debug_annotations);
        }
    }

    fn on_event(&self, event: &tracing::Event<'_>, _ctx: layer::Context<'_, S>) {
        let meta = event.metadata();
        let mut debug_annotations = debug_annotations::ProtoDebugAnnotations::default();
        event.record(&mut debug_annotations);
        self.report_counters(meta, debug_annotations.take_counters());

        let (track_uuid, sequence_id, _) = self.pick_trace_track_sequence();

        let packet = schema::TracePacket {
            timestamp: Some(ffi::trace_time_ns()),
            optional_trusted_packet_sequence_id: Some(
                trace_packet::OptionalTrustedPacketSequenceId::TrustedPacketSequenceId(
                    sequence_id.as_raw(),
                ),
            ),
            data: Some(trace_packet::Data::TrackEvent(schema::TrackEvent {
                r#type: Some(track_event::Type::Instant as i32),
                track_uuid: Some(track_uuid.as_raw()),
                name_field: Some(track_event::NameField::Name(meta.name().to_owned())),
                debug_annotations: debug_annotations.into_proto(),
                source_location_field: source_location_field(meta),
                ..Default::default()
            })),
            ..Default::default()
        };

        self.ensure_context_known(meta);
        self.write_packet(meta, packet);
    }

    fn on_enter(&self, id: &span::Id, ctx: layer::Context<'_, S>) {
        let span = ctx.span(id).expect("span to be found (this is a bug)");

        let (track_uuid, sequence_id, flavor) = self.pick_trace_track_sequence();
        let meta = span.metadata();

        if flavor == flavor::Flavor::Sync {
            span.extensions_mut().replace(track_uuid);
            span.extensions_mut().replace(sequence_id);

            let debug_annotations = span
                .extensions()
                .get::<debug_annotations::ProtoDebugAnnotations>()
                .cloned()
                .unwrap_or_default();
            self.slice_begin(meta, track_uuid, sequence_id, debug_annotations);
        }
    }

    fn on_exit(&self, id: &tracing::Id, ctx: layer::Context<'_, S>) {
        let span = ctx.span(id).expect("span to be found (this is a bug)");

        let meta = span.metadata();
        let (track_uuid, sequence_id, flavor) = self.pick_trace_track_sequence();
        let extensions = span.extensions();

        if flavor == flavor::Flavor::Sync {
            self.slice_end(meta, track_uuid, sequence_id, extensions);
        }
    }

    fn on_close(&self, id: tracing::Id, ctx: layer::Context<'_, S>) {
        let span = ctx.span(&id).expect("span to be found (this is a bug)");

        let meta = span.metadata();
        let (track_uuid, sequence_id, flavor) = self.pick_trace_track_sequence();
        let extensions = span.extensions();

        if flavor == flavor::Flavor::Async {
            self.slice_end(meta, track_uuid, sequence_id, extensions);
        }
    }
}

impl<'c, W> Builder<'c, W>
where
    W: for<'w> fmt::MakeWriter<'w> + Send + Sync + 'static,
{
    fn new(config_bytes: borrow::Cow<'c, [u8]>, writer: W) -> Self {
        Self {
            config_bytes,
            writer,
            drop_flush_timeout: time::Duration::from_millis(100),
            drop_poll_timeout: time::Duration::from_millis(100),
            background_flush_timeout: time::Duration::from_millis(100),
            background_poll_timeout: time::Duration::from_millis(100),
            background_poll_interval: time::Duration::from_millis(100),
            force_flavor: None,
            enable_in_process: true,
            enable_system: false,
        }
    }

    /// If `Some`, force the specified trace flavor. If `None`, use heuristics
    /// for every created span to determine the flavor.
    pub fn with_force_flavor(mut self, force_flavor: Option<flavor::Flavor>) -> Self {
        self.force_flavor = force_flavor;
        self
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

    /// The timeout of the final poll that will happen when dropping this
    /// layer.
    pub fn with_drop_poll_timeout(mut self, drop_flush_timeout: time::Duration) -> Self {
        self.drop_flush_timeout = drop_flush_timeout;
        self
    }

    /// The timeout of each flush in the background trace polling thread.
    pub fn with_background_flush_timeout(
        mut self,
        background_flush_timeout: time::Duration,
    ) -> Self {
        self.background_flush_timeout = background_flush_timeout;
        self
    }

    /// The timeout of each poll in the background trace polling thread.
    pub fn with_background_poll_timeout(mut self, background_poll_timeout: time::Duration) -> Self {
        self.background_poll_timeout = background_poll_timeout;
        self
    }

    /// The delay between each poll in the background trace polling thread.
    pub fn with_background_poll_interval(
        mut self,
        background_poll_interval: time::Duration,
    ) -> Self {
        self.background_poll_interval = background_poll_interval;
        self
    }

    /// Turn this builder into a built layer.
    pub fn build(self) -> error::Result<NativeLayer<W>> {
        NativeLayer::build(self)
    }
}

impl<W> Inner<W>
where
    W: for<'w> fmt::MakeWriter<'w>,
{
    fn flush(
        &self,
        flush_timeout: time::Duration,
        poll_timeout: time::Duration,
    ) -> error::Result<()> {
        use std::io::Write as _;
        let data = ffi_utils::with_session_lock(&*self.ffi_session, |session| {
            ffi_utils::do_flush(session, flush_timeout)?;
            let data = ffi_utils::do_poll_traces(session, poll_timeout)?;
            Ok(data)
        })?;
        self.writer.make_writer().write_all(&*data.data)?;
        Ok(())
    }

    fn stop(&self) -> error::Result<()> {
        // Can't use ffi_utils::with_session_lock here because we want to take the
        // session object
        let mut session = self
            .ffi_session
            .lock()
            .map_err(|_| error::Error::PoisonedMutex)?;
        if let Some(mut session) = session.take() {
            session.pin_mut().stop();
        }
        Ok(())
    }
}

impl<W> Drop for Inner<W>
where
    W: for<'w> fmt::MakeWriter<'w>,
{
    fn drop(&mut self) {
        let _ = self.flush(self.drop_flush_timeout, self.drop_poll_timeout);
        let _ = self.stop();
    }
}

fn background_poller_thread<W>(
    ffi_session: sync::Arc<sync::Mutex<Option<cxx::UniquePtr<ffi::PerfettoTracingSession>>>>,
    writer: sync::Arc<W>,
    background_flush_timeout: time::Duration,
    background_poll_timeout: time::Duration,
    background_poll_interval: time::Duration,
) where
    W: for<'w> fmt::MakeWriter<'w> + 'static,
{
    use std::io::Write as _;

    loop {
        let poll_result = ffi_utils::with_session_lock(&*ffi_session, |session| {
            // TODO: consider making timeouts configurable
            ffi_utils::do_flush(session, background_flush_timeout)?;
            let data = ffi_utils::do_poll_traces(session, background_poll_timeout)?;
            Ok(data)
        });

        match poll_result {
            Ok(data) => {
                let _ = writer.make_writer().write_all(&*data.data);
            }
            Err(error) => match error {
                error::Error::TimedOut => {
                    tracing::warn!(
                        "background trace poll operation timed out; will ignore and continue"
                    );
                }
                error::Error::LayerStopped => {
                    break;
                }
                error => {
                    tracing::error!(
                        ?error,
                        "background trace poll operation failed; will terminate"
                    );
                    break;
                }
            },
        }
        thread::sleep(background_poll_interval);
    }
}

fn source_location_field(meta: &tracing::Metadata) -> Option<track_event::SourceLocationField> {
    Some(track_event::SourceLocationField::SourceLocation(
        schema::SourceLocation {
            file_name: Some(meta.file().unwrap_or("").to_owned()),
            line_number: Some(meta.line().unwrap_or(0)),
            ..Default::default()
        },
    ))
}
