//! isolate.heap.used records the JS heap per worker, in buckets that set the
//! default heap limit, 128 MiB, apart.

use opentelemetry_sdk::metrics::data::{AggregatedMetrics, MetricData};
use opentelemetry_sdk::metrics::{InMemoryMetricExporter, PeriodicReader, SdkMeterProvider};
use openworkers_runner::metrics::{init_metrics, record_isolate_heap};

const MIB: usize = 1024 * 1024;

#[test]
fn a_worker_above_the_default_heap_limit_lands_above_its_bucket() {
    let exporter = InMemoryMetricExporter::default();
    let provider = SdkMeterProvider::builder()
        .with_reader(PeriodicReader::builder(exporter.clone()).build())
        .build();
    opentelemetry::global::set_meter_provider(provider.clone());
    init_metrics();

    record_isolate_heap("heavy", 200 * MIB);
    record_isolate_heap("light", 20 * MIB);
    provider.force_flush().unwrap();

    let exported = exporter.get_finished_metrics().unwrap();
    let histogram = exported
        .iter()
        .flat_map(|resource| resource.scope_metrics())
        .flat_map(|scope| scope.metrics())
        .find(|metric| metric.name() == "isolate.heap.used")
        .map(|metric| match metric.data() {
            AggregatedMetrics::U64(MetricData::Histogram(histogram)) => histogram.clone(),
            other => panic!("isolate.heap.used is not a u64 histogram: {other:?}"),
        })
        .expect("isolate.heap.used is exported");

    // The count of each worker above the 128 MiB boundary
    let above_limit = |worker: &str| {
        let point = histogram
            .data_points()
            .find(|point| {
                point
                    .attributes()
                    .any(|kv| kv.key.as_str() == "worker_id" && kv.value.as_str() == worker)
            })
            .unwrap_or_else(|| panic!("no data point for {worker}"));
        let bounds: Vec<f64> = point.bounds().collect();
        let limit = bounds
            .iter()
            .position(|&bound| bound == (128 * MIB) as f64)
            .expect("128 MiB is a bucket boundary");

        point.bucket_counts().skip(limit + 1).sum::<u64>()
    };

    assert_eq!(above_limit("heavy"), 1);
    assert_eq!(above_limit("light"), 0);
}
