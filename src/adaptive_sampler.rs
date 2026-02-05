//! Adaptive sampler that adjusts sampling rate based on traffic volume
//!
//! Low-traffic workers get higher sampling rates to ensure visibility,
//! while high-traffic workers get lower rates to avoid overwhelming the system.

use opentelemetry::trace::{TraceId, Link, SpanKind, SamplingResult, SamplingDecision};
use opentelemetry_sdk::trace::{Sampler, ShouldSample};
use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

/// Tracks request counts per worker for adaptive sampling
#[derive(Clone)]
pub struct AdaptiveSampler {
    state: Arc<RwLock<SamplerState>>,
    min_rate: f64,
    max_rate: f64,
}

struct SamplerState {
    worker_counts: HashMap<String, WorkerStats>,
    last_reset: Instant,
    reset_interval: Duration,
}

struct WorkerStats {
    count: u64,
    sampling_rate: f64,
}

impl AdaptiveSampler {
    pub fn new(min_rate: f64, max_rate: f64) -> Self {
        Self {
            state: Arc::new(RwLock::new(SamplerState {
                worker_counts: HashMap::new(),
                last_reset: Instant::now(),
                reset_interval: Duration::from_secs(60),
            })),
            min_rate: min_rate.clamp(0.0, 1.0),
            max_rate: max_rate.clamp(0.0, 1.0),
        }
    }

    fn get_sampling_rate(&self, worker_name: &str) -> f64 {
        let mut state = self.state.write().unwrap();

        // Reset counts periodically
        if state.last_reset.elapsed() >= state.reset_interval {
            state.worker_counts.clear();
            state.last_reset = Instant::now();
        }

        // Get or create worker stats
        let stats = state
            .worker_counts
            .entry(worker_name.to_string())
            .or_insert(WorkerStats {
                count: 0,
                sampling_rate: self.max_rate,
            });

        stats.count += 1;

        // Adaptive rate: high traffic = low rate, low traffic = high rate
        // Formula: max_rate / log2(1 + count)
        let rate = if stats.count <= 1 {
            self.max_rate
        } else {
            let divisor = (1.0 + stats.count as f64).log2();
            (self.max_rate / divisor).max(self.min_rate)
        };

        stats.sampling_rate = rate;
        rate
    }
}

impl ShouldSample for AdaptiveSampler {
    fn should_sample(
        &self,
        parent_context: Option<&opentelemetry::Context>,
        trace_id: TraceId,
        name: &str,
        _span_kind: &SpanKind,
        attributes: &[opentelemetry::KeyValue],
        _links: &[Link],
    ) -> SamplingResult {
        // Extract worker name from attributes
        let worker_name = attributes
            .iter()
            .find(|kv| kv.key.as_str() == "worker.name")
            .map(|kv| kv.value.to_string())
            .unwrap_or_else(|| "unknown".to_string());

        let sampling_rate = self.get_sampling_rate(&worker_name);

        // Hash-based sampling decision
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
        let should_sample = hash <= threshold;

        SamplingResult {
            decision: if should_sample {
                SamplingDecision::RecordAndSample
            } else {
                SamplingDecision::Drop
            },
            attributes: vec![],
            trace_state: parent_context
                .and_then(|ctx| {
                    use opentelemetry::trace::TraceContextExt;
                    Some(ctx.span().span_context().trace_state().clone())
                })
                .unwrap_or_default(),
        }
    }
}
