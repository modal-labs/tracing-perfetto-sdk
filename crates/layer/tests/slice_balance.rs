#![cfg(feature = "tokio")]
//! A span may be created in one execution context and entered in another, for
//! example created on a Tokio task and then entered on a plain OS thread. When
//! that happens the span must still be reported with exactly one slice end per
//! slice begin, on the track the begin was emitted to.

use std::collections::HashMap;
use std::{env, fs, thread, time};

use schema::{trace_config, trace_packet, track_event};
use tracing_perfetto_sdk_layer as layer;
use tracing_perfetto_sdk_schema as schema;

#[tokio::test(flavor = "multi_thread")]
async fn slices_balance_across_execution_contexts() -> anyhow::Result<()> {
    use prost::Message as _;
    use tracing_subscriber::layer::SubscriberExt as _;

    let trace_path = env::temp_dir().join("slice_balance.pftrace");
    let file = fs::File::create(&trace_path)?;
    let (writer, writer_guard) = tracing_appender::non_blocking(file);
    let perfetto_layer = layer::NativeLayer::from_config(trace_config(), writer).build()?;

    let subscriber = tracing_subscriber::registry().with(perfetto_layer.clone());
    tracing::subscriber::set_global_default(subscriber)?;

    // Created inside a real Tokio task, so the layer classifies it as async.
    let span = tokio::spawn(async { tracing::info_span!("created_on_task") }).await?;

    // ...but entered and exited on a plain OS thread, which on its own looks
    // synchronous to the layer.
    let entered_elsewhere = span.clone();
    thread::spawn(move || {
        let _guard = entered_elsewhere.enter();
        thread::sleep(time::Duration::from_millis(10));
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

    // Net slice depth per track: +1 for every begin, -1 for every end. Every
    // track has to come back to zero, and must never dip below it.
    let mut depth_by_track: HashMap<u64, i64> = HashMap::new();
    for packet in &trace.packet {
        let Some(trace_packet::Data::TrackEvent(ref event)) = packet.data else {
            continue;
        };
        let track_uuid = event.track_uuid();
        let depth = depth_by_track.entry(track_uuid).or_default();
        match event.r#type() {
            track_event::Type::SliceBegin => *depth += 1,
            track_event::Type::SliceEnd => *depth -= 1,
            _ => continue,
        }
        assert!(
            *depth >= 0,
            "track {track_uuid} saw a slice end without a matching begin"
        );
    }

    assert!(
        depth_by_track.values().any(|depth| *depth == 0),
        "expected at least one track carrying the span's slices"
    );
    for (track_uuid, depth) in &depth_by_track {
        assert_eq!(
            *depth, 0,
            "track {track_uuid} ended with {depth} unterminated slice(s)"
        );
    }

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
