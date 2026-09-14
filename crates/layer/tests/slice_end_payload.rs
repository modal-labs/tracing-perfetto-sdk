//! Slice ends should carry only what Perfetto actually reads off them: no
//! name, no source location, and no repeat of annotations that already went
//! out with the matching begin.

use std::{env, fs, time};

use schema::{trace_config, trace_packet, track_event};
use tracing_perfetto_sdk_layer as layer;
use tracing_perfetto_sdk_schema as schema;

#[test]
fn slice_ends_are_bare() -> anyhow::Result<()> {
    use prost::Message as _;
    use tracing_subscriber::layer::SubscriberExt as _;

    let trace_path = env::temp_dir().join("slice_end_payload.pftrace");
    let file = fs::File::create(&trace_path)?;
    let (writer, writer_guard) = tracing_appender::non_blocking(file);
    // Delaying the begin is the configuration that used to emit every
    // annotation twice, once on the begin and again on the end.
    let perfetto_layer = layer::NativeLayer::from_config(trace_config(), writer)
        .with_force_flavor(Some(layer::Flavor::Async))
        .with_delay_slice_begin(true)
        .build()?;

    let subscriber = tracing_subscriber::registry().with(perfetto_layer.clone());

    tracing::subscriber::with_default(subscriber, || {
        let span = tracing::info_span!("work", answer = 42);
        drop(span);
    });

    perfetto_layer.flush(
        time::Duration::from_millis(1000),
        time::Duration::from_millis(1000),
    )?;
    drop(writer_guard);
    perfetto_layer.stop()?;

    let trace = schema::Trace::decode(&*fs::read(&trace_path)?)?;

    let mut annotated = 0;
    let mut ends = 0;
    for packet in &trace.packet {
        let Some(trace_packet::Data::TrackEvent(ref event)) = packet.data else {
            continue;
        };
        if event.debug_annotations.iter().any(|annotation| {
            annotation.name_field
                == Some(schema::debug_annotation::NameField::Name("answer".to_owned()))
        }) {
            annotated += 1;
        }
        if event.r#type() == track_event::Type::SliceEnd {
            ends += 1;
            assert!(
                event.debug_annotations.is_empty(),
                "annotations already went out with the begin"
            );
            assert!(event.name_field.is_none(), "Perfetto ignores names on ends");
            assert!(
                event.source_location_field.is_none(),
                "the source location already went out with the begin"
            );
        }
    }

    assert_eq!(ends, 1, "expected exactly one slice end");
    assert_eq!(annotated, 1, "the annotation should be recorded exactly once");

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
