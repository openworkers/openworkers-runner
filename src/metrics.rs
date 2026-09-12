//! Metrics for OpenWorkers Runner
//!
//! Provides OpenTelemetry metrics for observability:
//! - Request rate and latency
//! - Scheduled task performance
//! - Worker pool and isolate pool occupancy
//! - Queue time vs execution time

use std::time::Instant;

use openworkers_core::TerminationReason;

#[cfg(feature = "telemetry")]
use opentelemetry::metrics::{Counter, Histogram, Meter};
#[cfg(feature = "telemetry")]
use opentelemetry::{KeyValue, global};

/// How a request or a task ended. A failure name is a metric label, so the set
/// is closed and never carries the message a reason holds.
#[derive(Clone, Copy)]
pub enum Outcome {
    Success,
    Failed(&'static str),
}

impl Outcome {
    /// The outcome of a worker the runtime stopped.
    pub fn terminated(reason: &TerminationReason) -> Self {
        Self::Failed(match reason {
            TerminationReason::CpuTimeLimit => "cpu_time_limit",
            TerminationReason::WallClockTimeout => "wall_clock_timeout",
            TerminationReason::MemoryLimit => "memory_limit",
            TerminationReason::MaxIterationsReached => "max_iterations",
            TerminationReason::Exception(_) => "exception",
            TerminationReason::InitializationError(_) => "initialization_error",
            TerminationReason::Terminated => "terminated",
            TerminationReason::Aborted => "aborted",
            TerminationReason::Other(_) => "other",
        })
    }

    #[cfg(feature = "telemetry")]
    fn status(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::Failed(_) => "error",
        }
    }
}

/// Global metrics instance
#[cfg(feature = "telemetry")]
pub struct Metrics {
    // HTTP Request metrics
    pub http_requests_total: Counter<u64>,
    pub http_request_duration: Histogram<f64>,
    pub http_request_queue_time: Histogram<f64>,
    pub http_request_execution_time: Histogram<f64>,

    // Scheduled task metrics
    pub scheduled_tasks_total: Counter<u64>,
    pub scheduled_task_duration: Histogram<f64>,
    pub scheduled_task_queue_time: Histogram<f64>,
    pub scheduled_task_execution_time: Histogram<f64>,

    // Error metrics
    pub errors_total: Counter<u64>,
}

#[cfg(feature = "telemetry")]
impl Metrics {
    pub fn new(meter: Meter) -> Self {
        Self {
            // HTTP Request metrics
            http_requests_total: meter
                .u64_counter("http.requests.total")
                .with_description("Total number of HTTP requests")
                .build(),
            http_request_duration: meter
                .f64_histogram("http.request.duration")
                .with_description("HTTP request total duration in seconds")
                .with_unit("s")
                .build(),
            http_request_queue_time: meter
                .f64_histogram("http.request.queue_time")
                .with_description("Time from request received to worker spawned in seconds")
                .with_unit("s")
                .build(),
            http_request_execution_time: meter
                .f64_histogram("http.request.execution_time")
                .with_description("Time spent executing in worker in seconds")
                .with_unit("s")
                .build(),

            // Scheduled task metrics
            scheduled_tasks_total: meter
                .u64_counter("scheduled.tasks.total")
                .with_description("Total number of scheduled tasks")
                .build(),
            scheduled_task_duration: meter
                .f64_histogram("scheduled.task.duration")
                .with_description("Scheduled task total duration in seconds")
                .with_unit("s")
                .build(),
            scheduled_task_queue_time: meter
                .f64_histogram("scheduled.task.queue_time")
                .with_description("Time from task received to worker spawned in seconds")
                .with_unit("s")
                .build(),
            scheduled_task_execution_time: meter
                .f64_histogram("scheduled.task.execution_time")
                .with_description("Time spent executing task in worker in seconds")
                .with_unit("s")
                .build(),

            // Error metrics
            errors_total: meter
                .u64_counter("errors.total")
                .with_description("Total number of errors")
                .build(),
        }
    }
}

#[cfg(feature = "telemetry")]
static METRICS: once_cell::sync::OnceCell<Metrics> = once_cell::sync::OnceCell::new();

/// Initialize metrics (called from telemetry::init)
#[cfg(feature = "telemetry")]
pub fn init_metrics() {
    let meter = global::meter("openworkers-runner");
    let metrics = Metrics::new(meter.clone());
    let _ = METRICS.set(metrics);
    observe_pools(&meter);
}

/// Pool occupancy is read at collection time, so a pool never has to push.
#[cfg(feature = "telemetry")]
fn observe_pools(meter: &Meter) {
    meter
        .u64_observable_gauge("worker.pool.active_tasks")
        .with_description("Workers running or queued")
        .with_callback(|observer| {
            observer.observe(crate::worker_pool::get_active_tasks() as u64, &[])
        })
        .build();

    meter
        .u64_observable_gauge("worker.pool.available_permits")
        .with_description("Free slots in the worker semaphore")
        .with_callback(|observer| {
            let free = crate::worker_pool::WORKER_SEMAPHORE.available_permits();
            observer.observe(free as u64, &[])
        })
        .build();

    #[cfg(feature = "v8")]
    {
        meter
            .u64_observable_counter("isolate.pool.requests")
            .with_description("Executions that asked the isolate pool for an isolate")
            .with_callback(|observer| {
                let stats = openworkers_runtime_v8::get_pinned_pool_stats();
                observer.observe(stats.total_requests as u64, &[])
            })
            .build();

        meter
            .u64_observable_counter("isolate.pool.hits")
            .with_description("Executions served by an isolate already holding the worker")
            .with_callback(|observer| {
                let stats = openworkers_runtime_v8::get_pinned_pool_stats();
                observer.observe(stats.cache_hits as u64, &[])
            })
            .build();
    }
}

/// Get global metrics instance
#[cfg(feature = "telemetry")]
pub fn metrics() -> Option<&'static Metrics> {
    METRICS.get()
}

/// Timer for tracking request/task duration and phases
pub struct MetricsTimer {
    #[cfg_attr(not(feature = "telemetry"), allow(dead_code))]
    pub start: Instant,
    #[cfg(feature = "telemetry")]
    worker_spawned: Option<Instant>,
    #[cfg(feature = "telemetry")]
    labels: Vec<KeyValue>,
}

impl MetricsTimer {
    pub fn new() -> Self {
        Self {
            start: Instant::now(),
            #[cfg(feature = "telemetry")]
            worker_spawned: None,
            #[cfg(feature = "telemetry")]
            labels: Vec::new(),
        }
    }

    #[cfg(feature = "telemetry")]
    pub fn with_labels(mut self, labels: Vec<KeyValue>) -> Self {
        self.labels = labels;
        self
    }

    /// Mark when worker was spawned (to calculate queue time)
    #[cfg(feature = "telemetry")]
    pub fn mark_worker_spawned(&mut self) {
        self.worker_spawned = Some(Instant::now());
    }

    /// Record HTTP request metrics
    #[cfg(feature = "telemetry")]
    pub fn record_http_request(self, outcome: Outcome) {
        if let Some(m) = metrics() {
            let total_duration = self.start.elapsed().as_secs_f64();

            // Record total count
            let mut labels = self.labels.clone();
            labels.push(KeyValue::new("status", outcome.status()));
            m.http_requests_total.add(1, &labels);

            // Record durations
            m.http_request_duration.record(total_duration, &labels);

            if let Some(spawned) = self.worker_spawned {
                let queue_time = spawned.duration_since(self.start).as_secs_f64();
                let execution_time = spawned.elapsed().as_secs_f64();

                m.http_request_queue_time.record(queue_time, &labels);
                m.http_request_execution_time
                    .record(execution_time, &labels);
            }

            record_error(m, labels, outcome);
        }
    }

    /// Record scheduled task metrics
    #[cfg(feature = "telemetry")]
    pub fn record_scheduled_task(self, outcome: Outcome) {
        if let Some(m) = metrics() {
            let total_duration = self.start.elapsed().as_secs_f64();

            // Record total count
            let mut labels = self.labels.clone();
            labels.push(KeyValue::new("status", outcome.status()));
            m.scheduled_tasks_total.add(1, &labels);

            // Record durations
            m.scheduled_task_duration.record(total_duration, &labels);

            if let Some(spawned) = self.worker_spawned {
                let queue_time = spawned.duration_since(self.start).as_secs_f64();
                let execution_time = spawned.elapsed().as_secs_f64();

                m.scheduled_task_queue_time.record(queue_time, &labels);
                m.scheduled_task_execution_time
                    .record(execution_time, &labels);
            }

            record_error(m, labels, outcome);
        }
    }

    // No-op versions for when telemetry is disabled
    #[cfg(not(feature = "telemetry"))]
    pub fn mark_worker_spawned(&mut self) {}

    #[cfg(not(feature = "telemetry"))]
    pub fn record_http_request(self, _outcome: Outcome) {}

    #[cfg(not(feature = "telemetry"))]
    pub fn record_scheduled_task(self, _outcome: Outcome) {}
}

/// The reason rides on `errors.total` alone: on a duration histogram it would
/// multiply every bucket.
#[cfg(feature = "telemetry")]
fn record_error(m: &Metrics, mut labels: Vec<KeyValue>, outcome: Outcome) {
    let Outcome::Failed(reason) = outcome else {
        return;
    };

    labels.push(KeyValue::new("reason", reason));
    m.errors_total.add(1, &labels);
}

impl Default for MetricsTimer {
    fn default() -> Self {
        Self::new()
    }
}
