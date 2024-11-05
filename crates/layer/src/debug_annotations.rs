//! Internal module used to collect span attrs as Perfetto SDK
//! `DebugAnnotation`s

use std::mem;

use schema::debug_annotation;
use tracing::field;
use tracing_perfetto_sdk_schema as schema;
use tracing_perfetto_sdk_schema::track_event;
use tracing_perfetto_sdk_sys::ffi;

const COUNTER_FIELD_PREFIX: &str = "counter.";
const SUPPRESS_EVENT_FIELD: &str = "perfetto.suppress_event";

#[derive(Default)]
pub struct FFIDebugAnnotations {
    counters: Vec<Counter>,
    suppress_event: bool,
    strings: Vec<ffi::DebugStringAnnotation>,
    bools: Vec<ffi::DebugBoolAnnotation>,
    ints: Vec<ffi::DebugIntAnnotation>,
    doubles: Vec<ffi::DebugDoubleAnnotation>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct ProtoDebugAnnotations {
    counters: Vec<Counter>,
    suppress_event: bool,
    annotations: Vec<schema::DebugAnnotation>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Counter {
    pub name: &'static str,
    pub unit: Option<&'static str>,
    pub value: CounterValue,
}

#[derive(Clone, Debug, PartialEq)]
pub enum CounterValue {
    Float(f64),
    Int(i64),
}

impl FFIDebugAnnotations {
    pub fn as_ffi(&self) -> ffi::DebugAnnotations {
        ffi::DebugAnnotations {
            strings: self.strings.as_slice(),
            bools: self.bools.as_slice(),
            ints: self.ints.as_slice(),
            doubles: self.doubles.as_slice(),
        }
    }

    pub fn suppress_event(&self) -> bool {
        self.suppress_event
    }
}

impl field::Visit for FFIDebugAnnotations {
    fn record_f64(&mut self, field: &field::Field, value: f64) {
        if !populate_counter(&mut self.counters, field, value) {
            self.doubles.push(ffi::DebugDoubleAnnotation {
                key: field.name(),
                value,
            });
        }
    }

    fn record_i64(&mut self, field: &field::Field, value: i64) {
        if !populate_counter(&mut self.counters, field, value) {
            self.ints.push(ffi::DebugIntAnnotation {
                key: field.name(),
                value,
            });
        }
    }

    fn record_u64(&mut self, field: &field::Field, value: u64) {
        if !populate_counter(&mut self.counters, field, value) {
            if let Ok(v) = i64::try_from(value) {
                self.ints.push(ffi::DebugIntAnnotation {
                    key: field.name(),
                    value: v,
                });
            } else {
                self.strings.push(ffi::DebugStringAnnotation {
                    key: field.name(),
                    value: value.to_string(),
                });
            }
        }
    }

    fn record_i128(&mut self, field: &field::Field, value: i128) {
        if !populate_counter(&mut self.counters, field, value) {
            self.strings.push(ffi::DebugStringAnnotation {
                key: field.name(),
                value: value.to_string(),
            });
        }
    }

    fn record_u128(&mut self, field: &field::Field, value: u128) {
        if !populate_counter(&mut self.counters, field, value) {
            self.strings.push(ffi::DebugStringAnnotation {
                key: field.name(),
                value: value.to_string(),
            });
        }
    }

    fn record_bool(&mut self, field: &field::Field, value: bool) {
        if field.name() == SUPPRESS_EVENT_FIELD {
            self.suppress_event = value;
        } else {
            self.bools.push(ffi::DebugBoolAnnotation {
                key: field.name(),
                value,
            });
        }
    }

    fn record_str(&mut self, field: &field::Field, value: &str) {
        self.strings.push(ffi::DebugStringAnnotation {
            key: field.name(),
            value: value.to_owned(),
        });
    }

    fn record_error(&mut self, field: &field::Field, value: &(dyn std::error::Error + 'static)) {
        self.strings.push(ffi::DebugStringAnnotation {
            key: field.name(),
            value: format!("{:#}", value),
        });
    }

    fn record_debug(&mut self, field: &field::Field, value: &dyn std::fmt::Debug) {
        self.strings.push(ffi::DebugStringAnnotation {
            key: field.name(),
            value: format!("{:?}", value),
        });
    }
}

impl ProtoDebugAnnotations {
    pub fn into_proto(self) -> Vec<schema::DebugAnnotation> {
        self.annotations
    }

    pub fn take_counters(&mut self) -> Vec<Counter> {
        mem::take(&mut self.counters)
    }

    pub fn suppress_event(&self) -> bool {
        self.suppress_event
    }

    fn name_field(field: &field::Field) -> Option<debug_annotation::NameField> {
        Some(debug_annotation::NameField::Name(field.name().to_string()))
    }
}

impl field::Visit for ProtoDebugAnnotations {
    fn record_f64(&mut self, field: &field::Field, value: f64) {
        if !populate_counter(&mut self.counters, field, value) {
            self.annotations.push(schema::DebugAnnotation {
                name_field: Self::name_field(field),
                value: Some(debug_annotation::Value::DoubleValue(value)),
                ..Default::default()
            });
        }
    }

    fn record_i64(&mut self, field: &field::Field, value: i64) {
        if !populate_counter(&mut self.counters, field, value) {
            self.annotations.push(schema::DebugAnnotation {
                name_field: Self::name_field(field),
                value: Some(debug_annotation::Value::IntValue(value)),
                ..Default::default()
            });
        }
    }

    fn record_u64(&mut self, field: &field::Field, value: u64) {
        if !populate_counter(&mut self.counters, field, value) {
            if let Ok(v) = i64::try_from(value) {
                self.annotations.push(schema::DebugAnnotation {
                    name_field: Self::name_field(field),
                    value: Some(debug_annotation::Value::IntValue(v)),
                    ..Default::default()
                });
            } else {
                self.annotations.push(schema::DebugAnnotation {
                    name_field: Self::name_field(field),
                    value: Some(debug_annotation::Value::StringValue(value.to_string())),
                    ..Default::default()
                });
            }
        }
    }

    fn record_i128(&mut self, field: &field::Field, value: i128) {
        if !populate_counter(&mut self.counters, field, value) {
            self.annotations.push(schema::DebugAnnotation {
                name_field: Self::name_field(field),
                value: Some(debug_annotation::Value::StringValue(value.to_string())),
                ..Default::default()
            });
        }
    }

    fn record_u128(&mut self, field: &field::Field, value: u128) {
        if !populate_counter(&mut self.counters, field, value) {
            self.annotations.push(schema::DebugAnnotation {
                name_field: Self::name_field(field),
                value: Some(debug_annotation::Value::StringValue(value.to_string())),
                ..Default::default()
            });
        }
    }

    fn record_bool(&mut self, field: &field::Field, value: bool) {
        if field.name() == SUPPRESS_EVENT_FIELD {
            self.suppress_event = value;
        } else {
            self.annotations.push(schema::DebugAnnotation {
                name_field: Self::name_field(field),
                value: Some(debug_annotation::Value::BoolValue(value)),
                ..Default::default()
            });
        }
    }

    fn record_str(&mut self, field: &field::Field, value: &str) {
        self.annotations.push(schema::DebugAnnotation {
            name_field: Self::name_field(field),
            value: Some(debug_annotation::Value::StringValue(value.to_owned())),
            ..Default::default()
        });
    }

    fn record_error(&mut self, field: &field::Field, value: &(dyn std::error::Error + 'static)) {
        self.annotations.push(schema::DebugAnnotation {
            name_field: Self::name_field(field),
            value: Some(debug_annotation::Value::StringValue(format!("{:#}", value))),
            ..Default::default()
        });
    }

    fn record_debug(&mut self, field: &field::Field, value: &dyn std::fmt::Debug) {
        self.annotations.push(schema::DebugAnnotation {
            name_field: Self::name_field(field),
            value: Some(debug_annotation::Value::StringValue(format!("{:?}", value))),
            ..Default::default()
        });
    }
}

impl CounterValue {
    pub fn to_proto(self) -> track_event::CounterValueField {
        match self {
            CounterValue::Float(v) => track_event::CounterValueField::DoubleCounterValue(v),
            CounterValue::Int(v) => track_event::CounterValueField::CounterValue(v),
        }
    }
}

impl From<f64> for CounterValue {
    fn from(value: f64) -> Self {
        CounterValue::Float(value)
    }
}

impl From<i64> for CounterValue {
    fn from(value: i64) -> Self {
        CounterValue::Int(value)
    }
}

impl From<u64> for CounterValue {
    fn from(value: u64) -> Self {
        if let Ok(v) = i64::try_from(value) {
            CounterValue::Int(v)
        } else {
            CounterValue::Float(value as f64)
        }
    }
}

impl From<i128> for CounterValue {
    fn from(value: i128) -> Self {
        if let Ok(v) = i64::try_from(value) {
            CounterValue::Int(v)
        } else {
            CounterValue::Float(value as f64)
        }
    }
}

impl From<u128> for CounterValue {
    fn from(value: u128) -> Self {
        if let Ok(v) = i64::try_from(value) {
            CounterValue::Int(v)
        } else {
            CounterValue::Float(value as f64)
        }
    }
}

fn populate_counter(
    dest_counters: &mut Vec<Counter>,
    field: &field::Field,
    value: impl Into<CounterValue>,
) -> bool {
    if let Some(name) = field.name().strip_prefix(COUNTER_FIELD_PREFIX) {
        let (name, unit) = name
            .rsplit_once('.')
            .map(|(n, u)| (n, Some(u)))
            .unwrap_or_else(|| (name, None));

        let value = value.into();
        dest_counters.push(Counter { name, value, unit });
        true
    } else {
        false
    }
}
