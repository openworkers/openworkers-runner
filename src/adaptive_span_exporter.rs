//! Adaptive span exporter for tail-based sampling
//!
//! Wraps a SpanExporter and filters spans at export time based on worker traffic.
//! This allows combining adaptive sampling with with_batch_exporter() which
//! properly propagates resource attributes.

use opentelemetry_sdk::error::OTelSdkResult;
use opentelemetry_sdk::trace::{SpanData, SpanExporter};
use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::Instant;

/// Decay half-life for traffic measurement (seconds)
const DECAY_HALF_LIFE: f64 = 60.0;

/// Traffic threshold for 50% sampling rate (req/min)
const TRAFFIC_THRESHOLD: f64 = 10.0;

/// Span exporter that applies adaptive sampling at export time
#[derive(Debug)]
pub struct AdaptiveSpanExporter<T: SpanExporter> {
    inner: T,
    state: Arc<RwLock<ExporterState>>,
    min_rate: f64,
    max_rate: f64,
}

#[derive(Debug)]
struct ExporterState {
    worker_counts: HashMap<String, WorkerStats>,
}

#[derive(Debug)]
struct WorkerStats {
    count: f64,
    last_update: Instant,
}

impl<T: SpanExporter> AdaptiveSpanExporter<T> {
    pub fn new(inner: T, min_rate: f64, max_rate: f64) -> Self {
        Self {
            inner,
            state: Arc::new(RwLock::new(ExporterState {
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
            .map(|kv| kv.value.to_string())
            .filter(|id: &String| !id.is_empty() && id != "Empty");

        // Always export spans without worker_id (system spans, etc.)
        let worker_id = match worker_id {
            Some(id) => id,
            None => return true,
        };

        let sampling_rate = self.get_sampling_rate(&worker_id);

        // Deterministic hash-based decision using trace_id
        let trace_id_bytes = span_data.span_context.trace_id().to_bytes();
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

        let stats = state
            .worker_counts
            .entry(worker_id.to_string())
            .or_insert(WorkerStats {
                count: 0.0,
                last_update: now,
            });

        // Apply exponential decay for sliding window effect
        let elapsed = now.duration_since(stats.last_update).as_secs_f64();
        let decay_rate = 0.693_147_2 / DECAY_HALF_LIFE; // ln(2) / half_life
        stats.count *= (-decay_rate * elapsed).exp();
        stats.last_update = now;
        stats.count += 1.0;

        // Asymptotic sampling rate
        self.min_rate + (self.max_rate - self.min_rate) / (1.0 + stats.count / TRAFFIC_THRESHOLD)
    }
}

impl<T: SpanExporter> SpanExporter for AdaptiveSpanExporter<T> {
    async fn export(&self, batch: Vec<SpanData>) -> OTelSdkResult {
        // Filter spans based on sampling rate
        let filtered: Vec<SpanData> = batch
            .into_iter()
            .filter(|span| self.should_export(span))
            .collect();

        if filtered.is_empty() {
            return Ok(());
        }

        self.inner.export(filtered).await
    }

    fn shutdown(&mut self) -> OTelSdkResult {
        self.inner.shutdown()
    }

    fn set_resource(&mut self, resource: &opentelemetry_sdk::Resource) {
        self.inner.set_resource(resource);
    }
}
