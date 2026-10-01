# vzdv-tasks

Background task runner for the vZDV site. A long-running binary built on
`clokwerk::AsyncScheduler` that shares the SQLite database and `vzdv.toml`
config with `vzdv-site` and `vzdv-bot`.

Run it like the other binaries:

```sh
cargo run --bin vzdv-tasks [-- --config vzdv.toml] [-d]
```

## Scheduled jobs

| Interval | Job | Code |
| -------- | --- | ---- |
| 5 min | Partial roster update (recently-active controllers) | `roster.rs` |
| 2 hours | Full roster sync against VATUSA | `roster.rs` |
| 15 min | Spot-update activity for online controllers + reconcile recently-offline | `activity.rs` |
| 6 hours | Full activity true-up for every roster controller | `activity.rs` |
| 30 min | Expire solo certs | `solo_cert.rs` |
| 12 hours | Expire no-shows | `no_show_expiration.rs` |
| 5 min | Clean up stale ATIS rows | `atis.rs` |
| 24 hours | Currency reminder emails near quarter end | `currency.rs` |

`traffic_tracking.rs` is not scheduled; it just snapshots the v3 live data
feed into `kvs` for other code to consume.

Partial and full syncs for roster and activity share `tokio::sync::Semaphore`
guards (one per domain). Partial jobs `try_acquire` and skip if a full job is
running; full jobs `.acquire()` and block partials while running. Do not
remove these — the jobs genuinely overlap.

## Activity tracking

Activity rows are `activity(cid, month, minutes)`, where `month` is
`YYYY-MM` and a session counts toward the month it **started** in.

- **Spot updates (15 min)**: for each currently-online, on-roster,
  in-facility controller, writes
  `completed sessions this month + manual adjustments + online_seconds`,
  where `online_seconds = now - logon_time` clamped to the current month
  and a 24-hour maximum. Stored values are snapshots — they trail real time
  by up to a tick.
- **Recently-offline reconcile**: a session only appears in the VATSIM API
  after it ends, so the last tick of a session misses up to 15 minutes.
  Controllers who drop offline are re-reconciled ~30 minutes later (up to 3
  attempts if the session hasn't finalized), covering their session-start
  month and the drop month.
- **Full true-up (6 hours)**: refetches ~6 months of sessions per controller
  and rewrites their activity rows in a transaction, including current
  in-progress session time.

The VATSIM ratings API is rate-limited; all sweeps sleep 8 seconds between
controllers, so a full true-up takes ~15 minutes.

## The VATSIM API bug

Occasionally the API reports a session with a wildly wrong duration —
`minutes_on_callsign` of weeks' worth, with a bogus `end` time. The real
duration is not recoverable from the API (`total_minutes_on_callsign` is a
lifetime cumulative).

The pipeline drops any session reporting `>= 1440` minutes (24 hours) and
warns once per session to the `errors` Discord webhook, deduplicated by a
`bugged_session:{connection_id}` marker in the `kvs` table. The lost time
must be restored with a manual adjustment.

## Manually adjusting a controller's time

Insert a row into `controller_activity_manual_adjustment`. Values are
**seconds added** to the month's computed activity (negative values
subtract):

```sql
INSERT INTO controller_activity_manual_adjustment (cid, month, seconds)
VALUES (1809927, '2026-08', 15540);  -- +4h19m for August 2026
```

Multiple rows per `(cid, month)` are allowed and summed. Rows older than the
true-up window (~6 months) stop being applied as that month ages out of the
recomputed set.

Timing: current-month adjustments apply on the next spot tick while the
controller is online, or the next true-up otherwise; past-month adjustments
apply on the next true-up (up to 6 hours). The admin activity report is
cached up to 3 hours — its "delete" button refreshes the cache.

To compute what adjustment is needed rather than guessing, use the reconcile
script, which diffs live VATSIM data against the database and prints the
INSERT statements (it does not execute them):

```sh
cd ../scripts/reconcile-hours
python3 main.py <YYYY-MM-DD> <db-path> [cid] --config ../../vzdv.toml
```
