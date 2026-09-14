//! Nested spans should be linked by a Perfetto flow, so that a child slice can
//! be traced back to the parent that created it even when the two end up on
//! different tracks.

use std::collections::HashMap;
use std::{env, fs, thread, time};

use schema::{trace_config, trace_packet, track_event};
use tracing_perfetto_sdk_layer as layer;
use tracing_perfetto_sdk_schema as schema;

#[test]
fn child_slices_join_the_parent_flow() -> anyhow::Result<()> {
    use prost::Message as _;
    use tracing_subscriber::layer::SubscriberExt as _;

    let trace_path = env::temp_dir().join("span_flows.pftrace");
    let file = fs::File::create(&trace_path)?;
    let (writer, writer_guard) = tracing_appender::non_blocking(file);
    let perfetto_layer = layer::NativeLayer::from_config(trace_config(), writer).build()?;

    let subscriber = tracing_subscriber::registry().with(perfetto_layer.clone());

    tracing::subscriber::with_default(subscriber, || {
        let parent = tracing::info_span!("parent");
        let _parent_guard = parent.enter();
        for _ in 0..2 {
            let child = tracing::info_span!("child");
            let _child_guard = child.enter();
            thread::sleep(time::Duration::from_millis(1));
        }
    });

    perfetto_layer.flush(
        time::Duration::from_millis(1000),
        time::Duration::from_millis(1000),
    )?;
    drop(writer_guard);
    perfetto_layer.stop()?;

    let trace = schema::Trace::decode(&*fs::read(&trace_path)?)?;

    // Collect, per slice-begin event name, the flows it advertises.
    let mut flows_by_name: HashMap<String, Vec<u64>> = HashMap::new();
    let mut terminated = Vec::new();
    for packet in &trace.packet {
        let Some(trace_packet::Data::TrackEvent(ref event)) = packet.data else {
            continue;
        };
        match event.r#type() {
            track_event::Type::SliceBegin => {
                let name = match event.name_field {
                    Some(track_event::NameField::Name(ref name)) => name.clone(),
                    _ => continue,
                };
                flows_by_name
                    .entry(name)
                    .or_default()
                    .extend(event.flow_ids.iter().copied());
            }
            track_event::Type::SliceEnd => terminated.extend(event.terminating_flow_ids.iter()),
            _ => {}
        }
    }

    let parent_flows = flows_by_name.get("parent").expect("a parent slice");
    let child_flows = flows_by_name.get("child").expect("child slices");

    // The parent advertises exactly one flow: its own.
    assert_eq!(parent_flows.len(), 1, "parent should source a single flow");
    let parent_flow = parent_flows[0];

    // Both children join it, in addition to sourcing their own.
    let joining_children = child_flows
        .iter()
        .filter(|flow| **flow == parent_flow)
        .count();
    assert_eq!(
        joining_children, 2,
        "both child slices should join the parent's flow, giving Perfetto an \
         arrow from the parent into each child"
    );

    // Every flow that was sourced is also explicitly terminated, so the
    // registry is free to recycle the underlying span ids.
    for flow in parent_flows.iter().chain(child_flows) {
        assert!(
            terminated.contains(flow),
            "flow {flow} was never terminated"
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
