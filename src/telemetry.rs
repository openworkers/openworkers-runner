//! Telemetry initialization for OpenWorkers Runner
//!
//! Provides logging/tracing setup with optional OpenTelemetry export.
//!
//! ## Usage
//!
//! ```ignore
//! // Without telemetry feature: env_logger (RUST_LOG)
//! telemetry::init();
//!
//! // With telemetry feature + OTLP_ENDPOINT: OpenTelemetry
//! // OTLP_ENDPOINT=https://otlp.example.com:4317
//! // OTLP_API_KEY=xxx (optional, convenience for api-key header)
//! // OTLP_HEADERS=key1=value1,key2=value2 (optional, for custom headers)
//! // OTLP_SERVICE_NAME=openworkers-runner (optional)
//! telemetry::init();
//!
//! // With telemetry feature, no OTLP_ENDPOINT: tracing console
//! telemetry::init();
//! ```

#[cfg(feature = "telemetry")]
use tracing_subscriber::{EnvFilter, layer::SubscriberExt, util::SubscriberInitExt};

/// Initialize telemetry/logging
///
/// **Without `telemetry` feature:**
/// - Uses `env_logger` (simple, RUST_LOG env var)
///
/// **With `telemetry` feature:**
/// - If OTLP_ENDPOINT set: OpenTelemetry with OTLP export
/// - Otherwise: tracing_subscriber console output
pub fn init() {
    #[cfg(not(feature = "telemetry"))]
    {
        env_logger::init();
    }

    #[cfg(feature = "telemetry")]
    {
        if std::env::var("OTLP_ENDPOINT").is_ok() {
            if let Err(e) = init_otel() {
                eprintln!(
                    "Failed to init OpenTelemetry: {}, falling back to console",
                    e
                );
                init_console();
            }
        } else {
            init_console();
        }
    }
}

/// Console-only tracing (no OTLP)
#[cfg(feature = "telemetry")]
fn init_console() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    tracing_subscriber::registry()
        .with(filter)
        .with(tracing_subscriber::fmt::layer())
        .init();
}

/// OpenTelemetry tracing with OTLP export
#[cfg(feature = "telemetry")]
fn init_otel() -> Result<(), Box<dyn std::error::Error>> {
    use opentelemetry::KeyValue;
    use opentelemetry::trace::TracerProvider as _;
    use opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge;
    use opentelemetry_otlp::{WithExportConfig, WithTonicConfig};
    use opentelemetry_sdk::Resource;
    use opentelemetry_sdk::logs::SdkLoggerProvider;
    use opentelemetry_sdk::trace::SdkTracerProvider;
    use tonic::metadata::{MetadataMap, MetadataValue};
    use tonic::transport::ClientTlsConfig;

    let otlp_endpoint = std::env::var("OTLP_ENDPOINT")?;
    let service_name =
        std::env::var("OTLP_SERVICE_NAME").unwrap_or_else(|_| "openworkers-runner".to_string());
    let api_key = std::env::var("OTLP_API_KEY").ok();
    let headers = std::env::var("OTLP_HEADERS").ok();

    // Generate unique instance ID (first 8 chars of UUID v4)
    let instance_id = uuid::Uuid::new_v4().to_string()[..8].to_string();

    // Prepare metadata headers for authentication
    let mut metadata = MetadataMap::new();

    // Convenience: OTLP_API_KEY sets api-key header
    if let Some(key) = &api_key {
        metadata.insert("api-key", MetadataValue::try_from(key)?);
    }

    // Parse OTLP_HEADERS (format: key1=value1,key2=value2)
    if let Some(headers_str) = headers {
        for header in headers_str.split(',') {
            let parts: Vec<&str> = header.trim().splitn(2, '=').collect();
            if parts.len() == 2 {
                let key = parts[0].trim();
                let value = parts[1].trim();
                if let Ok(metadata_value) = MetadataValue::try_from(value) {
                    metadata.insert(key, metadata_value);
                }
            }
        }
    }

    // Configure TLS if endpoint is HTTPS
    let use_tls = otlp_endpoint.starts_with("https://");

    // Shared resource for traces and logs
    let resource = Resource::builder()
        .with_attribute(KeyValue::new("service.name", service_name.clone()))
        .with_attribute(KeyValue::new("service.instance.id", instance_id.clone()))
        .build();

    // Create OTLP span exporter
    let mut span_builder = opentelemetry_otlp::SpanExporter::builder()
        .with_tonic()
        .with_endpoint(&otlp_endpoint)
        .with_metadata(metadata.clone());

    if use_tls {
        span_builder = span_builder.with_tls_config(ClientTlsConfig::new().with_native_roots());
    }

    let span_exporter = span_builder.build()?;

    // Create tracer provider
    let tracer_provider = SdkTracerProvider::builder()
        .with_resource(resource.clone())
        .with_batch_exporter(span_exporter)
        .build();

    let tracer = tracer_provider.tracer("openworkers-runner");
    opentelemetry::global::set_tracer_provider(tracer_provider);

    // Create OTLP log exporter
    let mut log_builder = opentelemetry_otlp::LogExporter::builder()
        .with_tonic()
        .with_endpoint(&otlp_endpoint)
        .with_metadata(metadata);

    if use_tls {
        log_builder = log_builder.with_tls_config(ClientTlsConfig::new().with_native_roots());
    }

    let log_exporter = log_builder.build()?;

    // Create logger provider
    let logger_provider = SdkLoggerProvider::builder()
        .with_resource(resource)
        .with_batch_exporter(log_exporter)
        .build();

    // Tracing layer for spans
    let telemetry_layer = tracing_opentelemetry::layer().with_tracer(tracer);

    // Logging layer with trace correlation
    let otel_log_layer = OpenTelemetryTracingBridge::new(&logger_provider);

    // Env filter
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    // Console output
    let fmt_layer = tracing_subscriber::fmt::layer();

    // Combine all layers
    tracing_subscriber::registry()
        .with(filter)
        .with(fmt_layer)
        .with(telemetry_layer)
        .with(otel_log_layer)
        .init();

    tracing::info!(
        service = %service_name,
        instance_id = %instance_id,
        endpoint = %otlp_endpoint,
        "OpenTelemetry initialized"
    );

    Ok(())
}

/// Shutdown OpenTelemetry gracefully
#[allow(dead_code)]
pub fn shutdown() {
    // In 0.31+, tracer provider is shut down automatically on drop
    // or can be explicitly shut down by dropping the global provider
}
