//! Update activity from VATSIM.

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Months, NaiveDateTime, Utc};
use log::{debug, error, warn};
use sqlx::{Pool, Row, Sqlite};
use std::{
    collections::{HashMap, HashSet},
    time::Duration,
};
use tokio::{sync::Mutex, time};
use vatsim_utils::errors::VatsimUtilError;
use vatsim_utils::{
    live_api::Vatsim,
    models::{AtcSessionEntry, Controller},
    rest_api,
};
use vzdv::{
    GENERAL_HTTP_CLIENT,
    config::Config,
    position_in_facility_airspace,
    sql::{self, ControllerActivityManualAdjustment},
};

/// Everything activity tracking pulls from outside the DB: the VATSIM APIs,
/// the clock, and the rate-limit sleeps between API calls.
///
/// Abstracted so tests can run the update logic against canned data.
pub trait ActivitySource {
    /// Controllers currently connected, from the V3 live data feed.
    async fn online_controllers(&self) -> Result<Vec<Controller>>;

    /// A controller's ATC sessions starting on or after `start` (`YYYY-MM-DD`).
    async fn atc_sessions(
        &self,
        cid: u64,
        start: &str,
    ) -> Result<Vec<AtcSessionEntry>, VatsimUtilError>;

    /// The current time.
    fn now(&self) -> DateTime<Utc>;

    /// Wait out a delay between VATSIM API calls.
    async fn sleep(&self, duration: Duration);
}

/// The real VATSIM APIs and clock.
pub struct LiveVatsim;

impl ActivitySource for LiveVatsim {
    async fn online_controllers(&self) -> Result<Vec<Controller>> {
        let vatsim = Vatsim::new().await?;
        Ok(vatsim.get_v3_data().await?.controllers)
    }

    async fn atc_sessions(
        &self,
        cid: u64,
        start: &str,
    ) -> Result<Vec<AtcSessionEntry>, VatsimUtilError> {
        /*
         * I'm not (currently) worried about pagination as even the facility's most
         * active controllers don't have enough sessions in the true-up's time range
         * to go over the endpoint's single-page response limit.
         */
        Ok(
            rest_api::get_atc_sessions(cid, None, None, Some(start), None)
                .await?
                .results,
        )
    }

    fn now(&self) -> DateTime<Utc> {
        Utc::now()
    }

    async fn sleep(&self, duration: Duration) {
        time::sleep(duration).await;
    }
}

/// How far apart a live-feed logon time and a completed session's start can
/// be for them to be the same connection. The sessions API reports the logon
/// time truncated to the second.
const SESSION_START_TOLERANCE_SECS: i64 = 1;

/// Seconds a controller has been online this session, clamped to the current
/// month and a 24-hour maximum.
///
/// A session that spans a month boundary attributes all of its time to the
/// month it started in, so a still-open connection should likewise only
/// contribute time since the month started. The 24-hour cap guards against
/// the VATSIM API bug of wildly-wrong timestamps (here, `logon_time`).
fn clamped_online_seconds(
    logon_time: &str,
    now: &DateTime<Utc>,
    month_start_ts: i64,
) -> Result<i64> {
    let logon_ts = DateTime::parse_from_rfc3339(logon_time)?.timestamp();
    let now_ts = now.timestamp();
    Ok((now_ts - logon_ts)
        .min(now_ts - month_start_ts)
        .clamp(0, 86_400))
}

/// Whether a live-feed connection already shows up as a completed session.
///
/// The live feed and the sessions API are fetched at different times — up to
/// a whole true-up sweep apart — so a controller can still be listed online
/// after their just-ended connection has landed in the API. Counting it as
/// online time too would count it twice.
fn connection_completed(sessions: &[AtcSessionEntry], connection: &Controller) -> bool {
    let Ok(logon) = DateTime::parse_from_rfc3339(&connection.logon_time) else {
        return false;
    };
    sessions.iter().any(|session| {
        session.callsign == connection.callsign
            && session
                .start
                .get(..19)
                .and_then(|start| NaiveDateTime::parse_from_str(start, "%Y-%m-%dT%H:%M:%S").ok())
                .is_some_and(|start| {
                    (start.and_utc().timestamp() - logon.timestamp()).abs()
                        <= SESSION_START_TOLERANCE_SECS
                })
    })
}

/// Get all currently-online, in-facility connections by CID, each with its
/// clamped online seconds as of now.
///
/// Mirrors `update_single_activity`'s `online_seconds` handling, so that a
/// full true-up doesn't drop in-progress session time from the current month
/// and cause a visible dip until the next spot-update.
async fn online_connections(
    source: &impl ActivitySource,
    config: &Config,
) -> Result<HashMap<u32, Vec<(Controller, i64)>>> {
    let controllers = source.online_controllers().await?;
    let now = source.now();
    let month_start =
        DateTime::parse_from_rfc3339(&format!("{}T00:00:00Z", now.format("%Y-%m-01")))?.timestamp();
    let mut map: HashMap<u32, Vec<(Controller, i64)>> = HashMap::new();
    for controller in controllers {
        if !position_in_facility_airspace(config, &controller.callsign) {
            continue;
        }
        match clamped_online_seconds(&controller.logon_time, &now, month_start) {
            Ok(secs) => map
                .entry(controller.cid as u32)
                .or_default()
                .push((controller, secs)),
            Err(e) => warn!("Could not parse logon time for {}: {e}", controller.cid),
        }
    }
    Ok(map)
}

/// Notify the errors Discord webhook of a bugged session, once per session.
///
/// The VATSIM API bug sets `minutes_on_callsign` to multiple weeks' worth of
/// time; the real session time is dropped and needs a manual adjustment to
/// restore. The `kvs` table tracks which sessions have already been reported
/// so the warning is only sent once.
async fn notify_bugged_session(
    config: &Config,
    db: &Pool<Sqlite>,
    cid: u32,
    session: &AtcSessionEntry,
) -> Result<()> {
    let key = format!("bugged_session:{}", session.connection_id);
    if sqlx::query(sql::GET_KVS_ENTRY)
        .bind(&key)
        .fetch_optional(db)
        .await?
        .is_some()
    {
        return Ok(());
    }
    if !config.discord.webhooks.errors.is_empty() {
        GENERAL_HTTP_CLIENT
            .post(&config.discord.webhooks.errors)
            .json(&serde_json::json!({
                "content": format!(
                    "Bugged ATC session detected: {cid} on {} (connection {}) starting {} reported {} minutes. \
                     The session's time was dropped; insert a manual adjustment to restore it.",
                    session.callsign,
                    session.connection_id,
                    session.start,
                    session.minutes_on_callsign
                )
            }))
            .send()
            .await?
            .error_for_status()?;
    }
    sqlx::query(sql::UPSERT_KVS_ENTRY)
        .bind(&key)
        .bind(Utc::now().to_rfc3339())
        .execute(db)
        .await?;
    Ok(())
}

/// Update the activity for a single controller, looking back several
/// months and replacing the DB records with new data from VATSIM.
async fn true_up_single_activity(
    source: &impl ActivitySource,
    config: &Config,
    db: &Pool<Sqlite>,
    five_months_ago: &str,
    cid: u32,
    online: &HashMap<u32, Vec<(Controller, i64)>>,
) -> Result<()> {
    // get the last 5 months of the controller's activity
    let sessions = match source.atc_sessions(cid as u64, five_months_ago).await {
        Ok(data) => data,
        Err(VatsimUtilError::InvalidStatusCode(code)) => {
            warn!("Getting rate limited on activity API; waiting 30 seconds before continuing");
            source.sleep(Duration::from_secs(30)).await;
            bail!("getting activity for {cid}; got HTTP response {code}")
        }
        Err(VatsimUtilError::FailedJsonParse(e)) => bail!("failed parsing activity for {cid}: {e}"),
        Err(e) => bail!("getting activity for {cid}: {e}"),
    };

    // group the controller's activity by month
    let mut seconds_map: HashMap<String, f32> = HashMap::new();
    for session in &sessions {
        // filter to only sessions in the facility
        if !position_in_facility_airspace(config, &session.callsign) {
            continue;
        }

        let month = session.start[0..7].to_string();
        let seconds = session.minutes_on_callsign.parse::<f32>().unwrap() * 60.0;

        if seconds >= 86400.0 {
            // Bug in VATSIM api setting controllers' start timestamp to multiple weeks out
            // Simply unreasonable unless someone is doing a 24-hour run lol
            // But we have the manual adjustments to use to correct these
            warn!("Controller {cid} has {seconds} seconds in month {month}; skipping");
            if let Err(e) = notify_bugged_session(config, db, cid, session).await {
                error!("Error sending bugged-session notification: {e}");
            }
            continue;
        }

        seconds_map
            .entry(month)
            .and_modify(|acc| *acc += seconds)
            .or_insert(seconds);
    }

    // transaction for these queries
    let mut tx = db.begin().await?;

    // See if we made any manual adjustments to a controller's time due to the api bug
    let manual_adjustments: Vec<ControllerActivityManualAdjustment> =
        sqlx::query_as(sql::GET_CONTROLLER_ACTIVITY_MANUAL_ADJUSTMENT)
            .bind(cid)
            .fetch_all(&mut *tx)
            .await
            .with_context(|| format!("Getting manual adjustment for CID {cid}"))?;

    let five_mo_ago_time = chrono::DateTime::parse_from_rfc3339(
        format!("{}-01T00:00:00Z", &five_months_ago[..7]).as_str(),
    )?;
    let mut months = HashSet::new();
    for i in 0..=5 {
        let month_delta = Months::new(i);
        let month = five_mo_ago_time
            .checked_add_months(month_delta)
            .context("Unable to add month_delta to five_mo_ago datetime")?;
        let month_stub = month.to_string()[..7].to_string();
        months.insert(month_stub);
    }

    for row in manual_adjustments {
        if !months.contains(&row.month) {
            continue;
        }
        seconds_map
            .entry(row.month)
            .and_modify(|acc| *acc += row.seconds as f32)
            .or_insert(row.seconds as f32);
    }

    // include in-progress session time for currently-online controllers so
    // this rewrite of the current month doesn't dip below the spot-update
    // value, unless the connection has ended since the feed was fetched
    if let Some(connections) = online.get(&cid) {
        let secs: i64 = connections
            .iter()
            .filter(|(connection, _)| !connection_completed(&sessions, connection))
            .map(|(_, secs)| secs)
            .sum();
        seconds_map
            .entry(source.now().format("%Y-%m").to_string())
            .and_modify(|acc| *acc += secs as f32)
            .or_insert(secs as f32);
    }

    // clear the controller's existing records in prep for replacement
    sqlx::query(sql::DELETE_ACTIVITY_FOR_CID)
        .bind(cid)
        .execute(&mut *tx)
        .await
        .with_context(|| format!("Processing CID {cid}"))?;
    // for each relevant month, store their total controlled minutes in the DB
    for (month, seconds) in seconds_map {
        let minutes = (seconds / 60.0).round() as u32;
        sqlx::query(sql::INSERT_INTO_ACTIVITY)
            .bind(cid)
            .bind(month)
            .bind(minutes)
            .execute(&mut *tx)
            .await
            .with_context(|| format!("Processing CID {cid}"))?;
    }
    // commit the controller's changes
    tx.commit().await?;

    Ok(())
}

/// Update all controllers' stored activity data with data from VATSIM.
///
/// For each controller in the DB, their activity data will be cleared,
/// and then (for on-roster controllers) fetched and stored in the DB as
/// part of a transaction.
pub async fn true_up_all_controllers_activity(
    source: &impl ActivitySource,
    config: &Config,
    db: &Pool<Sqlite>,
) -> Result<()> {
    // prep cids for on-roster controllers and a 5-month-ago timestamp that the API recognizes
    let controllers = sqlx::query(sql::GET_ALL_ROSTER_CONTROLLER_CIDS)
        .fetch_all(db)
        .await?;
    let five_months_ago = source
        .now()
        .checked_sub_months(Months::new(5))
        .unwrap()
        .format("%Y-%m-01")
        .to_string();
    let online = match online_connections(source, config).await {
        Ok(map) => map,
        Err(e) => {
            warn!("Could not get live VATSIM data for activity true-up: {e}");
            HashMap::new()
        }
    };
    for row in controllers {
        let cid: u32 = row.try_get("cid").expect("no 'cid' column");
        debug!("Getting activity for {cid}");
        if let Err(e) =
            true_up_single_activity(source, config, db, &five_months_ago, cid, &online).await
        {
            error!("Error updating activity for {cid}: {e}");
        }
        // wait 8 seconds to adhere to the VATSIM API rate limits
        source.sleep(Duration::from_secs(8)).await;
    }
    Ok(())
}

/// Updates a single controller's activity just this month.
///
/// `connection` is the controller's current connection from the live feed,
/// or `None` when reconciling one that has gone offline (only completed
/// sessions are counted then). Returns whether the activity row
/// was written: an offline reconcile that would reduce the stored minutes —
/// meaning the just-ended session likely hasn't finalized in the API yet —
/// is skipped instead of written, unless `allow_regress` is set (used when
/// clearing a transient spot-update row for a month the session doesn't
/// belong to).
async fn update_single_activity(
    source: &impl ActivitySource,
    config: &Config,
    db: &Pool<Sqlite>,
    start_of_month: &str,
    cid: u64,
    connection: Option<&Controller>,
    allow_regress: bool,
) -> Result<bool> {
    let year_and_month = &start_of_month[..7];

    let sessions = source.atc_sessions(cid, start_of_month).await?;
    let mut counter = 0.0;
    for session in &sessions {
        if !position_in_facility_airspace(config, &session.callsign) {
            continue;
        }
        // sessions count toward the month they started in; the API start
        // param is only a lower bound, so drop later months' sessions when
        // reconciling a past month
        if !session.start.starts_with(year_and_month) {
            continue;
        }

        let minutes_on_callsign = session.minutes_on_callsign.parse::<f32>().unwrap();
        if minutes_on_callsign >= 1440.0 {
            // VATSIM API Bug, ignore this and let the manual adjustment apply
            continue;
        }

        counter += minutes_on_callsign * 60.0;
    }

    let adjustment_seconds = {
        let adjustments: Vec<sql::ControllerActivityManualAdjustment> =
            sqlx::query_as(sql::GET_CONTROLLER_ACTIVITY_MANUAL_ADJUSTMENT_FOR_MONTH)
                .bind(cid as u32)
                .bind(year_and_month)
                .fetch_all(db)
                .await?;
        adjustments
            .into_iter()
            .map(|adj| adj.seconds as f32)
            .sum::<f32>()
    };

    let now = source.now();
    let month_start =
        DateTime::parse_from_rfc3339(&format!("{start_of_month}T00:00:00Z"))?.timestamp();
    let online_seconds = match connection {
        // the connection may have ended since the feed was fetched
        Some(connection) if !connection_completed(&sessions, connection) => {
            clamped_online_seconds(&connection.logon_time, &now, month_start)?
        }
        _ => 0,
    };
    let minutes = ((counter + adjustment_seconds + online_seconds as f32) / 60.0).round() as u32;

    if connection.is_none() && !allow_regress {
        // a session only appears in the API once it ends; if the just-ended
        // session hasn't finalized yet, this recompute would regress the
        // stored minutes, so skip the write and let the caller retry
        let stored: Option<i64> = sqlx::query_scalar(sql::GET_ACTIVITY_MINUTES_FOR_CID_MONTH)
            .bind(cid as u32)
            .bind(year_and_month)
            .fetch_optional(db)
            .await?;
        if stored.is_some_and(|stored| stored > i64::from(minutes)) {
            debug!("Offline reconcile for {cid} would regress stored minutes; deferring");
            return Ok(false);
        }
    }

    // update the controller's time for this month (if able)
    let result = sqlx::query(sql::UPDATE_ACTIVITY)
        .bind(cid as u32)
        .bind(year_and_month)
        .bind(minutes)
        .execute(db)
        .await
        .with_context(|| format!("Updating CID {cid}"))?;
    if result.rows_affected() == 0 {
        // The controller hasn't yet completed a full session for this month,
        // so no rows were updated. Insert a new row.
        sqlx::query(sql::INSERT_INTO_ACTIVITY)
            .bind(cid as u32)
            .bind(year_and_month)
            .bind(minutes)
            .execute(db)
            .await
            .with_context(|| format!("Inserting new activity row for spot-update for {cid}"))?;
    }

    Ok(true)
}

/// Ticks to wait after a disconnect before reconciling, giving the VATSIM
/// API time to finalize the session (≈30 minutes at a 15-minute tick).
const OFFLINE_RECONCILE_WAIT_TICKS: u8 = 2;
/// Maximum reconcile attempts when the result would regress the stored
/// minutes (i.e. the session still hasn't finalized in the API).
const OFFLINE_RECONCILE_MAX_TRIES: u8 = 3;

/// Controllers that have recently gone offline, pending a final reconcile.
///
/// A session only appears in the ATC sessions API once it has ended, so the
/// last spot-update of a session is always missing the stretch between that
/// tick and the disconnect (up to the tick interval). This state re-runs the
/// update for controllers shortly after they drop offline to recover it.
#[derive(Default)]
pub struct RecentlyOffline {
    /// In-facility, on-roster CIDs online on the last tick, mapped to the
    /// months that would need reconciling if they dropped: the month their
    /// session started in plus the month they were last seen online in.
    previous: HashMap<u64, HashSet<String>>,
    /// CID -> (months to reconcile, ticks to wait, failed reconcile tries).
    pending: HashMap<u64, (HashSet<String>, u8, u8)>,
}

impl RecentlyOffline {
    /// Advance the state to a new tick, returning `(cid, months)` pairs due
    /// for a reconcile attempt.
    fn tick(&mut self, online: &HashMap<u64, HashSet<String>>) -> Vec<(u64, HashSet<String>)> {
        // a controller who's back online is handled by the normal path
        self.pending.retain(|cid, _| !online.contains_key(cid));
        // newly-dropped controllers wait before reconciling so the API can
        // finalize their just-ended session
        for (cid, months) in &self.previous {
            if !online.contains_key(cid) {
                self.pending
                    .insert(*cid, (months.clone(), OFFLINE_RECONCILE_WAIT_TICKS, 0));
            }
        }
        self.previous.clone_from(online);

        let mut due = Vec::new();
        for (cid, (months, wait, _)) in &mut self.pending {
            if *wait == 0 {
                due.push((*cid, months.clone()));
            } else {
                *wait -= 1;
            }
        }
        due
    }

    /// Record a reconcile attempt that should be retried on a later tick
    /// (the write would have regressed stored minutes, or the request
    /// failed), unless the attempt cap is reached.
    fn mark_regressed(&mut self, cid: u64) {
        match self.pending.get_mut(&cid) {
            Some((_, wait, tries)) if *tries + 1 < OFFLINE_RECONCILE_MAX_TRIES => {
                *tries += 1;
                *wait = 1;
            }
            _ => {
                self.pending.remove(&cid);
            }
        }
    }
}

/// Update this month's activity for currently online controllers, plus a
/// final reconcile for controllers that recently went offline.
pub async fn update_online_controller_activity(
    source: &impl ActivitySource,
    config: &Config,
    db: &Pool<Sqlite>,
    recent: &Mutex<RecentlyOffline>,
) -> Result<()> {
    let on_roster_cids: Vec<u64> = {
        let rows = sqlx::query(sql::GET_ALL_ROSTER_CONTROLLER_CIDS)
            .fetch_all(db)
            .await?;
        rows.iter()
            .map(|row| row.try_get("cid").expect("no 'cid' column"))
            .collect()
    };
    let online_controllers = source.online_controllers().await?;
    let start_of_month = source.now().format("%Y-%m-01").to_string();

    let year_and_month = &start_of_month[..7];
    let mut online: HashMap<u64, HashSet<String>> = HashMap::new();
    for controller in &online_controllers {
        let cid = controller.cid;
        if !on_roster_cids.contains(&cid) {
            continue;
        }
        if !position_in_facility_airspace(config, &controller.callsign) {
            // ignore controlling in other facilities and observers
            continue;
        }
        // sessions count toward the month they started in, which may differ
        // from the current month for a connection spanning a boundary
        let session_month = controller
            .logon_time
            .get(..7)
            .unwrap_or(year_and_month)
            .to_string();
        online.insert(
            cid,
            HashSet::from([session_month, year_and_month.to_string()]),
        );
        debug!("Spot-updating activity for {cid}");
        if let Err(e) = update_single_activity(
            source,
            config,
            db,
            &start_of_month,
            cid,
            Some(controller),
            false,
        )
        .await
        {
            error!("Error spot-updating CID {cid}: {e}")
        }
        // wait 8 seconds to adhere to the VATSIM API rate limits
        source.sleep(Duration::from_secs(8)).await;
    }

    // take the due list in its own statement: a guard created in the `for`
    // expression lives for the whole loop, so the `recent.lock()` calls below
    // would deadlock on it
    let due = recent.lock().await.tick(&online);
    for (cid, months) in due {
        // earliest month first — that's the session-start month, which keeps
        // the no-regress guard while its session finalizes; any later month
        // (a spot-update row from a cross-boundary connection) is rewritten
        // unconditionally
        let mut months: Vec<String> = months.into_iter().collect();
        months.sort_unstable();
        let mut settled = true;
        for (i, month) in months.iter().enumerate() {
            debug!("Reconciling recently-offline CID {cid} for {month}");
            match update_single_activity(
                source,
                config,
                db,
                &format!("{month}-01"),
                cid,
                None,
                i > 0,
            )
            .await
            {
                Ok(true) => {}
                Ok(false) => settled = false,
                Err(e) => {
                    error!("Error reconciling offline CID {cid} for {month}: {e}");
                    settled = false;
                }
            }
            // wait 8 seconds to adhere to the VATSIM API rate limits
            source.sleep(Duration::from_secs(8)).await;
        }
        if settled {
            recent.lock().await.pending.remove(&cid);
        } else {
            recent.lock().await.mark_regressed(cid);
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use sqlx::{Executor, sqlite::SqlitePoolOptions};

    async fn test_db() -> Pool<Sqlite> {
        let db = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        db.execute(sql::CREATE_TABLES).await.unwrap();
        db
    }

    fn test_config() -> Config {
        let mut config = Config::default();
        config.stats.position_prefixes = ["APA", "ASE", "COS", "DEN", "EGE"]
            .map(String::from)
            .to_vec();
        config.stats.position_suffixes =
            ["_GND", "_TWR", "_APP", "_CTR"].map(String::from).to_vec();
        config
    }

    /// Canned VATSIM data: the live feed's online controllers and each CID's
    /// completed sessions, as of `now`.
    struct MockVatsim {
        now: DateTime<Utc>,
        online: Vec<Controller>,
        sessions: HashMap<u64, Vec<AtcSessionEntry>>,
    }

    impl MockVatsim {
        fn at(now: &str) -> Self {
            Self {
                now: utc(now),
                online: Vec::new(),
                sessions: HashMap::new(),
            }
        }
    }

    impl ActivitySource for MockVatsim {
        async fn online_controllers(&self) -> Result<Vec<Controller>> {
            Ok(self.online.clone())
        }

        async fn atc_sessions(
            &self,
            cid: u64,
            start: &str,
        ) -> Result<Vec<AtcSessionEntry>, VatsimUtilError> {
            // like the API's `start` param, filter on when the session started
            Ok(self
                .sessions
                .get(&cid)
                .into_iter()
                .flatten()
                .filter(|session| session.start.as_str() >= start)
                .cloned()
                .collect())
        }

        fn now(&self) -> DateTime<Utc> {
            self.now
        }

        async fn sleep(&self, _duration: Duration) {}
    }

    fn utc(timestamp: &str) -> DateTime<Utc> {
        timestamp.parse().unwrap()
    }

    /// A completed session as the ATC sessions API reports it.
    fn session(callsign: &str, start: &str, minutes: &str) -> AtcSessionEntry {
        AtcSessionEntry {
            callsign: callsign.to_string(),
            start: start.to_string(),
            minutes_on_callsign: minutes.to_string(),
            ..Default::default()
        }
    }

    /// A connected controller as the V3 live data feed reports it.
    fn online(cid: u32, callsign: &str, logon_time: &str) -> Controller {
        Controller {
            cid: cid.into(),
            callsign: callsign.to_string(),
            logon_time: logon_time.to_string(),
            ..Default::default()
        }
    }

    /// Stand-in CID for the controller in `TEST_CONTROLLER_SESSIONS`.
    const TEST_CONTROLLER: u32 = 456;

    /// `TEST_CONTROLLER`'s facility sessions, 2026-09-16 through 2026-10-03:
    /// (callsign, start, minutes_on_callsign).
    const TEST_CONTROLLER_SESSIONS: &[(&str, &str, &str)] = &[
        ("APA_GND", "2026-10-03T00:05:39", "132.0"),
        ("COS_GND", "2026-10-02T23:35:11", "30.35"),
        ("COS_GND", "2026-10-02T23:34:56", "0.166667"),
        ("COS_GND", "2026-10-02T22:58:49", "36.05"),
        ("APA_GND", "2026-10-02T21:26:43", "9.1"),
        ("APA_GND", "2026-10-02T20:55:25", "31.25"),
        ("ASE_GND", "2026-10-02T20:13:14", "40.233333"),
        ("ASE_GND", "2026-10-02T18:16:09", "117.0"),
        ("COS_GND", "2026-10-02T13:16:25", "119.0"),
        ("COS_GND", "2026-10-02T13:07:52", "8.5"),
        ("COS_GND", "2026-10-01T20:28:50", "50.116667"),
        ("COS_GND", "2026-10-01T18:59:36", "89.183333"),
        ("ASE_GND", "2026-10-01T16:28:21", "150.983333"),
        ("ASE_GND", "2026-09-26T15:25:34", "55.05"),
        ("ASE_GND", "2026-09-24T23:39:49", "94.266667"),
        ("APA_GND", "2026-09-24T19:12:20", "124.266667"),
        ("COS_GND", "2026-09-22T23:29:51", "120.466667"),
        ("ASE_GND", "2026-09-22T18:16:48", "96.4"),
        ("COS_GND", "2026-09-20T22:23:18", "143.183333"),
        ("APA_GND", "2026-09-20T21:48:50", "34.233333"),
        ("ASE_GND", "2026-09-20T18:40:36", "187.766667"),
        ("EGE_GND", "2026-09-19T14:50:08", "60.15"),
        ("ASE_GND", "2026-09-19T13:27:03", "81.083333"),
        ("ASE_GND", "2026-09-19T01:56:14", "184.3"),
        ("ASE_GND", "2026-09-19T00:17:53", "98.25"),
        ("ASE_GND", "2026-09-18T19:26:10", "84.383333"),
        ("COS_GND", "2026-09-18T16:56:47", "56.033333"),
        ("COS_GND", "2026-09-18T15:12:49", "96.7"),
        ("COS_GND", "2026-09-17T11:50:59", "120.566667"),
        ("COS_GND", "2026-09-16T00:20:13", "160.683333"),
    ];

    /// `TEST_CONTROLLER`'s sessions that started before `before`.
    fn test_controller_sessions(before: &str) -> Vec<AtcSessionEntry> {
        TEST_CONTROLLER_SESSIONS
            .iter()
            .filter(|(_, start, _)| *start < before)
            .map(|(callsign, start, minutes)| session(callsign, start, minutes))
            .collect()
    }

    async fn add_controller(db: &Pool<Sqlite>, cid: u32, on_roster: bool) {
        sqlx::query(
            "INSERT INTO controller (cid, first_name, last_name, is_on_roster) VALUES ($1, '', '', $2)",
        )
        .bind(cid)
        .bind(on_roster)
        .execute(db)
        .await
        .unwrap();
    }

    async fn set_minutes(db: &Pool<Sqlite>, cid: u32, month: &str, minutes: u32) {
        sqlx::query(sql::INSERT_INTO_ACTIVITY)
            .bind(cid)
            .bind(month)
            .bind(minutes)
            .execute(db)
            .await
            .unwrap();
    }

    async fn add_adjustment(db: &Pool<Sqlite>, cid: u32, month: &str, seconds: i32) {
        sqlx::query(
            "INSERT INTO controller_activity_manual_adjustment (cid, month, seconds) VALUES ($1, $2, $3)",
        )
        .bind(cid)
        .bind(month)
        .bind(seconds)
        .execute(db)
        .await
        .unwrap();
    }

    async fn stored_minutes(db: &Pool<Sqlite>, cid: u32, month: &str) -> Option<i64> {
        sqlx::query_scalar(sql::GET_ACTIVITY_MINUTES_FOR_CID_MONTH)
            .bind(cid)
            .bind(month)
            .fetch_optional(db)
            .await
            .unwrap()
    }

    async fn all_minutes(db: &Pool<Sqlite>, cid: u32) -> Vec<(String, i64)> {
        sqlx::query_as("SELECT month, minutes FROM activity WHERE cid=$1 ORDER BY month")
            .bind(cid)
            .fetch_all(db)
            .await
            .unwrap()
    }

    /// Run one spot-update tick, failing instead of hanging if it deadlocks.
    async fn tick(source: &MockVatsim, db: &Pool<Sqlite>, recent: &Mutex<RecentlyOffline>) {
        time::timeout(
            Duration::from_secs(5),
            update_online_controller_activity(source, &test_config(), db, recent),
        )
        .await
        .expect("spot-update tick hung")
        .unwrap();
    }

    #[test]
    fn test_clamped_online_seconds_normal() {
        let now = Utc.with_ymd_and_hms(2026, 9, 15, 12, 0, 0).unwrap();
        let month_start = Utc
            .with_ymd_and_hms(2026, 9, 1, 0, 0, 0)
            .unwrap()
            .timestamp();
        let seconds = clamped_online_seconds("2026-09-15T10:00:00Z", &now, month_start).unwrap();
        assert_eq!(seconds, 7_200);
    }

    #[test]
    fn test_clamped_online_seconds_caps_at_24h() {
        let now = Utc.with_ymd_and_hms(2026, 9, 15, 12, 0, 0).unwrap();
        let month_start = Utc
            .with_ymd_and_hms(2026, 9, 1, 0, 0, 0)
            .unwrap()
            .timestamp();
        // logged on 5 days ago — more than the 24-hour cap
        let seconds = clamped_online_seconds("2026-09-10T10:00:00Z", &now, month_start).unwrap();
        assert_eq!(seconds, 86_400);
    }

    #[test]
    fn test_clamped_online_seconds_bounded_by_month_start() {
        let now = Utc.with_ymd_and_hms(2026, 9, 1, 2, 0, 0).unwrap();
        let month_start = Utc
            .with_ymd_and_hms(2026, 9, 1, 0, 0, 0)
            .unwrap()
            .timestamp();
        // logged on last month — only this month's share counts
        let seconds = clamped_online_seconds("2026-08-31T20:00:00Z", &now, month_start).unwrap();
        assert_eq!(seconds, now.timestamp() - month_start);
    }

    #[test]
    fn test_clamped_online_seconds_future_logon_is_zero() {
        let now = Utc.with_ymd_and_hms(2026, 9, 15, 12, 0, 0).unwrap();
        let month_start = Utc
            .with_ymd_and_hms(2026, 9, 1, 0, 0, 0)
            .unwrap()
            .timestamp();
        let seconds = clamped_online_seconds("2026-09-16T00:00:00Z", &now, month_start).unwrap();
        assert_eq!(seconds, 0);
    }

    #[test]
    fn test_clamped_online_seconds_bad_timestamp() {
        let now = Utc.with_ymd_and_hms(2026, 9, 15, 12, 0, 0).unwrap();
        assert!(clamped_online_seconds("not a timestamp", &now, 0).is_err());
    }

    fn online_set(months: &[&str]) -> HashMap<u64, HashSet<String>> {
        HashMap::from([(123_u64, months.iter().map(|m| m.to_string()).collect())])
    }

    #[test]
    fn test_recently_offline_reconciles_after_wait() {
        let mut recent = RecentlyOffline::default();
        let online = online_set(&["2026-09"]);

        // seen online, then drops offline
        assert!(recent.tick(&online).is_empty());
        let empty = HashMap::new();
        assert!(recent.tick(&empty).is_empty());
        assert!(recent.tick(&empty).is_empty());

        let due = recent.tick(&empty);
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].0, 123);
        assert_eq!(due[0].1, HashSet::from(["2026-09".to_string()]));
        // a successful reconcile removes the entry
        recent.pending.remove(&123);
        assert!(recent.pending.is_empty());
    }

    #[test]
    fn test_recently_offline_keeps_online_month_across_boundary() {
        let mut recent = RecentlyOffline::default();
        let empty = HashMap::new();

        // session started in September and they were last online in it; the
        // drop is detected on an October tick
        recent.tick(&online_set(&["2026-09"]));
        recent.tick(&empty);
        recent.tick(&empty);

        // only September reconciles — they were never online in October
        let due = recent.tick(&empty);
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].1, HashSet::from(["2026-09".to_string()]));
    }

    #[test]
    fn test_recently_offline_online_through_boundary() {
        let mut recent = RecentlyOffline::default();
        let empty = HashMap::new();

        // session started in September; still online during an October tick,
        // then drops — the transient October spot row also needs reconciling
        recent.tick(&online_set(&["2026-09"]));
        recent.tick(&online_set(&["2026-09", "2026-10"]));
        recent.tick(&empty);
        recent.tick(&empty);

        let due = recent.tick(&empty);
        assert_eq!(due.len(), 1);
        assert_eq!(
            due[0].1,
            HashSet::from(["2026-09".to_string(), "2026-10".to_string()])
        );
    }

    #[test]
    fn test_recently_offline_reconnect_cancels_pending() {
        let mut recent = RecentlyOffline::default();
        let online = online_set(&["2026-09"]);
        let empty = HashMap::new();

        recent.tick(&online);
        recent.tick(&empty);
        // reconnect before the wait elapses
        recent.tick(&online);

        assert!(recent.pending.is_empty());
        assert!(recent.tick(&empty).is_empty());
    }

    #[test]
    fn test_recently_offline_regress_retries_then_gives_up() {
        let mut recent = RecentlyOffline::default();
        let empty = HashMap::new();

        recent.tick(&online_set(&["2026-09"]));
        recent.tick(&empty);
        recent.tick(&empty);

        // first attempt regresses (session not yet finalized); retry is queued
        assert!(!recent.tick(&empty).is_empty());
        recent.mark_regressed(123);
        assert!(recent.tick(&empty).is_empty());
        assert!(!recent.tick(&empty).is_empty());
        recent.mark_regressed(123);
        assert!(recent.tick(&empty).is_empty());
        assert!(!recent.tick(&empty).is_empty());

        // third regress exhausts the attempt cap
        recent.mark_regressed(123);
        assert!(recent.pending.is_empty());
    }

    #[tokio::test]
    async fn test_notify_bugged_session_marks_once() {
        let db = test_db().await;
        let config = Config::default(); // empty webhook URL skips the POST
        let session = AtcSessionEntry {
            connection_id: 42,
            callsign: "DEN_CTR".to_string(),
            start: "2026-08-04T23:13:03".to_string(),
            minutes_on_callsign: "19085.266667".to_string(),
            ..Default::default()
        };

        notify_bugged_session(&config, &db, 123, &session)
            .await
            .unwrap();
        notify_bugged_session(&config, &db, 123, &session)
            .await
            .unwrap();

        // still exactly one row — the second call early-returns
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM kvs")
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(count, 1);
    }

    #[tokio::test]
    async fn test_notify_bugged_session_send_failure_not_marked() {
        let db = test_db().await;
        let mut config = Config::default();
        // unroutable endpoint makes the request fail
        config.discord.webhooks.errors = "http://127.0.0.1:1/".to_string();
        let session = AtcSessionEntry::default();

        assert!(
            notify_bugged_session(&config, &db, 123, &session)
                .await
                .is_err()
        );

        // no marker written, so the next true-up will retry
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM kvs")
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn test_spot_update_counts_sessions_adjustments_and_online_time() {
        let db = test_db().await;
        add_controller(&db, 123, true).await;
        add_adjustment(&db, 123, "2026-09", 900).await;
        let mut vatsim = MockVatsim::at("2026-09-15T12:00:00Z");
        vatsim.online = vec![online(123, "DEN_CTR", "2026-09-15T11:00:00Z")];
        vatsim.sessions.insert(
            123,
            vec![
                // last month
                session("DEN_CTR", "2026-08-31T20:00:00", "100.0"),
                session("DEN_CTR", "2026-09-14T18:00:00", "60.0"),
                // another facility's position
                session("ZLA_CTR", "2026-09-14T20:00:00", "45.0"),
                // the VATSIM API bug
                session("DEN_APP", "2026-09-10T20:00:00", "20000.0"),
            ],
        );

        tick(&vatsim, &db, &Mutex::default()).await;

        // 60m session + 15m adjustment + 60m online
        assert_eq!(stored_minutes(&db, 123, "2026-09").await, Some(135));
    }

    #[tokio::test]
    async fn test_spot_update_skips_off_roster_and_out_of_facility() {
        let db = test_db().await;
        add_controller(&db, 1, false).await;
        add_controller(&db, 2, true).await;
        let mut vatsim = MockVatsim::at("2026-09-15T12:00:00Z");
        vatsim.online = vec![
            online(1, "DEN_CTR", "2026-09-15T11:00:00Z"),
            online(2, "ZLA_CTR", "2026-09-15T11:00:00Z"),
        ];
        let recent = Mutex::default();

        tick(&vatsim, &db, &recent).await;

        assert_eq!(stored_minutes(&db, 1, "2026-09").await, None);
        assert_eq!(stored_minutes(&db, 2, "2026-09").await, None);
        assert!(recent.lock().await.previous.is_empty());
    }

    #[tokio::test]
    async fn test_offline_reconcile_completes() {
        // replays the controller's 2026-10-02: stored time was stuck at their
        // Oct 1 sessions because the first offline reconcile deadlocked the tick
        let db = test_db().await;
        add_controller(&db, TEST_CONTROLLER, true).await;
        set_minutes(&db, TEST_CONTROLLER, "2026-10", 290).await;
        let recent = Mutex::default();

        let mut vatsim = MockVatsim::at("2026-10-02T14:00:00Z");
        vatsim.online = vec![online(TEST_CONTROLLER, "COS_GND", "2026-10-02T13:16:25Z")];
        vatsim.sessions.insert(
            TEST_CONTROLLER.into(),
            test_controller_sessions("2026-10-02T13:16:25"),
        );
        tick(&vatsim, &db, &recent).await;
        // Oct 1's 290.3m + 8.5m session + 43.6m online
        assert_eq!(
            stored_minutes(&db, TEST_CONTROLLER, "2026-10").await,
            Some(342)
        );

        // disconnected at 15:15:25 and the session finalized in the API
        vatsim.online.clear();
        vatsim.sessions.insert(
            TEST_CONTROLLER.into(),
            test_controller_sessions("2026-10-02T18:00:00"),
        );
        for now in ["2026-10-02T15:30:00Z", "2026-10-02T15:45:00Z"] {
            vatsim.now = utc(now);
            tick(&vatsim, &db, &recent).await;
        }
        assert_eq!(
            stored_minutes(&db, TEST_CONTROLLER, "2026-10").await,
            Some(342)
        );

        vatsim.now = utc("2026-10-02T16:00:00Z");
        tick(&vatsim, &db, &recent).await;
        // 290.3m + 8.5m + 119m
        assert_eq!(
            stored_minutes(&db, TEST_CONTROLLER, "2026-10").await,
            Some(418)
        );
        assert!(recent.lock().await.pending.is_empty());

        // and the next tick still runs
        vatsim.now = utc("2026-10-02T16:15:00Z");
        tick(&vatsim, &db, &recent).await;
    }

    #[tokio::test]
    async fn test_offline_reconcile_waits_for_session_to_finalize() {
        let db = test_db().await;
        add_controller(&db, 123, true).await;
        let recent = Mutex::default();
        let mut vatsim = MockVatsim::at("2026-09-15T12:00:00Z");
        vatsim.online = vec![online(123, "DEN_CTR", "2026-09-15T10:00:00Z")];
        tick(&vatsim, &db, &recent).await;
        assert_eq!(stored_minutes(&db, 123, "2026-09").await, Some(120));

        // offline, but the session hasn't shown up in the API yet
        vatsim.online.clear();
        for now in [
            "2026-09-15T12:15:00Z",
            "2026-09-15T12:30:00Z",
            "2026-09-15T12:45:00Z",
        ] {
            vatsim.now = utc(now);
            tick(&vatsim, &db, &recent).await;
        }
        // the reconcile would have regressed to 0, so it was deferred
        assert_eq!(stored_minutes(&db, 123, "2026-09").await, Some(120));
        assert!(recent.lock().await.pending.contains_key(&123));

        vatsim.sessions.insert(
            123,
            vec![session("DEN_CTR", "2026-09-15T10:00:00", "130.0")],
        );
        for now in ["2026-09-15T13:00:00Z", "2026-09-15T13:15:00Z"] {
            vatsim.now = utc(now);
            tick(&vatsim, &db, &recent).await;
        }
        assert_eq!(stored_minutes(&db, 123, "2026-09").await, Some(130));
        assert!(recent.lock().await.pending.is_empty());
    }

    #[tokio::test]
    async fn test_offline_reconcile_across_month_boundary() {
        let db = test_db().await;
        add_controller(&db, 123, true).await;
        set_minutes(&db, 123, "2026-09", 60).await;
        let recent = Mutex::default();
        let mut vatsim = MockVatsim::at("2026-10-01T00:30:00Z");
        vatsim.online = vec![online(123, "DEN_CTR", "2026-09-30T23:00:00Z")];
        vatsim
            .sessions
            .insert(123, vec![session("DEN_CTR", "2026-09-15T10:00:00", "60.0")]);

        tick(&vatsim, &db, &recent).await;
        // only the time since the month started lands in October for now
        assert_eq!(stored_minutes(&db, 123, "2026-10").await, Some(30));

        vatsim.online.clear();
        vatsim.sessions.get_mut(&123).unwrap().push(session(
            "DEN_CTR",
            "2026-09-30T23:00:00",
            "120.0",
        ));
        for now in [
            "2026-10-01T01:15:00Z",
            "2026-10-01T01:30:00Z",
            "2026-10-01T01:45:00Z",
        ] {
            vatsim.now = utc(now);
            tick(&vatsim, &db, &recent).await;
        }

        // the whole session counts toward September, where it started
        assert_eq!(stored_minutes(&db, 123, "2026-09").await, Some(180));
        assert_eq!(stored_minutes(&db, 123, "2026-10").await, Some(0));
        assert!(recent.lock().await.pending.is_empty());
    }

    #[tokio::test]
    async fn test_true_up_rewrites_activity() {
        let db = test_db().await;
        add_controller(&db, 123, true).await;
        set_minutes(&db, 123, "2026-01", 500).await;
        set_minutes(&db, 123, "2026-10", 999).await;
        add_adjustment(&db, 123, "2026-08", 3600).await;
        // older than the true-up window
        add_adjustment(&db, 123, "2026-03", 6000).await;
        let mut vatsim = MockVatsim::at("2026-10-03T02:00:00Z");
        vatsim.online = vec![online(123, "DEN_CTR", "2026-10-03T01:30:00Z")];
        vatsim.sessions.insert(
            123,
            vec![
                session("DEN_CTR", "2026-04-20T18:00:00", "100.0"),
                session("DEN_CTR", "2026-05-10T18:00:00", "50.0"),
                AtcSessionEntry {
                    connection_id: 42,
                    ..session("DEN_CTR", "2026-08-04T23:13:03", "19085.266667")
                },
                session("DEN_APP", "2026-09-01T18:00:00", "30.0"),
                session("DEN_CTR", "2026-09-20T18:00:00", "90.0"),
                session("ZLA_CTR", "2026-09-21T18:00:00", "500.0"),
                session("DEN_CTR", "2026-10-01T18:00:00", "45.0"),
            ],
        );

        true_up_all_controllers_activity(&vatsim, &test_config(), &db)
            .await
            .unwrap();

        assert_eq!(
            all_minutes(&db, 123).await,
            vec![
                ("2026-05".to_string(), 50),
                // just the adjustment; the bugged session is dropped
                ("2026-08".to_string(), 60),
                ("2026-09".to_string(), 120),
                // 45m session + 30m online
                ("2026-10".to_string(), 75),
            ]
        );
        let marker = sqlx::query(sql::GET_KVS_ENTRY)
            .bind("bugged_session:42")
            .fetch_optional(&db)
            .await
            .unwrap();
        assert!(marker.is_some());
    }

    #[tokio::test]
    async fn test_true_up_matches_vatsim_sessions() {
        let db = test_db().await;
        add_controller(&db, TEST_CONTROLLER, true).await;
        set_minutes(&db, TEST_CONTROLLER, "2026-09", 1792).await;
        set_minutes(&db, TEST_CONTROLLER, "2026-10", 290).await;
        let mut vatsim = MockVatsim::at("2026-10-03T02:20:00Z");
        vatsim.sessions.insert(
            TEST_CONTROLLER.into(),
            test_controller_sessions("2026-10-04"),
        );

        true_up_all_controllers_activity(&vatsim, &test_config(), &db)
            .await
            .unwrap();

        assert_eq!(
            all_minutes(&db, TEST_CONTROLLER).await,
            vec![
                // 29h58m
                ("2026-09".to_string(), 1798),
                // 13h34m
                ("2026-10".to_string(), 814),
            ]
        );
    }

    #[tokio::test]
    async fn test_true_up_skips_online_time_already_in_sessions() {
        // the feed is fetched once at the start of the sweep; by the time the
        // sweep reaches this controller, their connection has ended and
        // landed in the sessions API
        let db = test_db().await;
        add_controller(&db, 123, true).await;
        let mut vatsim = MockVatsim::at("2026-10-03T03:00:00Z");
        vatsim.online = vec![online(123, "DEN_APP", "2026-10-03T00:00:00.477022Z")];
        vatsim.sessions.insert(
            123,
            vec![
                session("DEN_CTR", "2026-10-01T03:00:00", "60.0"),
                session("DEN_APP", "2026-10-03T00:00:00", "181.0"),
            ],
        );

        true_up_all_controllers_activity(&vatsim, &test_config(), &db)
            .await
            .unwrap();

        // not another 180m online on top
        assert_eq!(stored_minutes(&db, 123, "2026-10").await, Some(241));
    }

    #[tokio::test]
    async fn test_spot_update_skips_online_time_already_in_sessions() {
        let db = test_db().await;
        add_controller(&db, 123, true).await;
        let mut vatsim = MockVatsim::at("2026-09-15T12:00:00Z");
        // still in the feed, but the connection has already ended
        vatsim.online = vec![online(123, "DEN_CTR", "2026-09-15T10:00:00.477022Z")];
        vatsim.sessions.insert(
            123,
            vec![session("DEN_CTR", "2026-09-15T10:00:00", "119.0")],
        );

        tick(&vatsim, &db, &Mutex::default()).await;

        assert_eq!(stored_minutes(&db, 123, "2026-09").await, Some(119));
    }

    #[tokio::test]
    async fn test_spot_update_counts_online_time_after_reconnect() {
        let db = test_db().await;
        add_controller(&db, 123, true).await;
        let mut vatsim = MockVatsim::at("2026-09-15T12:00:00Z");
        vatsim.online = vec![online(123, "DEN_CTR", "2026-09-15T11:00:05.123456Z")];
        // a brief connection on the same position just before reconnecting
        vatsim.sessions.insert(
            123,
            vec![session("DEN_CTR", "2026-09-15T10:59:50", "0.166667")],
        );

        tick(&vatsim, &db, &Mutex::default()).await;

        // 10s session + 59m55s online
        assert_eq!(stored_minutes(&db, 123, "2026-09").await, Some(60));
    }
}
