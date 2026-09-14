#![cfg(feature = "tokio")]
//! An instant event belongs on the same track as the slice for the span it
//! was recorded in, even when it is emitted from a different execution
//! context than the one the span is being recorded on.

use std::{env, fs, thread, time};

use schema::{trace_config, trace_packet, track_event};
use tracing_perfetto_sdk_layer as layer;
use tracing_perfetto_sdk_schema as schema;

#[tokio::test(flavor = "multi_thread")]
async fn events_land_on_their_span_track() -> anyhow::Result<()> {
    use prost::Message as _;
    use tracing_subscriber::layer::SubscriberExt as _;

    let trace_path = env::temp_dir().join("event_tracks.pftrace");
    let file = fs::File::create(&trace_path)?;
    let (writer, writer_guard) = tracing_appender::non_blocking(file);
    let perfetto_layer = layer::NativeLayer::from_config(trace_config(), writer).build()?;

    let subscriber = tracing_subscriber::registry().with(perfetto_layer.clone());
    tracing::subscriber::set_global_default(subscriber)?;

    // Created on a Tokio task, so its slice goes on the Tokio track.
    let span = tokio::spawn(async { tracing::info_span!("owner") }).await?;

    // Recorded against that span from a plain OS thread, whose own ambient
    // track is a thread track. Naming the parent explicitly keeps the span
    // from being entered, so the only thing that can route this event
    // correctly is the span it points at.
    let event_parent = span.clone();
    thread::spawn(move || {
        tracing::info!(parent: &event_parent, "instant");
    })
    .join()
    .unwrap();

    drop(span);

    perfetto_layer.flush(
        time::Duration::from_millis(1000),
        time::Duration::from_millis(1000),
    )?;
    drop(writer_guard);
    perfetto_layer.stop()?;

    let trace = schema::Trace::decode(&*fs::read(&trace_path)?)?;

    let mut span_track = None;
    let mut event_track = None;
    for packet in &trace.packet {
        let Some(trace_packet::Data::TrackEvent(ref event)) = packet.data else {
            continue;
        };
        match event.r#type() {
            // Events are named after their source location rather than their
            // message, so match the slice on its span name and the instant on
            // simply being the only one in the trace.
            track_event::Type::SliceBegin
                if event.name_field == Some(track_event::NameField::Name("owner".to_owned())) =>
            {
                span_track = Some(event.track_uuid())
            }
            track_event::Type::Instant => event_track = Some(event.track_uuid()),
            _ => {}
        }
    }

    eprintln!(
        "DUMP: {:?}",
        trace
            .packet
            .iter()
            .filter_map(|p| match p.data {
                Some(trace_packet::Data::TrackEvent(ref e)) =>
                    Some((e.r#type(), e.name_field.clone(), e.track_uuid())),
                _ => None,
            })
            .collect::<Vec<_>>()
    );
    let span_track = span_track.expect("a slice for the owning span");
    let event_track = event_track.expect("an instant event");
    assert_eq!(
        event_track, span_track,
        "the instant was recorded on a different track than the span it belongs to, so it renders \
         outside its own slice"
    );

    Ok(())
}

fn trace_config() -> schema::TraceConfig {
    schema::TraceConfig {
        buffers: vec![trace_config::BufferConfig {
            size_kb: Some(1024),
            ..Default::default()
        }],
        data_sources: vec![trace_config::DataSource {
            config: Some(schema::DataSourceConfig {
                name: Some("rust_tracing".into()),
                ..Default::default()
            }),
            ..Default::default()
        }],
        ..Default::default()
    }
}
