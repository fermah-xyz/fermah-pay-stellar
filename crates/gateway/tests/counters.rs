//! The counters alerts watch with `increase()` exist at zero from the start,
//! so the first event after a restart raises its alert.

#![allow(clippy::unwrap_used)]

use fermah_pay_stellar_domain::AccountAddress;
use fermah_pay_stellar_gateway::telemetry;
use metrics_util::debugging::{DebugValue, DebuggingRecorder};

/// A counter series: its name, its labels sorted, and its value.
type Series = (String, Vec<(String, String)>, u64);

/// Every counter series `register` registers.
fn registered(register: impl FnOnce()) -> Vec<Series> {
    let recorder = DebuggingRecorder::new();
    let snapshotter = recorder.snapshotter();
    metrics::with_local_recorder(&recorder, register);
    snapshotter
        .snapshot()
        .into_vec()
        .into_iter()
        .filter_map(|(key, _, _, value)| {
            let DebugValue::Counter(value) = value else { return None };
            let mut labels: Vec<_> =
                key.key().labels().map(|l| (l.key().to_owned(), l.value().to_owned())).collect();
            labels.sort();
            Some((key.key().name().to_owned(), labels, value))
        })
        .collect()
}

fn has(series: &[Series], name: &str, labels: &[(&str, &str)]) -> bool {
    series.iter().any(|(n, l, _)| {
        n == name && labels.iter().all(|(k, v)| l.iter().any(|(lk, lv)| lk == k && lv == v))
    })
}

#[test]
fn test_every_series_an_alert_selects_is_registered_at_zero() {
    let source = AccountAddress::from_public_key([3; 32]);
    let gateway = registered(telemetry::register_gateway_counters);
    let worker = registered(|| telemetry::register_worker_counters(std::slice::from_ref(&source)));
    let observer = registered(telemetry::register_observer_counters);
    for series in [&gateway, &worker, &observer] {
        assert!(series.iter().all(|(.., value)| *value == 0));
    }

    // The label values deploy/monitoring/alerts.yml selects on.
    for (name, labels) in [
        ("pay_stellar_api_refusals_total", vec![("reason", "deployment_deposit_quota_exceeded")]),
        ("pay_stellar_api_refusals_total", vec![("reason", "deployment_mandate_quota_exceeded")]),
        ("pay_stellar_api_refusals_total", vec![("reason", "buyer_quota_exceeded")]),
        ("pay_stellar_withdrawals_prepared_total", vec![("destination", "other")]),
    ] {
        assert!(has(&gateway, name, &labels), "{name} {labels:?}");
    }
    let source = source.to_string();
    for (name, labels) in [
        ("pay_stellar_charges_settled_total", vec![("result", "quarantined")]),
        ("pay_stellar_recurring_settled_total", vec![("result", "quarantined")]),
        ("pay_stellar_recurring_settled_total", vec![("result", "allowance_short")]),
        ("pay_stellar_recurring_settled_total", vec![("result", "no_mandate")]),
        ("pay_stellar_fees_charged_stroops_total", vec![("kind", "deposit")]),
        (
            "pay_stellar_submissions_closed_total",
            vec![("kind", "charge_batch"), ("state", "failed")],
        ),
        ("pay_stellar_submissions_closed_total", vec![("kind", "mandate"), ("state", "expired")]),
        ("pay_stellar_source_sequence_taken_total", vec![("source", source.as_str())]),
        ("pay_stellar_signing_failures_total", vec![("role", "operator")]),
        ("pay_stellar_signing_failures_total", vec![("role", "fee_source")]),
        ("pay_stellar_worker_step_failures_total", vec![]),
    ] {
        assert!(has(&worker, name, &labels), "{name} {labels:?}");
    }
    for kind in ["unknown_charge", "unknown_recurring_charge", "code_changed", "treasury_deficit"] {
        assert!(
            has(
                &observer,
                "pay_stellar_findings_total",
                &[("kind", kind), ("severity", "critical")]
            ),
            "{kind}"
        );
    }
    // A submission still installed is not closed.
    assert!(!has(&worker, "pay_stellar_submissions_closed_total", &[("state", "installed")]));
}
