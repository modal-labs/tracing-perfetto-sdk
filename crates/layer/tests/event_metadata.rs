//! Track events should carry the `tracing` metadata Perfetto can actually
//! use: the target and level as categories, and, for log events, the message
//! as the displayed name.

use std::{env, fs, time};

use schema::{trace_config, trace_packet, track_event};
use tracing_perfetto_sdk_layer as layer;
use tracing_perfetto_sdk_schema as schema;

#[test]
fn events_carry_target_level_and_message() -> anyhow::Result<()> {
    use prost::Message as _;
    use tracing_subscriber::layer::SubscriberExt as _;

    let trace_path = env::temp_dir().join("event_metadata.pftrace");
    let file = fs::File::create(&trace_path)?;
    let (writer, writer_guard) = tracing_appender::non_blocking(file);
    let perfetto_layer = layer::NativeLayer::from_config(trace_config(), writer).build()?;

    let subscriber = tracing_subscriber::registry().with(perfetto_layer.clone());

    tracing::subscriber::with_default(subscriber, || {
        let span = tracing::info_span!(target: "my_target", "my_span");
        let _guard = span.enter();
        tracing::warn!(target: "my_target", "something happened");
    });

    perfetto_layer.flush(
        time::Duration::from_millis(1000),
        time::Duration::from_millis(1000),
    )?;
    drop(writer_guard);
    perfetto_layer.stop()?;

    let trace = schema::Trace::decode(&*fs::read(&trace_path)?)?;

    let mut slice = None;
    let mut instant = None;
    for packet in &trace.packet {
        let Some(trace_packet::Data::TrackEvent(ref event)) = packet.data else {
            continue;
        };
        match event.r#type() {
            track_event::Type::SliceBegin => slice = Some(event),
            track_event::Type::Instant => instant = Some(event),
            _ => {}
        }
    }

    let slice = slice.expect("a slice for the span");
    assert_eq!(slice.categories, ["my_target", "INFO"]);

    let instant = instant.expect("an instant for the event");
    assert_eq!(instant.categories, ["my_target", "WARN"]);
    assert_eq!(
        instant.name_field,
        Some(track_event::NameField::Name(
            "something happened".to_owned()
        )),
        "instants should be named after their message, not their source location"
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
