#!/usr/bin/env python3
"""Reconcile controller activity stored in the vZDV database against VATSIM.

For each controller, ATC sessions are fetched from the VATSIM API and the
minutes the site's activity pipeline *should* have recorded are computed for
each month, using the same rules as the Rust code:

  - only sessions on facility-airspace callsigns count
    ([stats] position_prefixes/suffixes from the vzdv config)
  - sessions reporting >= 1440 minutes on callsign are dropped (VATSIM API bug)
  - in-progress time for currently-online controllers is added to this month

The computed seconds per month are compared against the `activity` table's
stored minutes minus any existing manual adjustments. Where they disagree,
INSERT statements for `controller_activity_manual_adjustment` covering the
difference are pretty-printed to stdout. Nothing is written to the database.

Usage:
    python main.py <YYYY-MM-DD> <db-path> [cid] [--config vzdv.toml]
                                                [--delay 8] [--tolerance 60]
"""

import argparse
import os.path
import re
import sqlite3
import sys
import time
from datetime import datetime, timezone

import requests as r

SESSIONS_URL = "https://api.vatsim.net/api/ratings/{cid}/atcsessions/"
DATA_URL = "https://data.vatsim.net/v3/vatsim-data.json"
# Sessions reporting this many minutes are a VATSIM API bug (the site's
# pipeline drops them; see vzdv-tasks/src/activity.rs).
BAD_SESSION_MINUTES = 1440.0


def load_stats_filters(config_path):
    """Pull position_prefixes/position_suffixes out of the [stats] config."""
    with open(config_path) as f:
        text = f.read()
    filters = []
    for key in ("position_prefixes", "position_suffixes"):
        m = re.search(key + r"\s*=\s*\[(.*?)\]", text, re.S)
        if not m:
            raise RuntimeError(f"could not find stats.{key} in {config_path}")
        filters.append(re.findall(r'"([^"]+)"', m.group(1)))
    return filters


def in_facility(callsign, prefixes, suffixes):
    """Mirror of vzdv::position_in_facility_airspace."""
    return any(callsign.startswith(p) for p in prefixes) and any(
        callsign.endswith(s) for s in suffixes
    )


def fetch_sessions(cid, start_date):
    """All ATC sessions for the controller since start_date, following pagination."""
    url = f"{SESSIONS_URL.format(cid=cid)}?start={start_date}"
    sessions = []
    while url:
        resp = r.get(url, timeout=30)
        if resp.status_code == 429:
            print(f"rate limited on {cid}; waiting 30s", file=sys.stderr)
            time.sleep(30)
            resp = r.get(url, timeout=30)
        if resp.status_code != 200:
            raise RuntimeError(f"HTTP {resp.status_code}: {resp.text[:200]}")
        data = resp.json()
        sessions.extend(data["results"])
        url = data.get("next")
    return sessions


def fetch_online_seconds(target_cids, prefixes, suffixes):
    """Map of cid -> seconds currently online in facility airspace.

    Mirrors the online-time addition in vzdv-tasks update_single_activity so
    the current month compares fairly.
    """
    try:
        data = r.get(DATA_URL, timeout=30).json()
    except Exception as e:
        print(
            f"warning: could not fetch VATSIM live data ({e}); "
            "current-month online time will be missing",
            file=sys.stderr,
        )
        return {}
    now = datetime.now(timezone.utc)
    online = {}
    for c in data.get("controllers", []):
        cid = c.get("cid")
        if cid not in target_cids:
            continue
        if not in_facility(c.get("callsign") or "", prefixes, suffixes):
            continue
        logon = c.get("logon_time")
        if not logon:
            continue
        # Python 3.10's fromisoformat rejects the >6-digit fractional
        # seconds VATSIM sometimes sends; drop the fraction entirely
        logon = datetime.fromisoformat(
            re.sub(r"\.\d+", "", logon).replace("Z", "+00:00")
        )
        online[cid] = online.get(cid, 0.0) + (now - logon).total_seconds()
    return online


def reconcile(sessions, online_secs, stored, adjustments, current_month, tolerance):
    """Compute needed adjustment seconds per month.

    Returns (rows, skipped) where rows = [(month, delta_seconds, note)] and
    skipped = bugged sessions the pipeline would have dropped.
    """
    valid = {}  # month -> seconds
    skipped = []
    for s in sessions:
        try:
            minutes = float(s["minutes_on_callsign"])
        except (KeyError, TypeError, ValueError):
            continue
        month = s["start"][:7]
        if minutes >= BAD_SESSION_MINUTES:
            skipped.append(s)
            continue
        valid[month] = valid.get(month, 0.0) + minutes * 60.0
    if online_secs:
        valid[current_month] = valid.get(current_month, 0.0) + online_secs

    rows = []
    for month in sorted(set(valid) | set(stored)):
        true_secs = valid.get(month, 0.0)
        pipeline_secs = stored.get(month, 0) * 60 - adjustments.get(month, 0)
        delta = round(true_secs - pipeline_secs)
        if abs(delta) > tolerance:
            rows.append(
                (
                    month,
                    delta,
                    f"computed {true_secs / 60.0:.1f}m vs pipeline {pipeline_secs / 60.0:.1f}m",
                )
            )
    return rows, skipped


def main():
    parser = argparse.ArgumentParser(
        description="Generate controller_activity_manual_adjustment INSERTs "
        "reconciling stored activity with VATSIM sessions."
    )
    parser.add_argument("start_date", help="YYYY-MM-DD; look at sessions since this date")
    parser.add_argument("db", help="path to the vzdv sqlite database")
    parser.add_argument("cid", nargs="?", type=int, default=None,
                        help="reconcile only this CID; omit for all on-roster controllers")
    parser.add_argument("--config", default=None,
                        help="vzdv toml config for the stats callsign filters "
                        "(default: ./vzdv.toml or ./vzdv.example.toml)")
    parser.add_argument("--delay", type=float, default=8.0,
                        help="seconds between VATSIM API calls (default: 8)")
    parser.add_argument("--tolerance", type=int, default=60,
                        help="ignore per-month deltas smaller than this many seconds "
                        "(default: 60)")
    args = parser.parse_args()

    try:
        datetime.strptime(args.start_date, "%Y-%m-%d")
    except ValueError:
        parser.error(f"invalid start date {args.start_date!r}; expected YYYY-MM-DD")
    start_month = args.start_date[:7]

    config_path = args.config
    if config_path is None:
        for candidate in ("vzdv.toml", "vzdv.example.toml"):
            if os.path.exists(candidate):
                config_path = candidate
                break
        else:
            parser.error("no vzdv config found; pass --config with a vzdv.toml path")
    if not os.path.exists(config_path):
        parser.error(f"config not found: {config_path}")
    if not os.path.exists(args.db):
        parser.error(f"database not found: {args.db}")
    prefixes, suffixes = load_stats_filters(config_path)

    conn = sqlite3.connect(f"file:{args.db}?mode=ro", uri=True)
    conn.row_factory = sqlite3.Row

    if args.cid is not None:
        row = conn.execute(
            "SELECT cid, first_name, last_name, is_on_roster FROM controller WHERE cid = ?",
            (args.cid,),
        ).fetchone()
        if row is None:
            print(f"warning: CID {args.cid} is not in the controller table", file=sys.stderr)
            targets = [(args.cid, "unknown")]
        else:
            if not row["is_on_roster"]:
                print(f"warning: CID {args.cid} is not on the roster", file=sys.stderr)
            targets = [(row["cid"], f"{row['first_name']} {row['last_name']}")]
    else:
        targets = [
            (r_["cid"], f"{r_['first_name']} {r_['last_name']}")
            for r_ in conn.execute(
                "SELECT cid, first_name, last_name FROM controller "
                "WHERE is_on_roster = 1 ORDER BY cid"
            )
        ]
    if not targets:
        print("no controllers to reconcile", file=sys.stderr)
        return 1

    stored = {}  # cid -> {month: minutes}
    for r_ in conn.execute(
        "SELECT cid, month, minutes FROM activity WHERE month >= ?", (start_month,)
    ):
        stored.setdefault(r_["cid"], {})[r_["month"]] = r_["minutes"]

    adjustments = {}  # cid -> {month: seconds}
    has_adj_table = conn.execute(
        "SELECT 1 FROM sqlite_master WHERE type = 'table' "
        "AND name = 'controller_activity_manual_adjustment'"
    ).fetchone()
    if has_adj_table:
        for r_ in conn.execute(
            "SELECT cid, month, SUM(seconds) AS s "
            "FROM controller_activity_manual_adjustment GROUP BY cid, month"
        ):
            adjustments.setdefault(r_["cid"], {})[r_["month"]] = r_["s"]

    target_cids = {cid for cid, _ in targets}
    online = fetch_online_seconds(target_cids, prefixes, suffixes)
    current_month = datetime.now(timezone.utc).strftime("%Y-%m")

    results = []  # (cid, name, rows, skipped, error)
    for i, (cid, name) in enumerate(targets):
        if args.cid is None:
            print(f"[{i + 1}/{len(targets)}] fetching sessions for {cid}", file=sys.stderr)
        try:
            sessions = fetch_sessions(cid, args.start_date)
            sessions = [
                s
                for s in sessions
                if in_facility(s.get("callsign") or "", prefixes, suffixes)
            ]
            rows, skipped = reconcile(
                sessions,
                online.get(cid, 0.0),
                stored.get(cid, {}),
                adjustments.get(cid, {}),
                current_month,
                args.tolerance,
            )
            results.append((cid, name, rows, skipped, None))
        except Exception as e:
            print(f"error reconciling {cid}: {e}", file=sys.stderr)
            results.append((cid, name, [], [], str(e)))
        if i + 1 < len(targets):
            time.sleep(args.delay)

    # Pretty-printed SQL to stdout; everything above went to stderr.
    print("-- vZDV controller activity reconciliation")
    print(f"-- generated {datetime.now(timezone.utc).strftime('%Y-%m-%dT%H:%M:%SZ')}")
    print(f"-- sessions since {args.start_date}; db {args.db}")
    print(
        "-- delta = computed VATSIM seconds - (stored minutes*60 - existing adjustment seconds)"
    )
    needs_adjustment = 0
    for cid, name, rows, skipped, error in results:
        print()
        print(f"-- {cid} {name}")
        if error is not None:
            print(f"--   fetch failed; skipped: {error}")
            continue
        for s in skipped:
            print(
                f"--   dropped bugged session: conn {s.get('connection_id')} "
                f"{s.get('callsign')} {s.get('start')} "
                f"reported {float(s['minutes_on_callsign']):.1f}m"
            )
        if not rows:
            print("--   reconciled; no adjustments needed")
            continue
        needs_adjustment += 1
        print(
            "INSERT INTO controller_activity_manual_adjustment "
            "(cid, month, seconds) VALUES"
        )
        for j, (month, delta, note) in enumerate(rows):
            sep = "," if j + 1 < len(rows) else ";"
            print(f"    -- {note}")
            print(f"    ({cid}, '{month}', {delta}){sep}")

    print(
        f"\n-- {needs_adjustment}/{len(results)} controllers need adjustments",
        file=sys.stderr,
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
