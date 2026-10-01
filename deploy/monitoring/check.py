#!/usr/bin/env python3
"""Checks the alert rules and the Grafana dashboard against the code.

- Every `pay_stellar_*` metric the rules or the dashboard name is one the
  gateway, worker or observer emits (a renamed metric would otherwise leave
  an alert that can never fire).
- The dashboard is well formed: unique panel ids, every target has a query.
- With PROMETHEUS_URL, every dashboard query parses on that Prometheus.
- With GRAFANA_URL, Grafana has loaded the provisioned dashboard.
"""

import json
import os
import pathlib
import re
import sys
import urllib.parse
import urllib.request

ROOT = pathlib.Path(__file__).resolve().parents[2]
MONITORING = ROOT / "deploy" / "monitoring"
DASHBOARD = MONITORING / "grafana" / "dashboards" / "pay-stellar.json"
METRIC = re.compile(r"\bpay_stellar_[a-z_]+\b")


def emitted() -> set:
    names = set()
    for source in (ROOT / "crates" / "gateway" / "src").rglob("*.rs"):
        names.update(re.findall(r'"(pay_stellar_[a-z_]+)"', source.read_text()))
    return names


def queries(dashboard: dict) -> list:
    found = []
    for panel in dashboard["panels"]:
        for target in panel.get("targets", []):
            found.append((panel["title"], target.get("expr", "")))
    return found


def get(url: str) -> dict:
    with urllib.request.urlopen(url, timeout=10) as response:
        return json.load(response)


def main() -> int:
    errors = []
    known = emitted()
    rules = (MONITORING / "alerts.yml").read_text()
    dashboard = json.loads(DASHBOARD.read_text())
    used = set(METRIC.findall(rules)) | {
        name for _, expr in queries(dashboard) for name in METRIC.findall(expr)
    }
    errors += [f"metric {name} is not emitted by the code" for name in sorted(used - known)]
    ids = [panel["id"] for panel in dashboard["panels"]]
    if len(ids) != len(set(ids)):
        errors.append("panel ids are not unique")
    errors += [f"panel {title!r} has a target without a query" for title, expr in queries(dashboard) if not expr]

    prometheus = os.environ.get("PROMETHEUS_URL")
    if prometheus:
        for title, expr in queries(dashboard):
            url = f"{prometheus}/api/v1/query?" + urllib.parse.urlencode({"query": expr})
            try:
                if get(url).get("status") != "success":
                    errors.append(f"panel {title!r}: query refused")
            except urllib.error.HTTPError as error:
                errors.append(f"panel {title!r}: {error.read().decode()}")
    grafana = os.environ.get("GRAFANA_URL")
    if grafana:
        loaded = get(f"{grafana}/api/dashboards/uid/{dashboard['uid']}")
        if len(loaded["dashboard"]["panels"]) != len(dashboard["panels"]):
            errors.append("Grafana did not load every panel of the dashboard")

    for error in errors:
        print(error, file=sys.stderr)
    print(f"{len(used)} metrics and {len(queries(dashboard))} queries checked; {len(errors)} problems")
    return 1 if errors else 0


if __name__ == "__main__":
    sys.exit(main())
