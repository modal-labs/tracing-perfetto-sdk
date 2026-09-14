//! Perfetto treats a `trusted_packet_sequence_id` as one writer emitting
//! packets in order, so every sequence in the trace has to carry
//! non-decreasing timestamps, and no sequence may use the reserved id 0.

use std::collections::{HashMap, HashSet};
use std::{env, fs, thread, time};

use schema::{trace_config, trace_packet};
use tracing_perfetto_sdk_layer as layer;
use tracing_perfetto_sdk_schema as schema;

const THREADS: usize = 8;
const COUNTER_UPDATES_PER_THREAD: usize = 64;

#[test]
fn sequences_are_single_writer() -> anyhow::Result<()> {
    use prost::Message as _;
    use tracing_subscriber::layer::SubscriberExt as _;

    let trace_path = env::temp_dir().join("sequence_ids.pftrace");
    let file = fs::File::create(&trace_path)?;
    let (writer, writer_guard) = tracing_appender::non_blocking(file);
    let perfetto_layer = layer::NativeLayer::from_config(trace_config(), writer).build()?;

    let subscriber = tracing_subscriber::registry().with(perfetto_layer.clone());
    tracing::subscriber::set_global_default(subscriber)?;

    // Several threads hammering the *same* counter is the interesting case:
    // the counter track is shared, so the sequence the updates land on must
    // still be per-writer.
    thread::scope(|scope| {
        for _ in 0..THREADS {
            scope.spawn(|| {
                for i in 0..COUNTER_UPDATES_PER_THREAD {
                    tracing::info!(counter.shared.count = i as i64, "tick");
                    thread::yield_now();
                }
            });
        }
    });

    perfetto_layer.flush(
        time::Duration::from_millis(1000),
        time::Duration::from_millis(1000),
    )?;
    drop(writer_guard);
    perfetto_layer.stop()?;

    let trace = schema::Trace::decode(&*fs::read(&trace_path)?)?;

    let mut last_timestamp: HashMap<u32, u64> = HashMap::new();
    let mut sequences = HashSet::new();
    for packet in &trace.packet {
        if !matches!(packet.data, Some(trace_packet::Data::TrackEvent(_))) {
            continue;
        }
        let Some(trace_packet::OptionalTrustedPacketSequenceId::TrustedPacketSequenceId(
            sequence_id,
        )) = packet.optional_trusted_packet_sequence_id
        else {
            panic!("track event packet without a sequence id");
        };
        assert_ne!(sequence_id, 0, "0 is reserved to mean an unset sequence id");
        sequences.insert(sequence_id);

        let timestamp = packet.timestamp();
        let previous = last_timestamp.insert(sequence_id, timestamp).unwrap_or(0);
        assert!(
            timestamp >= previous,
            "sequence {sequence_id} went backwards in time, from {previous} to {timestamp}, \
             so it has more than one writer"
        );
    }

    assert_eq!(
        sequences.len(),
        THREADS,
        "expected one sequence per writing thread, got {sequences:?}"
    );

    Ok(())
}

fn trace_config() -> schema::TraceConfig {
    schema::TraceConfig {
        buffers: vec![trace_config::BufferConfig {
            size_kb: Some(4096),
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
