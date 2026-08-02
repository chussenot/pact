#!/usr/bin/env python3
"""Generate ops/signoz/pact-fleet.dashboard.json.

Ten SigNoz widgets, each answering one question the pact-aw7 retro answered by
hand. The generator exists rather than hand-written JSON because a SigNoz widget
is ~60 lines of boilerplate around three interesting fields (metric, group-by,
aggregation) — so the interesting fields live in PANELS below and everything
else is filled in by `widget()`.

Deterministic on purpose: widget ids are UUIDv5 of the title, so regenerating
after an edit produces a reviewable diff instead of a new id for every panel.

    python3 ops/signoz/build_dashboard.py > ops/signoz/pact-fleet.dashboard.json
"""

import json
import uuid

NS = uuid.UUID("6ba7b810-9dad-11d1-80b4-00c04fd430c8")


def wid(title):
    return str(uuid.uuid5(NS, "pact-fleet/" + title))


def attr(key, dtype="string", atype="tag", is_column=False):
    return {
        "key": key,
        "dataType": dtype,
        "type": atype,
        "isColumn": is_column,
        "isJSON": False,
    }


def metric_query(
    name,
    metric,
    metric_type,
    time_agg,
    space_agg,
    group_by=(),
    legend="",
    filters=(),
    disabled=False,
):
    return {
        "dataSource": "metrics",
        "queryName": name,
        "aggregateOperator": time_agg,
        "aggregateAttribute": {
            "key": metric,
            "dataType": "float64",
            "type": metric_type,
            "isColumn": True,
            "isJSON": False,
        },
        "timeAggregation": time_agg,
        "spaceAggregation": space_agg,
        "functions": [],
        "filters": {"items": list(filters), "op": "AND"},
        "expression": name,
        "disabled": disabled,
        "stepInterval": 60,
        "having": [],
        "limit": None,
        "orderBy": [],
        "groupBy": [attr(g) for g in group_by],
        "legend": legend,
        "reduceTo": "sum",
    }


def trace_query(name, group_by=(), legend="", filters=(), agg="count"):
    return {
        "dataSource": "traces",
        "queryName": name,
        "aggregateOperator": agg,
        "aggregateAttribute": {
            "key": "",
            "dataType": "",
            "type": "",
            "isColumn": False,
            "isJSON": False,
        },
        "timeAggregation": agg,
        "spaceAggregation": "sum",
        "functions": [],
        "filters": {"items": list(filters), "op": "AND"},
        "expression": name,
        "disabled": False,
        "stepInterval": 60,
        "having": [],
        "limit": 50,
        "orderBy": [{"columnName": "#SIGNOZ_VALUE", "order": "desc"}],
        "groupBy": [attr(*g) if isinstance(g, tuple) else attr(g) for g in group_by],
        "legend": legend,
        "reduceTo": "sum",
    }


def tfilter(key, op, value, dtype="string", atype="tag", is_column=False):
    return {
        "id": str(uuid.uuid5(NS, f"{key}{op}{value}"))[:8],
        "key": attr(key, dtype, atype, is_column),
        "op": op,
        "value": value,
    }


def formula(name, expression, legend=""):
    return {
        "queryName": name,
        "expression": expression,
        "legend": legend,
        "disabled": False,
    }


def widget(title, description, queries, panel="graph", unit="none", formulas=(), thresholds=()):
    i = wid(title)
    return {
        "id": i,
        "title": title,
        "description": description,
        "panelTypes": panel,
        "isStacked": False,
        "opacity": "1",
        "nullZeroValues": "zero",
        "timePreferance": "GLOBAL_TIME",
        "softMax": None,
        "softMin": None,
        "fillSpans": False,
        "yAxisUnit": unit,
        "thresholds": list(thresholds),
        "selectedLogFields": [],
        "selectedTracesFields": [],
        "query": {
            "queryType": "builder",
            "promql": [{"name": "A", "query": "", "legend": "", "disabled": False}],
            "clickhouse_sql": [
                {"name": "A", "legend": "", "disabled": False, "query": ""}
            ],
            "id": i,
            "builder": {"queryData": queries, "queryFormulas": list(formulas)},
        },
    }


# --------------------------------------------------------------------------
# The panels. Order here is the reading order on the dashboard.
# --------------------------------------------------------------------------

PANELS = [
    widget(
        "Lease traffic by outcome",
        "Every lease state transition pact makes. `conflicted` is contention: an "
        "agent asked for a path someone else held. `expired` + `reclaimed` is a "
        "fleet where someone walked away without releasing.",
        [
            metric_query(
                "A",
                "pact.lease.transitions",
                "Sum",
                "increase",
                "sum",
                group_by=["pact.lease.outcome"],
                legend="{{pact.lease.outcome}}",
            )
        ],
    ),
    widget(
        "Leaked leases (acquires minus releases)",
        "A - B. Positive and rising means the fleet is taking leases it never "
        "gives back; the retro found this by diffing `pact log` by hand. Steady "
        "zero is a fleet that cleans up after itself.",
        [
            metric_query(
                "A",
                "pact.lease.transitions",
                "Sum",
                "increase",
                "sum",
                filters=[tfilter("pact.lease.outcome", "=", "acquired")],
                legend="acquired",
                disabled=True,
            ),
            metric_query(
                "B",
                "pact.lease.transitions",
                "Sum",
                "increase",
                "sum",
                filters=[
                    tfilter("pact.lease.outcome", "in", ["released", "force_released"])
                ],
                legend="released",
                disabled=True,
            ),
        ],
        formulas=[formula("F1", "A - B", "leaked this interval")],
    ),
    widget(
        "Who is blocked, and on which path",
        "Traces only: `pact.lease.acquire` spans that failed with status `held`. "
        "Grouped by the blocked agent (resource attribute) and the path. NOTE: "
        "pact does not emit the HOLDER on the losing span, so this names the "
        "victim and the file, not the winner. See ops/signoz/README.md.",
        [
            trace_query(
                "A",
                group_by=[
                    ("pact.agent", "string", "resource"),
                    ("pact.path", "string", "tag"),
                ],
                legend="{{pact.agent}} blocked on {{pact.path}}",
                filters=[
                    tfilter("name", "=", "pact.lease.acquire", is_column=True),
                    tfilter("hasError", "=", "true", dtype="bool", is_column=True),
                ],
            )
        ],
        panel="table",
    ),
    widget(
        "Time an agent spent blocked before winning a path",
        "p95 of `pact.lease.wait.duration`: wall clock between an agent's first "
        "refused acquire on a path and the acquire that finally succeeded. This "
        "is the number that turns 'contention' into 'cost'.",
        [
            metric_query(
                "A",
                "pact.lease.wait.duration",
                "Histogram",
                "p95",
                "p95",
                legend="p95 blocked",
            )
        ],
        unit="ms",
    ),
    widget(
        "Lease hold time, and whether it overran its TTL",
        "p95 of `pact.lease.hold.duration` split by `pact.lease.overrun`. "
        "`overrun=true` is a promise broken: the holder said N seconds and took "
        "longer without renewing.",
        [
            metric_query(
                "A",
                "pact.lease.hold.duration",
                "Histogram",
                "p95",
                "p95",
                group_by=["pact.lease.overrun", "pact.lease.outcome"],
                legend="overrun={{pact.lease.overrun}} {{pact.lease.outcome}}",
            )
        ],
        unit="ms",
    ),
    widget(
        "Messages sent vs messages read",
        "`pact.msg.sent` against `pact.msg.read`. A persistent gap is the retro's "
        "headline finding restated as a metric: 51 of 59 messages went to agents "
        "that had already exited.",
        [
            metric_query(
                "A", "pact.msg.sent", "Sum", "increase", "sum", legend="sent"
            ),
            metric_query(
                "B", "pact.msg.read", "Sum", "increase", "sum", legend="read"
            ),
        ],
    ),
    widget(
        "Unread messages by age",
        "`pact.msg.unread` gauge, one series per age bucket. Anything in "
        "`15m_1h` or `gt_1h` is a message whose recipient is very likely gone.",
        [
            metric_query(
                "A",
                "pact.msg.unread",
                "Gauge",
                "max",
                "sum",
                group_by=["pact.msg.age_bucket"],
                legend="{{pact.msg.age_bucket}}",
            )
        ],
    ),
    widget(
        "How long a sender waited to be read (p50 / p95)",
        "`pact.msg.read_latency`: age of a message at the moment of its first "
        "read. The tail is the interesting half.",
        [
            metric_query(
                "A", "pact.msg.read_latency", "Histogram", "p50", "p50", legend="p50"
            ),
            metric_query(
                "B", "pact.msg.read_latency", "Histogram", "p95", "p95", legend="p95"
            ),
        ],
        unit="ms",
    ),
    widget(
        "Beads subprocess latency by subcommand (p95)",
        "`pact.beads.duration`: every `bd`/`br` spawn pact makes. pact shells out "
        "for all messaging, so this is the floor under every `pact msg` command.",
        [
            metric_query(
                "A",
                "pact.beads.duration",
                "Histogram",
                "p95",
                "p95",
                group_by=["pact.beads.subcommand", "pact.outcome"],
                legend="{{pact.beads.subcommand}} ({{pact.outcome}})",
            )
        ],
        unit="ms",
    ),
    widget(
        "Doctor check health over time",
        "`pact.doctor.check.status` per check: 0 = fail, 1 = warn, 2 = pass. "
        "pact only exports this when a verdict actually moves, so a flat line is "
        "a stable repo, not a stalled exporter.",
        [
            metric_query(
                "A",
                "pact.doctor.check.status",
                "Gauge",
                "min",
                "min",
                group_by=["pact.doctor.check"],
                legend="{{pact.doctor.check}}",
            )
        ],
        thresholds=[
            {
                "index": "fail",
                "keyIndex": 0,
                "moveThreshold": 0,
                "selectedGraph": "graph",
                "thresholdColor": "Red",
                "thresholdFormat": "Text",
                "thresholdLabel": "fail",
                "thresholdOperator": "<",
                "thresholdValue": 1,
                "thresholdUnit": "none",
            }
        ],
    ),
    widget(
        "pact command latency by subcommand (p95)",
        "`pact.command.duration`, the end-to-end cost of a pact invocation. "
        "Read it against the Beads panel: when `msg send` moves, this shows "
        "whether pact or `bd` moved.",
        [
            metric_query(
                "A",
                "pact.command.duration",
                "Histogram",
                "p95",
                "p95",
                group_by=["pact.subcommand"],
                legend="{{pact.subcommand}}",
            )
        ],
        unit="ms",
    ),
    widget(
        "Claude Code cost, by session (NOT joinable to pact — see description)",
        "`claude_code.cost.usage` grouped by `session.id`. This panel sits next "
        "to the pact panels so the two can be read on a shared time axis, which "
        "is TODAY THE ONLY CORRELATION AVAILABLE: pact emits no attribute that "
        "matches any Claude Code attribute. The fix is one line in pact — read "
        "$CLAUDE_CODE_SESSION_ID into a resource attribute. See "
        "ops/signoz/README.md, section 'The join'.",
        [
            metric_query(
                "A",
                "claude_code.cost.usage",
                "Sum",
                "increase",
                "sum",
                group_by=["session.id"],
                legend="{{session.id}}",
            )
        ],
        unit="none",
    ),
    widget(
        "Claude Code tokens, by session and type",
        "`claude_code.token.usage` grouped by `session.id` and `type` "
        "(input/output/cacheRead/cacheCreation). Pair with the lease-contention "
        "panel to eyeball 'did an agent burn tokens while blocked' — eyeball, "
        "because there is no join key to do it properly.",
        [
            metric_query(
                "A",
                "claude_code.token.usage",
                "Sum",
                "increase",
                "sum",
                group_by=["session.id", "type"],
                legend="{{session.id}} {{type}}",
            )
        ],
    ),
]


def layout(panels):
    out = []
    for n, p in enumerate(panels):
        out.append(
            {"i": p["id"], "x": (n % 2) * 6, "y": (n // 2) * 3, "w": 6, "h": 3}
        )
    return out


dashboard = {
    "title": "pact fleet coordination",
    "description": (
        "Every question the pact-aw7 retro answered by reading 219 log events, "
        "as a query. Generated by ops/signoz/build_dashboard.py — edit that, "
        "not this file."
    ),
    "tags": ["pact", "otel", "pact-aw7"],
    "layout": layout(PANELS),
    "widgets": PANELS,
    "variables": {},
    "version": "v4",
    "uploadedGrafana": False,
}

def check():
    """The smallest thing that fails if the generator breaks.

    SigNoz drops a widget whose id is missing from `layout` — silently, so the
    dashboard imports and the panel simply is not there. That is the failure
    worth a check; the rest is JSON.
    """
    ids = [w["id"] for w in PANELS]
    assert len(ids) == len(set(ids)), "duplicate widget id — two panels share a title"
    assert {l["i"] for l in dashboard["layout"]} == set(ids), "layout/widget id mismatch"
    for w in PANELS:
        qd = w["query"]["builder"]["queryData"]
        names = [q["queryName"] for q in qd]
        assert len(names) == len(set(names)), f"{w['title']}: duplicate queryName"
        for f in w["query"]["builder"]["queryFormulas"]:
            # a formula referencing a query that is not in this widget renders
            # as an empty panel with no error
            refs = {c for c in f["expression"] if c.isupper()}
            assert refs <= set(names), f"{w['title']}: formula {f['expression']} refs {refs - set(names)}"
        assert w["panelTypes"] in {"graph", "table", "value", "list"}, w["title"]
    json.dumps(dashboard)  # must be serialisable


if __name__ == "__main__":
    check()
    print(json.dumps(dashboard, indent=2))
