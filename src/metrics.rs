//! Metrics for OpenWorkers Runner
//!
//! Provides OpenTelemetry metrics for observability:
//! - Request rate and latency
//! - Scheduled task performance
//! - Worker pool utilization
//! - Queue time vs execution time

use std::time::Instant;

#[cfg(feature = "telemetry")]
use opentelemetry::metrics::{Counter, Histogram, Meter};
#[cfg(feature = "telemetry")]
use opentelemetry::{KeyValue, global};

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
    let metrics = Metrics::new(meter);
    let _ = METRICS.set(metrics);
}

/// Get global metrics instance
#[cfg(feature = "telemetry")]
pub fn metrics() -> Option<&'static Metrics> {
    METRICS.get()
}

/// Timer for tracking request/task duration and phases
pub struct MetricsTimer {
    start: Instant,
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
    pub fn record_http_request(self, success: bool) {
        if let Some(m) = metrics() {
            let total_duration = self.start.elapsed().as_secs_f64();

            // Record total count
            let mut labels = self.labels.clone();
            labels.push(KeyValue::new(
                "status",
                if success { "success" } else { "error" },
            ));
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

            if !success {
                m.errors_total.add(1, &labels);
            }
        }
    }

    /// Record scheduled task metrics
    #[cfg(feature = "telemetry")]
    pub fn record_scheduled_task(self, success: bool) {
        if let Some(m) = metrics() {
            let total_duration = self.start.elapsed().as_secs_f64();

            // Record total count
            let mut labels = self.labels.clone();
            labels.push(KeyValue::new(
                "status",
                if success { "success" } else { "error" },
            ));
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

            if !success {
                m.errors_total.add(1, &labels);
            }
        }
    }

    // No-op versions for when telemetry is disabled
    #[cfg(not(feature = "telemetry"))]
    pub fn mark_worker_spawned(&mut self) {}

    #[cfg(not(feature = "telemetry"))]
    pub fn record_http_request(self, _success: bool) {}

    #[cfg(not(feature = "telemetry"))]
    pub fn record_scheduled_task(self, _success: bool) {}
}

impl Default for MetricsTimer {
    fn default() -> Self {
        Self::new()
    }
}
