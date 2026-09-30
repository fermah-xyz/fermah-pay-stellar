//! Logs, traces and metrics for every process.
//!
//! - Logs are JSON lines on stdout, filtered by `RUST_LOG` (default `info`).
//! - Traces are exported over OTLP when `OTEL_EXPORTER_OTLP_ENDPOINT` is
//!   set, the standard OpenTelemetry variable: spans follow a charge from the
//!   API request to the transaction that settles it.
//! - Metrics are served for Prometheus at `/metrics` on the process's
//!   metrics address, when one is configured; `docs/self-hosting/monitoring.md`
//!   lists them, and `deploy/monitoring/alerts.yml` holds the alert rules
//!   built on them.

use std::net::SocketAddr;

use anyhow::Context;
use opentelemetry::trace::TracerProvider as _;
use opentelemetry_otlp::WithExportConfig as _;
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::trace::SdkTracerProvider;
use tracing_subscriber::layer::SubscriberExt as _;
use tracing_subscriber::util::SubscriberInitExt as _;
use tracing_subscriber::{EnvFilter, Layer as _};

const OTLP_ENDPOINT: &str = "OTEL_EXPORTER_OTLP_ENDPOINT";

/// Flushes buffered spans when dropped; keep it alive for the process.
pub struct Telemetry {
    tracer_provider: Option<SdkTracerProvider>,
}

impl Drop for Telemetry {
    fn drop(&mut self) {
        if let Some(provider) = self.tracer_provider.take()
            && let Err(error) = provider.shutdown()
        {
            eprintln!("flushing traces: {error}");
        }
    }
}

/// Installs logging, and tracing and metrics where configured, for the
/// process named `service`. Must be called inside the Tokio runtime.
pub fn init(service: &'static str, metrics_addr: Option<SocketAddr>) -> anyhow::Result<Telemetry> {
    let filter = || EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into());
    let tracer_provider = match std::env::var(OTLP_ENDPOINT) {
        Ok(endpoint) if !endpoint.is_empty() => {
            let exporter = opentelemetry_otlp::SpanExporter::builder()
                .with_tonic()
                .with_endpoint(endpoint)
                .build()
                .context("building the OTLP trace exporter")?;
            Some(
                SdkTracerProvider::builder()
                    .with_batch_exporter(exporter)
                    .with_resource(Resource::builder().with_service_name(service).build())
                    .build(),
            )
        }
        _ => None,
    };
    let traces = tracer_provider.as_ref().map(|provider| {
        tracing_opentelemetry::layer().with_tracer(provider.tracer(service)).with_filter(filter())
    });
    tracing_subscriber::registry()
        .with(tracing_subscriber::fmt::layer().json().with_filter(filter()))
        .with(traces)
        .try_init()
        .context("installing the tracing subscriber")?;
    if let Some(addr) = metrics_addr {
        metrics_exporter_prometheus::PrometheusBuilder::new()
            .with_http_listener(addr)
            .install()
            .with_context(|| format!("serving metrics on {addr}"))?;
        tracing::info!(metrics_addr = %addr, "metrics serving");
    }
    Ok(Telemetry { tracer_provider })
}
