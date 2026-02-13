//! Adaptive span processor for tail-based sampling
//!
//! Implements traffic-aware sampling that filters spans at export time.
//! High-traffic workers get lower sampling rates while low-traffic workers
//! maintain high visibility.
//!
//! ## How it works
//!
//! 1. Spans are created normally with all attributes
//! 2. When span ends, processor checks `worker_id` attribute
//! 3. Calculates sampling rate based on recent traffic (sliding window)
//! 4. Drops spans probabilistically before export
//!
//! ## Sampling formula
//!
//! ```text
//! rate = min_rate + (max_rate - min_rate) / (1 + req_per_min / threshold)
//!
//! Examples with min=0.01, max=1.0, threshold=10:
//! - 0 req/min   → 100% sampling
//! - 10 req/min  → 50% sampling
//! - 100 req/min → ~10% sampling
//! - ∞           → 1% sampling (asymptote)
//! ```
//!
//! Traffic is measured with exponential decay (60s half-life) to create
//! a sliding window effect.

use opentelemetry::Context;
use opentelemetry_sdk::error::OTelSdkResult;
use opentelemetry_sdk::trace::{SpanData, SpanProcessor};
use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

/// Decay half-life for traffic measurement (seconds)
const DECAY_HALF_LIFE: f64 = 60.0;

/// Traffic threshold for 50% sampling rate (req/min)
const TRAFFIC_THRESHOLD: f64 = 10.0;

/// Span processor that applies adaptive sampling at export time
#[derive(Debug)]
pub struct AdaptiveSpanProcessor<T: SpanProcessor> {
    inner: T,
    state: Arc<RwLock<ProcessorState>>,
    min_rate: f64,
    max_rate: f64,
}

#[derive(Debug)]
struct ProcessorState {
    worker_counts: HashMap<String, WorkerStats>,
}

#[derive(Debug)]
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

impl<T: SpanProcessor> SpanProcessor for AdaptiveSpanProcessor<T> {
    fn on_start(&self, span: &mut opentelemetry_sdk::trace::Span, cx: &Context) {
        self.inner.on_start(span, cx);
    }

    fn on_end(&self, span: SpanData) {
        if self.should_export(&span) {
            self.inner.on_end(span);
        }
    }

    fn force_flush(&self) -> OTelSdkResult {
        self.inner.force_flush()
    }

    fn shutdown_with_timeout(&self, timeout: Duration) -> OTelSdkResult {
        self.inner.shutdown_with_timeout(timeout)
    }
}
