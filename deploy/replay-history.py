#!/usr/bin/env python3
"""Prepare a recorder export for `energie replay`.

The export (`psql \\copy` of states/states_meta/state_attributes, see docs/REPLAY.md) only has
the rows written inside its window, so entities that did not change early in the window have
no state until much later (the helpers until HA's next restart). This script asks HA's history
API (read-only GET) for every exported entity's state at the window start, writes those as rows
half a second before it, and prepends them to the export:

    replay-history.py --sql export.sql --start 2026-11-03T00:00:00+01:00 \\
        --export energie-history.csv.gz --out history-full.csv.gz

HA_URL and HA_TOKEN come from the environment or from ~/.config/ha/env (KEY=value lines).
"""
import argparse
import csv
import datetime
import gzip
import io
import json
import os
import re
import shutil
import urllib.parse
import urllib.request

# The entities whose attributes the export carries (shared_attrs).
ATTR = {
    "sensor.jeroen_average_electricity_price",
    "sensor.injectie_average_electricity_price",
    "number.smartevse_1234_charge_current_override",
    "number.ev_charging_cable_charge_current",
    "sensor.p1_meter_kwartier_import",
}


def env():
    e = {k: os.environ[k] for k in ("HA_URL", "HA_TOKEN") if k in os.environ}
    path = os.path.expanduser("~/.config/ha/env")
    if len(e) < 2 and os.path.exists(path):
        for line in open(path):
            if "=" in line:
                k, v = line.strip().split("=", 1)
                e.setdefault(k, v.strip().strip('"'))
    return e


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--sql", required=True, help="the export query (its WHERE lists the entities)")
    ap.add_argument("--start", required=True, help="window start, ISO 8601 with offset")
    ap.add_argument("--export", required=True, help="the exported CSV (.gz)")
    ap.add_argument("--out", required=True, help="the merged CSV (.gz)")
    a = ap.parse_args()

    ids = re.findall(r"'([a-z_]+\.[a-z0-9_]+)'", open(a.sql).read().split("WHERE", 1)[1])
    start = datetime.datetime.fromisoformat(a.start)
    e = env()
    q = urllib.parse.urlencode(
        {"end_time": (start + datetime.timedelta(seconds=1)).isoformat(), "filter_entity_id": ",".join(ids)}
    )
    url = e["HA_URL"] + "/api/history/period/" + urllib.parse.quote(start.isoformat()) + "?" + q
    req = urllib.request.Request(url, headers={"Authorization": "Bearer " + e["HA_TOKEN"]})
    series = json.load(urllib.request.urlopen(req, timeout=300))

    buf = io.StringIO()
    w = csv.writer(buf)
    for s in (s[0] for s in series if s):
        attrs = json.dumps(s.get("attributes", {})) if s["entity_id"] in ATTR else ""
        w.writerow([s["entity_id"], s["state"], start.timestamp() - 0.5, attrs])
    with gzip.open(a.out, "wb", compresslevel=1) as out:
        out.write(buf.getvalue().encode())
        with gzip.open(a.export, "rb") as f:
            shutil.copyfileobj(f, out)
    print(f"{len(series)} initial states of {len(ids)} entities, written to {a.out}")


if __name__ == "__main__":
    main()
