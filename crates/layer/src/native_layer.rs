//! The main tracing layer and related utils exposed by this crate.
use std::sync::atomic;
use std::{borrow, env, marker, mem, process, sync, thread, time};

use prost::encoding;
#[cfg(feature = "tokio")]
use tokio::task;
use tracing::span;
use tracing_perfetto_sdk_schema as schema;
use tracing_perfetto_sdk_schema::{
    counter_descriptor, trace_packet, track_descriptor, track_event,
};
#[cfg(feature = "sdk")]
use tracing_perfetto_sdk_sys::ffi;
use tracing_subscriber::{fmt, layer, registry};

#[cfg(feature = "sdk")]
use crate::ffi_utils;
use crate::ids::thread_id;
use crate::{debug_annotations, error, flavor, ids, init};

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
    #[cfg(feature = "sdk")]
    config_bytes: borrow::Cow<'c, [u8]>,
    writer: W,
    drop_flush_timeout: time::Duration,
    drop_poll_timeout: time::Duration,
    background_flush_timeout: time::Duration,
    background_poll_timeout: time::Duration,
    background_poll_interval: time::Duration,
    force_flavor: Option<flavor::Flavor>,
    delay_slice_begin: bool,
    discard_tracing_data: bool,
    create_async_tracks: Option<String>,
    enable_in_process: bool,
    enable_system: bool,
    name: &'c str,
    _phantom: marker::PhantomData<&'c ()>,
}

struct Inner<W>
where
    W: for<'w> fmt::MakeWriter<'w>,
{
    // Mutex is held during start, stop, flush, and poll
    #[cfg(feature = "sdk")]
    ffi_session: sync::Arc<sync::Mutex<Option<cxx::UniquePtr<ffi::PerfettoTracingSession>>>>,
    writer: sync::Arc<W>,
    drop_flush_timeout: time::Duration,
    drop_poll_timeout: time::Duration,
    force_flavor: Option<flavor::Flavor>,
    delay_slice_begin: bool,
    discard_tracing_data: bool,
    create_async_tracks: Option<String>,
    process_track_uuid: ids::TrackUuid,
    process_descriptor_sent: atomic::AtomicBool,
    #[cfg(feature = "tokio")]
    tokio_descriptor_sent: atomic::AtomicBool,
    #[cfg(feature = "tokio")]
    tokio_track_uuid: ids::TrackUuid,
    counter_tracks_sent: dashmap::DashSet<&'static str>,
    thread_tracks_sent: dashmap::DashSet<usize>,
    #[cfg(feature = "tokio")]
    task_tracks_sent: dashmap::DashSet<task::Id>,
}

// Does not contain DebugAnnotations; they are shipped separately as a span
// extension
struct DelayedSliceBegin {
    timestamp_ns: u64,
    timestamp_clock_id: u32,
    meta: &'static tracing::Metadata<'static>,
    track_uuid: ids::TrackUuid,
    sequence_id: ids::SequenceId,
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
        init::global_init(builder.name, builder.enable_in_process, builder.enable_system);

        let writer = sync::Arc::new(builder.writer);

        // We send the config to the C++ code as encoded bytes, because it would be too
        // annoying to have some sort of shared proto struct between the Rust
        // and C++ worlds
        #[cfg(feature = "sdk")]
        let ffi_session = {
            let mut ffi_session = ffi::new_tracing_session(builder.config_bytes.as_ref(), -1)?;
            ffi_session.pin_mut().start();
            let ffi_session = sync::Arc::new(sync::Mutex::new(Some(ffi_session)));

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
            ffi_session
        };

        let drop_flush_timeout = builder.drop_flush_timeout;
        let drop_poll_timeout = builder.drop_poll_timeout;

        let force_flavor = builder.force_flavor;
        let delay_slice_begin = builder.delay_slice_begin;
        let discard_tracing_data = builder.discard_tracing_data;
        let create_async_tracks = builder.create_async_tracks;
        let pid = process::id();
        let process_track_uuid = ids::TrackUuid::for_process(pid);
        let process_descriptor_sent = atomic::AtomicBool::new(false);
        #[cfg(feature = "tokio")]
        let tokio_descriptor_sent = atomic::AtomicBool::new(false);
        #[cfg(feature = "tokio")]
        let tokio_track_uuid = ids::TrackUuid::for_tokio();
        let counter_tracks_sent = dashmap::DashSet::new();
        let thread_tracks_sent = dashmap::DashSet::new();
        #[cfg(feature = "tokio")]
        let task_tracks_sent = dashmap::DashSet::new();

        let inner = sync::Arc::new(Inner {
            #[cfg(feature = "sdk")]
            ffi_session,
            writer,
            drop_flush_timeout,
            drop_poll_timeout,
            force_flavor,
            delay_slice_begin,
            discard_tracing_data,
            create_async_tracks,
            process_track_uuid,
            process_descriptor_sent,
            #[cfg(feature = "tokio")]
            tokio_descriptor_sent,
            #[cfg(feature = "tokio")]
            tokio_track_uuid,
            counter_tracks_sent,
            thread_tracks_sent,
            #[cfg(feature = "tokio")]
            task_tracks_sent,
        });

        Ok(Self { inner })
    }

    fn pick_trace_track_sequence(&self) -> (ids::TrackUuid, ids::SequenceId, flavor::Flavor) {
        if let Some(flavor) = self.inner.force_flavor.as_ref() {
            match *flavor {
                flavor::Flavor::Sync => {
                    let tid = thread_id();
                    (
                        self.inner.process_track_uuid,
                        ids::SequenceId::for_thread(tid),
                        flavor::Flavor::Sync,
                    )
                }
                flavor::Flavor::Async => {
                    #[cfg(feature = "tokio")]
                    if let Some(res) =
                        self.tokio_trace_track_sequence(self.inner.process_track_uuid)
                    {
                        return res;
                    }

                    let tid = thread_id();
                    let track_uuid = if self.inner.create_async_tracks.is_some() {
                        ids::TrackUuid::for_thread(tid)
                    } else {
                        self.inner.process_track_uuid
                    };
                    (
                        track_uuid,
                        ids::SequenceId::for_thread(tid),
                        flavor::Flavor::Async,
                    )
                }
            }
        } else {
            #[cfg(feature = "tokio")]
            if let Some(res) = self.tokio_trace_track_sequence(self.inner.tokio_track_uuid) {
                return res;
            }

            let tid = thread_id();
            (
                ids::TrackUuid::for_thread(tid),
                ids::SequenceId::for_thread(tid),
                flavor::Flavor::Sync,
            )
        }
    }

    #[cfg(feature = "tokio")]
    fn tokio_trace_track_sequence(
        &self,
        default_track: ids::TrackUuid,
    ) -> Option<(ids::TrackUuid, ids::SequenceId, flavor::Flavor)> {
        let id = task::try_id()?;
        let track_uuid = if self.inner.create_async_tracks.is_some() {
            ids::TrackUuid::for_task(id)
        } else {
            default_track
        };

        Some((
            track_uuid,
            ids::SequenceId::for_task(id),
            flavor::Flavor::Async,
        ))
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

    fn report_slice_begin(
        &self,
        meta: &tracing::Metadata,
        track_uuid: ids::TrackUuid,
        sequence_id: ids::SequenceId,
        debug_annotations: debug_annotations::ProtoDebugAnnotations,
    ) {
        let packet = self.create_slice_begin_track_event_packet(
            trace_time_ns(),
            trace_clock_id(),
            meta,
            track_uuid,
            sequence_id,
            debug_annotations,
        );
        self.ensure_context_known(meta);
        self.write_packet(meta, packet);
    }

    fn report_slice_end(
        &self,
        meta: &tracing::Metadata,
        track_uuid: ids::TrackUuid,
        sequence_id: ids::SequenceId,
        extensions: registry::Extensions,
    ) {
        let track_uuid = extensions
            .get::<ids::TrackUuid>()
            .copied()
            .unwrap_or(track_uuid);
        let sequence_id = extensions
            .get::<ids::SequenceId>()
            .copied()
            .unwrap_or(sequence_id);
        let debug_annotations = extensions
            .get::<debug_annotations::ProtoDebugAnnotations>()
            .cloned()
            .unwrap_or_default();

        if let Some(delayed_slice_begin) = extensions.get::<DelayedSliceBegin>() {
            let slice_begin_packet = self.create_slice_begin_track_event_packet(
                delayed_slice_begin.timestamp_ns,
                delayed_slice_begin.timestamp_clock_id,
                delayed_slice_begin.meta,
                delayed_slice_begin.track_uuid,
                delayed_slice_begin.sequence_id,
                debug_annotations.clone(),
            );
            self.ensure_context_known(delayed_slice_begin.meta);
            self.write_packet(delayed_slice_begin.meta, slice_begin_packet);
        }

        let packet = self.create_slice_end_track_event_packet(
            trace_time_ns(),
            trace_clock_id(),
            meta,
            track_uuid,
            sequence_id,
            debug_annotations,
        );
        self.ensure_context_known(meta);
        self.write_packet(meta, packet);
    }

    fn report_event(
        &self,
        meta: &tracing::Metadata,
        debug_annotations: debug_annotations::ProtoDebugAnnotations,
        track_uuid: ids::TrackUuid,
        sequence_id: ids::SequenceId,
    ) {
        let packet = self.create_event_track_event_packet(
            trace_time_ns(),
            trace_clock_id(),
            meta,
            debug_annotations,
            track_uuid,
            sequence_id,
        );
        self.ensure_context_known(meta);
        self.write_packet(meta, packet);
    }

    fn report_counters(&self, meta: &tracing::Metadata, counters: Vec<debug_annotations::Counter>) {
        if !counters.is_empty() {
            let timestamp_ns = trace_time_ns();
            let timestamp_clock_id = trace_clock_id();
            self.ensure_counters_known(meta, &counters);
            for counter in counters {
                let packet = self.create_counter_track_event_packet(
                    timestamp_ns,
                    timestamp_clock_id,
                    counter,
                );
                self.write_packet(meta, packet);
            }
        }
    }

    fn ensure_context_known(&self, meta: &tracing::Metadata) {
        self.ensure_process_known(meta);
        self.ensure_thread_known(meta);
        #[cfg(feature = "tokio")]
        if let Some(ref name) = self.inner.create_async_tracks {
            self.ensure_task_track_known(meta, name);
        } else {
            self.ensure_tokio_runtime_known(meta);
        }
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
            let packet = self.create_process_track_descriptor(process_name, cmdline);
            self.write_packet(meta, packet);
        }
    }

    fn ensure_thread_known(&self, meta: &tracing::Metadata) {
        let thread_id = thread_id();
        if self.inner.thread_tracks_sent.insert(thread_id) {
            let thread_name = thread::current()
                .name()
                .map(|s| s.to_owned())
                .unwrap_or_else(|| format!("(unnamed thread {thread_id})"));
            let packet = if let Some(ref name) = self.inner.create_async_tracks {
                self.create_thread_track_descriptor(thread_id, name.to_owned(), false)
            } else {
                self.create_thread_track_descriptor(thread_id, thread_name, true)
            };
            self.write_packet(meta, packet);
        }
    }

    #[cfg(feature = "tokio")]
    fn ensure_tokio_runtime_known(&self, meta: &tracing::Metadata) {
        let tokio_descriptor_sent = self
            .inner
            .tokio_descriptor_sent
            .fetch_or(true, atomic::Ordering::Relaxed);
        if !tokio_descriptor_sent {
            let packet = self.create_tokio_runtime_track_descriptor();
            self.write_packet(meta, packet);
        }
    }

    #[cfg(feature = "tokio")]
    fn ensure_task_track_known(&self, meta: &tracing::Metadata, name: &str) {
        if let Some(task_id) = task::try_id() {
            if self.inner.task_tracks_sent.insert(task_id) {
                let packet = self.create_task_track_descriptor(task_id, name.to_owned());
                self.write_packet(meta, packet);
            }
        }
    }

    fn ensure_counters_known(
        &self,
        meta: &tracing::Metadata,
        counters: &[debug_annotations::Counter],
    ) {
        for counter in counters {
            if self.inner.counter_tracks_sent.insert(counter.name) {
                let packet = self.create_counter_track_descriptor(counter);
                self.write_packet(meta, packet);
            }
        }
    }

    #[must_use]
    fn create_process_track_descriptor(
        &self,
        process_name: String,
        cmdline: Vec<String>,
    ) -> schema::TracePacket {
        schema::TracePacket {
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
        }
    }

    #[must_use]
    fn create_thread_track_descriptor(
        &self,
        thread_id: usize,
        thread_name: String,
        include_thread_metadata: bool,
    ) -> schema::TracePacket {
        let thread = if include_thread_metadata {
            Some(schema::ThreadDescriptor {
                pid: Some(process::id() as i32),
                tid: Some((thread_id as i32).saturating_abs()),
                thread_name: Some(thread_name.clone()),
                ..Default::default()
            })
        } else {
            None
        };
        schema::TracePacket {
            data: Some(trace_packet::Data::TrackDescriptor(
                schema::TrackDescriptor {
                    parent_uuid: Some(self.inner.process_track_uuid.as_raw()),
                    uuid: Some(ids::TrackUuid::for_thread(thread_id).as_raw()),
                    thread,
                    static_or_dynamic_name: Some(track_descriptor::StaticOrDynamicName::Name(
                        thread_name,
                    )),
                    ..Default::default()
                },
            )),
            ..Default::default()
        }
    }

    #[cfg(feature = "tokio")]
    #[must_use]
    fn create_tokio_runtime_track_descriptor(&self) -> schema::TracePacket {
        // Bogus thread ID; this is unlikely to ever be an actually real thread ID.
        const TOKIO_THREAD_ID: usize = (i32::MAX - 1) as usize;
        schema::TracePacket {
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
        }
    }

    #[cfg(feature = "tokio")]
    #[must_use]
    fn create_task_track_descriptor(&self, task_id: task::Id, name: String) -> schema::TracePacket {
        let parent_uuid = if self.inner.force_flavor == Some(flavor::Flavor::Async) {
            self.inner.process_track_uuid
        } else {
            self.inner.tokio_track_uuid
        };
        schema::TracePacket {
            data: Some(trace_packet::Data::TrackDescriptor(
                schema::TrackDescriptor {
                    parent_uuid: Some(parent_uuid.as_raw()),
                    uuid: Some(ids::TrackUuid::for_task(task_id).as_raw()),
                    static_or_dynamic_name: Some(track_descriptor::StaticOrDynamicName::Name(
                        name.to_owned(),
                    )),
                    ..Default::default()
                },
            )),
            ..Default::default()
        }
    }

    #[must_use]
    fn create_counter_track_descriptor(
        &self,
        counter: &debug_annotations::Counter,
    ) -> schema::TracePacket {
        let (unit, unit_name) = if let Some(unit) = counter.unit {
            Self::pick_unit_repr(unit)
        } else {
            (None, None)
        };

        schema::TracePacket {
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
        }
    }

    #[must_use]
    fn create_slice_begin_track_event_packet(
        &self,
        timestamp_ns: u64,
        timestamp_clock_id: u32,
        meta: &tracing::Metadata,
        track_uuid: ids::TrackUuid,
        sequence_id: ids::SequenceId,
        debug_annotations: debug_annotations::ProtoDebugAnnotations,
    ) -> schema::TracePacket {
        schema::TracePacket {
            timestamp: Some(timestamp_ns),
            timestamp_clock_id: Some(timestamp_clock_id),
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
                source_location_field: Self::source_location_field(meta),
                ..Default::default()
            })),
            ..Default::default()
        }
    }

    #[must_use]
    fn create_slice_end_track_event_packet(
        &self,
        timestamp_ns: u64,
        timestamp_clock_id: u32,
        meta: &tracing::Metadata,
        track_uuid: ids::TrackUuid,
        sequence_id: ids::SequenceId,
        debug_annotations: debug_annotations::ProtoDebugAnnotations,
    ) -> schema::TracePacket {
        schema::TracePacket {
            timestamp: Some(timestamp_ns),
            timestamp_clock_id: Some(timestamp_clock_id),
            optional_trusted_packet_sequence_id: Some(
                trace_packet::OptionalTrustedPacketSequenceId::TrustedPacketSequenceId(
                    sequence_id.as_raw(),
                ),
            ),
            data: Some(trace_packet::Data::TrackEvent(schema::TrackEvent {
                r#type: Some(track_event::Type::SliceEnd as i32),
                track_uuid: Some(track_uuid.as_raw()),
                name_field: Some(track_event::NameField::Name(meta.name().to_owned())),
                debug_annotations: debug_annotations.into_proto(),
                source_location_field: Self::source_location_field(meta),
                ..Default::default()
            })),
            ..Default::default()
        }
    }

    #[must_use]
    fn create_event_track_event_packet(
        &self,
        timestamp_ns: u64,
        timestamp_clock_id: u32,
        meta: &tracing::Metadata,
        debug_annotations: debug_annotations::ProtoDebugAnnotations,
        track_uuid: ids::TrackUuid,
        sequence_id: ids::SequenceId,
    ) -> schema::TracePacket {
        schema::TracePacket {
            timestamp: Some(timestamp_ns),
            timestamp_clock_id: Some(timestamp_clock_id),
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
                source_location_field: Self::source_location_field(meta),
                ..Default::default()
            })),
            ..Default::default()
        }
    }

    #[must_use]
    fn create_counter_track_event_packet(
        &self,
        timestamp_ns: u64,
        timestamp_clock_id: u32,
        counter: debug_annotations::Counter,
    ) -> schema::TracePacket {
        schema::TracePacket {
            timestamp: Some(timestamp_ns),
            timestamp_clock_id: Some(timestamp_clock_id),
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

    fn source_location_field(meta: &tracing::Metadata) -> Option<track_event::SourceLocationField> {
        Some(track_event::SourceLocationField::SourceLocation(
            schema::SourceLocation {
                file_name: Some(meta.file().unwrap_or("").to_owned()),
                line_number: Some(meta.line().unwrap_or(0)),
                ..Default::default()
            },
        ))
    }
}

impl<S, W> tracing_subscriber::Layer<S> for NativeLayer<W>
where
    S: tracing::Subscriber,
    S: for<'a> registry::LookupSpan<'a>,
    W: for<'w> fmt::MakeWriter<'w> + Send + Sync + 'static,
{
    fn on_new_span(&self, attrs: &span::Attributes<'_>, id: &span::Id, ctx: layer::Context<'_, S>) {
        if self.inner.discard_tracing_data {
            return;
        }

        let span = ctx.span(id).expect("span to be found (this is a bug)");
        let meta = span.metadata();

        let (track_uuid, sequence_id, flavor) = self.pick_trace_track_sequence();
        span.extensions_mut().insert(track_uuid);
        span.extensions_mut().insert(sequence_id);

        let mut debug_annotations = debug_annotations::ProtoDebugAnnotations::default();
        attrs.record(&mut debug_annotations);
        self.report_counters(meta, debug_annotations.take_counters());

        if flavor == flavor::Flavor::Async {
            if self.inner.delay_slice_begin {
                span.extensions_mut().insert(debug_annotations);
                span.extensions_mut().insert(DelayedSliceBegin {
                    timestamp_ns: trace_time_ns(),
                    timestamp_clock_id: trace_clock_id(),
                    meta,
                    track_uuid,
                    sequence_id,
                })
            } else {
                self.report_slice_begin(meta, track_uuid, sequence_id, debug_annotations);
            }
        }
    }

    fn on_record(&self, id: &tracing::Id, values: &span::Record<'_>, ctx: layer::Context<'_, S>) {
        if self.inner.discard_tracing_data {
            return;
        }

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
        if self.inner.discard_tracing_data {
            return;
        }

        let meta = event.metadata();
        let mut debug_annotations = debug_annotations::ProtoDebugAnnotations::default();
        event.record(&mut debug_annotations);
        self.report_counters(meta, debug_annotations.take_counters());

        if !debug_annotations.suppress_event() {
            let (track_uuid, sequence_id, _) = self.pick_trace_track_sequence();
            self.report_event(meta, debug_annotations, track_uuid, sequence_id);
        }
    }

    fn on_enter(&self, id: &span::Id, ctx: layer::Context<'_, S>) {
        if self.inner.discard_tracing_data {
            return;
        }

        let span = ctx.span(id).expect("span to be found (this is a bug)");

        let (track_uuid, sequence_id, flavor) = self.pick_trace_track_sequence();
        let meta = span.metadata();

        if flavor == flavor::Flavor::Sync {
            span.extensions_mut().replace(track_uuid);
            span.extensions_mut().replace(sequence_id);

            let debug_annotations = span
                .extensions_mut()
                .get_mut::<debug_annotations::ProtoDebugAnnotations>()
                .map(mem::take)
                .unwrap_or_default();
            self.report_slice_begin(meta, track_uuid, sequence_id, debug_annotations);
        }
    }

    fn on_exit(&self, id: &tracing::Id, ctx: layer::Context<'_, S>) {
        if self.inner.discard_tracing_data {
            return;
        }

        let span = ctx.span(id).expect("span to be found (this is a bug)");

        let meta = span.metadata();
        let (track_uuid, sequence_id, flavor) = self.pick_trace_track_sequence();
        let extensions = span.extensions();

        if flavor == flavor::Flavor::Sync {
            self.report_slice_end(meta, track_uuid, sequence_id, extensions);
        }
    }

    fn on_close(&self, id: tracing::Id, ctx: layer::Context<'_, S>) {
        if self.inner.discard_tracing_data {
            return;
        }

        let span = ctx.span(&id).expect("span to be found (this is a bug)");

        let meta = span.metadata();
        let (track_uuid, sequence_id, flavor) = self.pick_trace_track_sequence();
        let extensions = span.extensions();

        if flavor == flavor::Flavor::Async {
            self.report_slice_end(meta, track_uuid, sequence_id, extensions);
        }
    }
}

impl<'c, W> Builder<'c, W>
where
    W: for<'w> fmt::MakeWriter<'w> + Send + Sync + 'static,
{
    #[allow(unused_variables)]
    fn new(config_bytes: borrow::Cow<'c, [u8]>, writer: W) -> Self {
        Self {
            #[cfg(feature = "sdk")]
            config_bytes,
            writer,
            drop_flush_timeout: time::Duration::from_millis(100),
            drop_poll_timeout: time::Duration::from_millis(100),
            background_flush_timeout: time::Duration::from_millis(100),
            background_poll_timeout: time::Duration::from_millis(100),
            background_poll_interval: time::Duration::from_millis(100),
            force_flavor: None,
            delay_slice_begin: false,
            discard_tracing_data: false,
            create_async_tracks: None,
            enable_in_process: true,
            enable_system: false,
            name: "rust_tracing",
            _phantom: marker::PhantomData,
        }
    }

    /// Set the name for perfetto to producer. This name will have to be specified in a data source.
    pub fn with_name(mut self, name: &'c str) -> Self {
        self.name = name;
        self
    }

    /// If `Some`, force the specified trace flavor. If `None`, use heuristics
    /// for every created span to determine the flavor.
    pub fn with_force_flavor(mut self, force_flavor: Option<flavor::Flavor>) -> Self {
        self.force_flavor = force_flavor;
        self
    }

    /// Delays the emission of `SliceBegin` events.
    ///
    /// This allows more fields to be recorded onto the span before it is
    /// written to the trace file, but the trade-off is that spans that never
    /// end will not be reported to the trace at all.
    pub fn with_delay_slice_begin(mut self, delay_slice_begin: bool) -> Self {
        self.delay_slice_begin = delay_slice_begin;
        self
    }

    /// Discards all tracing data coming from the `tracing` crate.
    ///
    /// This might sound crazy but might make sense if the only purpose of this
    /// layer is to collect metrics from the Perfetto `traced` daemon and
    /// re-emit them.
    pub fn with_discard_tracing_data(mut self, discard_tracing_data: bool) -> Self {
        self.discard_tracing_data = discard_tracing_data;
        self
    }

    /// Creates a separate track in the Perfetto UI for every "thread" even in
    /// async mode. This means there will be a separate "track" for each OS
    /// thread *and* Tokio task, but they will be light-weight tracks without as
    /// much info as a thread track would have. Instead, all of these tracks
    /// will have the specified name.
    ///
    /// This makes it easier for Perfetto to keep the necessary context window
    /// per track for some async events to not get dropped. However, the
    /// trade-off is some data inflation and a laggier UI as a result.
    pub fn with_create_async_tracks(mut self, create_async_tracks: Option<String>) -> Self {
        self.create_async_tracks = create_async_tracks;
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
    #[allow(unused_variables)]
    fn flush(
        &self,
        flush_timeout: time::Duration,
        poll_timeout: time::Duration,
    ) -> error::Result<()> {
        #[cfg(feature = "sdk")]
        {
            use std::io::Write as _;

            let data = ffi_utils::with_session_lock(&*self.ffi_session, |session| {
                ffi_utils::do_flush(session, flush_timeout)?;
                let data = ffi_utils::do_poll_traces(session, poll_timeout)?;
                Ok(data)
            })?;
            self.writer.make_writer().write_all(&*data.data)?;
        }

        Ok(())
    }

    fn stop(&self) -> error::Result<()> {
        #[cfg(feature = "sdk")]
        {
            // Can't use ffi_utils::with_session_lock here because we want to take the
            // session object
            let mut session = self
                .ffi_session
                .lock()
                .map_err(|_| error::Error::PoisonedMutex)?;
            if let Some(mut session) = session.take() {
                session.pin_mut().stop();
            }
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

#[cfg(not(feature = "sdk"))]
static HAS_BOOTTIME: sync::LazyLock<bool> = sync::LazyLock::new(|| {
    #[cfg(any(
        target_os = "linux",
        target_os = "android",
        target_os = "emscripten",
        target_os = "fuchsia"
    ))]
    {
        nix::time::clock_gettime(nix::time::ClockId::CLOCK_BOOTTIME).is_ok()
    }
    #[cfg(not(any(
        target_os = "linux",
        target_os = "android",
        target_os = "emscripten",
        target_os = "fuchsia"
    )))]
    {
        false
    }
});

#[cfg(feature = "sdk")]
fn trace_time_ns() -> u64 {
    ffi::trace_time_ns()
}

#[cfg(not(feature = "sdk"))]
fn trace_time_ns() -> u64 {
    use nix::time;
    #[cfg(any(
        target_os = "linux",
        target_os = "android",
        target_os = "emscripten",
        target_os = "fuchsia"
    ))]
    {
        if *HAS_BOOTTIME {
            std::time::Duration::from(time::clock_gettime(time::ClockId::CLOCK_BOOTTIME).unwrap())
                .as_nanos() as u64
        } else {
            std::time::Duration::from(time::clock_gettime(time::ClockId::CLOCK_MONOTONIC).unwrap())
                .as_nanos() as u64
        }
    }
    #[cfg(not(any(
        target_os = "linux",
        target_os = "android",
        target_os = "emscripten",
        target_os = "fuchsia"
    )))]
    {
        std::time::Duration::from(time::clock_gettime(time::ClockId::CLOCK_MONOTONIC).unwrap())
            .as_nanos() as u64
    }
}

#[cfg(feature = "sdk")]
fn trace_clock_id() -> u32 {
    ffi::trace_clock_id()
}

#[cfg(not(feature = "sdk"))]
fn trace_clock_id() -> u32 {
    #[cfg(any(
        target_os = "linux",
        target_os = "android",
        target_os = "emscripten",
        target_os = "fuchsia"
    ))]
    {
        if *HAS_BOOTTIME {
            schema::BuiltinClock::Boottime as u32
        } else {
            schema::BuiltinClock::Monotonic as u32
        }
    }
    #[cfg(not(any(
        target_os = "linux",
        target_os = "android",
        target_os = "emscripten",
        target_os = "fuchsia"
    )))]
    {
        schema::BuiltinClock::Monotonic as u32
    }
}

#[cfg(feature = "sdk")]
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
