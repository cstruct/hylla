//! Formatting logs as JSON, with the trace and span IDs of the current span.

use std::{fmt, time::SystemTime};

use opentelemetry::trace::TraceContextExt;
use serde_json::{Map, Value};
use tracing::{Event, Subscriber, field::Field};
use tracing_subscriber::{
    fmt::{FmtContext, FormatEvent, FormatFields, format::Writer},
    registry::LookupSpan,
};

// r[impl telemetry.logs.json]
/// Formats each event as a JSON object on a line of its own: its time, level, target,
/// message and fields, the spans it's in, and the trace and span IDs when traced.
pub struct Json;

impl<S, N> FormatEvent<S, N> for Json
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    N: for<'a> FormatFields<'a> + 'static,
{
    fn format_event(
        &self,
        ctx: &FmtContext<'_, S, N>,
        mut writer: Writer<'_>,
        event: &Event<'_>,
    ) -> fmt::Result {
        let metadata = event.metadata();
        let mut object = Map::new();
        object.insert(
            "timestamp".into(),
            humantime::format_rfc3339_micros(SystemTime::now())
                .to_string()
                .into(),
        );
        object.insert("level".into(), metadata.level().as_str().into());
        object.insert("target".into(), metadata.target().into());
        let mut fields = Map::new();
        event.record(&mut Visitor(&mut fields));
        if let Some(message) = fields.remove("message") {
            object.insert("message".into(), message);
        }
        if !fields.is_empty() {
            object.insert("fields".into(), fields.into());
        }
        if let Some(scope) = ctx.event_scope() {
            let spans = scope
                .from_root()
                .map(|span| Value::from(span.name()))
                .collect::<Vec<_>>();
            object.insert("spans".into(), spans.into());
        }
        // The current OpenTelemetry context is the event's span's, when traces are on.
        let context = opentelemetry::Context::current();
        let span = context.span();
        let span_context = span.span_context();
        if span_context.is_valid() {
            object.insert(
                "trace_id".into(),
                span_context.trace_id().to_string().into(),
            );
            object.insert("span_id".into(), span_context.span_id().to_string().into());
        }
        writeln!(writer, "{}", Value::Object(object))
    }
}

struct Visitor<'a>(&'a mut Map<String, Value>);

impl tracing::field::Visit for Visitor<'_> {
    fn record_bool(&mut self, field: &Field, value: bool) {
        self.0.insert(field.name().into(), value.into());
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        self.0.insert(field.name().into(), value.into());
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        self.0.insert(field.name().into(), value.into());
    }

    fn record_f64(&mut self, field: &Field, value: f64) {
        self.0.insert(field.name().into(), value.into());
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        self.0.insert(field.name().into(), value.into());
    }

    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        self.0
            .insert(field.name().into(), format!("{value:?}").into());
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use opentelemetry::trace::TracerProvider;
    use opentelemetry_sdk::trace::SdkTracerProvider;
    use serde_json::Value;
    use tracing_subscriber::{fmt::MakeWriter, layer::SubscriberExt};

    use super::Json;

    #[derive(Clone, Default)]
    struct Buffer(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for Buffer {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> MakeWriter<'a> for Buffer {
        type Writer = Buffer;

        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    // r[verify telemetry.logs.json]
    #[test]
    fn json_lines_carry_fields_spans_and_trace_ids() {
        let buffer = Buffer::default();
        let provider = SdkTracerProvider::builder().build();
        let subscriber = tracing_subscriber::registry()
            .with(tracing_opentelemetry::layer().with_tracer(provider.tracer("test")))
            .with(
                tracing_subscriber::fmt::layer()
                    .event_format(Json)
                    .with_writer(buffer.clone()),
            );
        tracing::subscriber::with_default(subscriber, || {
            tracing::warn!(backend = "assets", "outside spans");
            tracing::info_span!("request").in_scope(|| {
                tracing::info_span!("metadata").in_scope(|| {
                    tracing::info!(size = 3000u64, "fetched");
                });
            });
        });

        let output = String::from_utf8(buffer.0.lock().unwrap().clone()).unwrap();
        let lines = output
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(lines.len(), 2, "{output}");
        assert_eq!(lines[0]["level"], "WARN");
        assert_eq!(lines[0]["message"], "outside spans");
        assert_eq!(lines[0]["fields"]["backend"], "assets");
        assert!(lines[0].get("trace_id").is_none());
        assert!(lines[0]["timestamp"].as_str().unwrap().ends_with('Z'));

        assert_eq!(lines[1]["level"], "INFO");
        assert_eq!(lines[1]["target"], "hylla_server::logs::tests");
        assert_eq!(lines[1]["fields"]["size"], 3000);
        assert_eq!(
            lines[1]["spans"],
            serde_json::json!(["request", "metadata"])
        );
        assert_eq!(lines[1]["trace_id"].as_str().unwrap().len(), 32);
        assert_eq!(lines[1]["span_id"].as_str().unwrap().len(), 16);
    }
}
