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
use fermah_pay_stellar_chain::prepaid::{Outcome, RecurringOutcome};
use fermah_pay_stellar_domain::AccountAddress;
use opentelemetry::trace::TracerProvider as _;
use opentelemetry_otlp::WithExportConfig as _;
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::trace::SdkTracerProvider;
use tracing_subscriber::layer::SubscriberExt as _;
use tracing_subscriber::util::SubscriberInitExt as _;
use tracing_subscriber::{EnvFilter, Layer as _};

use crate::ledger::WITHDRAWAL_DESTINATIONS;
use crate::observer::matching::{FindingKind, Severity};
use crate::refusal::Refusal;
use crate::submission::{Kind, SigningRole, State};

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

/// Registers at zero, for every label value the code can give them, the
/// counters an alert watches with `increase()`. A counter otherwise appears
/// with its first increment, already at 1, and `increase()` finds no rise
/// in a series' first sample: the first event of each kind after a restart
/// would raise no alert.
pub fn register_gateway_counters() {
    for refusal in Refusal::ALL {
        metrics::counter!("pay_stellar_api_refusals_total", "reason" => refusal.reason())
            .increment(0);
    }
    for destination in WITHDRAWAL_DESTINATIONS {
        metrics::counter!("pay_stellar_withdrawals_prepared_total", "destination" => destination)
            .increment(0);
    }
}

/// [`register_gateway_counters`] for the settlement worker, which sends from
/// `sources`.
pub fn register_worker_counters(sources: &[AccountAddress]) {
    metrics::counter!("pay_stellar_worker_step_failures_total").increment(0);
    let charged = (0..).map_while(Outcome::from_code).map(Outcome::token);
    for result in charged.chain(["quarantined", "requeued"]) {
        metrics::counter!("pay_stellar_charges_settled_total", "result" => result).increment(0);
    }
    let recurring = (0..).map_while(RecurringOutcome::from_code).map(RecurringOutcome::token);
    for result in recurring.chain(["quarantined", "requeued"]) {
        metrics::counter!("pay_stellar_recurring_settled_total", "result" => result).increment(0);
    }
    for kind in Kind::ALL {
        metrics::counter!("pay_stellar_fees_charged_stroops_total", "kind" => kind.as_str())
            .increment(0);
        for state in State::ALL.iter().filter(|state| state.is_final()) {
            metrics::counter!(
                "pay_stellar_submissions_closed_total",
                "kind" => kind.as_str(),
                "state" => state.as_str()
            )
            .increment(0);
        }
    }
    for role in SigningRole::ALL {
        metrics::counter!("pay_stellar_signing_failures_total", "role" => role.as_str())
            .increment(0);
    }
    for source in sources {
        metrics::counter!("pay_stellar_source_sequence_taken_total", "source" => source.to_string())
            .increment(0);
    }
}

/// [`register_gateway_counters`] for the chain observer.
pub fn register_observer_counters() {
    for kind in FindingKind::ALL {
        for severity in Severity::ALL {
            metrics::counter!(
                "pay_stellar_findings_total",
                "kind" => kind.as_str(),
                "severity" => severity.as_str()
            )
            .increment(0);
        }
    }
}
