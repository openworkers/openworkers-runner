//! Adaptive span processor for tail-based sampling
//!
//! Filters spans at export time based on worker traffic, after all attributes
//! are set. This allows sampling decisions based on worker.id even when it's
//! recorded after span creation.

use opentelemetry::trace::{SpanContext, SpanId, TraceId};
use opentelemetry_sdk::export::trace::SpanData;
use opentelemetry_sdk::trace::{SpanProcessor, Context};
use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

/// Span processor that applies adaptive sampling at export time
pub struct AdaptiveSpanProcessor<T: SpanProcessor> {
    inner: T,
    state: Arc<RwLock<ProcessorState>>,
    min_rate: f64,
    max_rate: f64,
}

struct ProcessorState {
    worker_counts: HashMap<String, WorkerStats>,
}

struct WorkerStats {
    count: f64,
    last_update: Instant,
}

impl<T: SpanProcessor> AdaptiveSpanProcessor<T> {
    pub fn new(inner: T, min_rate: f64, max_rate: f64) -> Self {
        Self {
            inner,
            state: Arc::new(RwLock::new(ProcessorState {
                worker_counts: HashMap::new(),
            })),
            min_rate: min_rate.clamp(0.0, 1.0),
            max_rate: max_rate.clamp(0.0, 1.0),
        }
    }

    fn should_export(&self, span_data: &SpanData) -> bool {
        // Extract worker_id from span attributes
        let worker_id = span_data
            .attributes
            .iter()
            .find(|kv| kv.key.as_str() == "worker_id")
            .map(|kv| kv.value.to_string());

        let worker_id = match worker_id {
            Some(id) if !id.is_empty() && id != "Empty" => id,
            _ => return true, // Always export if no worker_id (system spans, etc.)
        };

        let sampling_rate = self.get_sampling_rate(&worker_id);

        // Hash-based decision using trace_id
        let trace_id = span_data.span_context.trace_id();
        let trace_id_bytes = trace_id.to_bytes();
        let hash = u64::from_be_bytes([
            trace_id_bytes[0],
            trace_id_bytes[1],
            trace_id_bytes[2],
            trace_id_bytes[3],
            trace_id_bytes[4],
            trace_id_bytes[5],
            trace_id_bytes[6],
            trace_id_bytes[7],
        ]);

        let threshold = (sampling_rate * u64::MAX as f64) as u64;
        hash <= threshold
    }

    fn get_sampling_rate(&self, worker_id: &str) -> f64 {
        let mut state = self.state.write().unwrap();
        let now = Instant::now();

        // Get or create worker stats
        let stats = state
            .worker_counts
            .entry(worker_id.to_string())
            .or_insert(WorkerStats {
                count: 0.0,
                last_update: now,
            });

        // Apply exponential decay (60s half-life)
        let elapsed = now.duration_since(stats.last_update).as_secs_f64();
        let decay_rate = 0.693 / 60.0; // ln(2) / 60s
        stats.count *= (-decay_rate * elapsed).exp();
        stats.last_update = now;

        // Increment count
        stats.count += 1.0;

        // Asymptotic rate: min + (max - min) / (1 + count / threshold)
        const THRESHOLD: f64 = 10.0; // 10 req/min
        self.min_rate + (self.max_rate - self.min_rate) / (1.0 + stats.count / THRESHOLD)
    }
}

impl<T: SpanProcessor> SpanProcessor for AdaptiveSpanProcessor<T> {
    fn on_start(&self, span: &mut opentelemetry_sdk::trace::Span, cx: &Context) {
        self.inner.on_start(span, cx);
    }

    fn on_end(&self, span: SpanData) {
        // Apply adaptive sampling: only forward to inner processor if we should export
        if self.should_export(&span) {
            self.inner.on_end(span);
        }
        // Otherwise drop the span silently
    }

    fn force_flush(&self) -> opentelemetry::trace::TraceResult<()> {
        self.inner.force_flush()
    }

    fn shutdown(&self) -> opentelemetry::trace::TraceResult<()> {
        self.inner.shutdown()
    }
}
