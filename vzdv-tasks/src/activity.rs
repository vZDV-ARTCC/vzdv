//! Update activity from VATSIM.

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Months, Utc};
use log::{debug, error, warn};
use sqlx::{Pool, Row, Sqlite};
use std::{collections::HashMap, time::Duration};
use tokio::time;
use vatsim_utils::errors::VatsimUtilError;
use vatsim_utils::rest_api;
use vzdv::{
    config::Config,
    position_in_facility_airspace,
    sql::{self, ControllerActivityManualAdjustment},
};

/// Update the activity for a single controller, looking back several
/// months and replacing the DB records with new data from VATSIM.
async fn true_up_single_activity(
    config: &Config,
    db: &Pool<Sqlite>,
    five_months_ago: &str,
    cid: u32,
) -> Result<()> {
    /*
     * Get the last 5 months of the controller's activity.
     *
     * I'm not (currently) worried about pagination as even the facility's most
     * active controllers don't have enough sessions in this time range to go over
     * the endpoint's single-page response limit.
     */
    let sessions_res =
        rest_api::get_atc_sessions(cid as u64, None, None, Some(five_months_ago), None).await;
    let sessions = match sessions_res {
        Ok(data) => data,
        Err(VatsimUtilError::InvalidStatusCode(code)) => {
            warn!("Getting rate limited on activity API; waiting 30 seconds before continuing");
            time::sleep(Duration::from_secs(30)).await;
            bail!("getting activity for {cid}; got HTTP response {code}")
        }
        Err(VatsimUtilError::FailedJsonParse(e)) => bail!("failed parsing activity for {cid}: {e}"),
        Err(e) => bail!("getting activity for {cid}: {e}"),
    };

    // group the controller's activity by month
    let mut seconds_map: HashMap<String, f32> = HashMap::new();
    for session in sessions.results {
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

    for row in manual_adjustments {
        seconds_map
            .entry(row.month)
            .and_modify(|acc| *acc += row.seconds as f32)
            .or_insert(row.seconds as f32);
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
pub async fn true_up_all_controllers_activity(config: &Config, db: &Pool<Sqlite>) -> Result<()> {
    // prep cids for on-roster controllers and a 5-month-ago timestamp that the API recognizes
    let controllers = sqlx::query(sql::GET_ALL_ROSTER_CONTROLLER_CIDS)
        .fetch_all(db)
        .await?;
    let five_months_ago = chrono::Utc::now()
        .checked_sub_months(Months::new(5))
        .unwrap()
        .format("%Y-%m-%d")
        .to_string();
    for row in controllers {
        let cid: u32 = row.try_get("cid").expect("no 'cid' column");
        debug!("Getting activity for {cid}");
        if let Err(e) = true_up_single_activity(config, db, &five_months_ago, cid).await {
            error!("Error updating activity for {cid}: {e}");
        }
        // wait 8 seconds to adhere to the VATSIM API rate limits
        time::sleep(Duration::from_secs(8)).await;
    }
    Ok(())
}

/// Updates a single controller's activity just this month.
async fn update_single_activity(
    config: &Config,
    db: &Pool<Sqlite>,
    start_of_month: &str,
    cid: u64,
    logon_time: &str,
) -> Result<()> {
    let sessions = rest_api::get_atc_sessions(cid, None, None, Some(start_of_month), None).await?;
    let mut counter = 0.0;
    for session in sessions.results {
        if !position_in_facility_airspace(config, &session.callsign) {
            continue;
        }
        counter += session.minutes_on_callsign.parse::<f32>().unwrap() * 60.0;
    }
    let logon_time = DateTime::parse_from_rfc3339(logon_time)?.timestamp();
    let online_seconds = Utc::now().timestamp() - logon_time;
    let minutes = ((counter + online_seconds as f32) / 60.0).round() as u32;

    // update the controller's time for this month (if able)
    let result = sqlx::query(sql::UPDATE_ACTIVITY)
        .bind(cid as u32)
        .bind(start_of_month[0..7].to_string())
        .bind(minutes)
        .execute(db)
        .await
        .with_context(|| format!("Updating CID {cid}"))?;
    if result.rows_affected() == 0 {
        // The controller hasn't yet completed a full session for this month,
        // so no rows were updated. Insert a new row.
        sqlx::query(sql::INSERT_INTO_ACTIVITY)
            .bind(cid as u32)
            .bind(start_of_month[0..7].to_string())
            .bind(minutes)
            .execute(db)
            .await
            .with_context(|| format!("Inserting new activity row for spot-update for {cid}"))?;
    }

    Ok(())
}

/// Update this month's activity for currently online controllers.
pub async fn update_online_controller_activity(config: &Config, db: &Pool<Sqlite>) -> Result<()> {
    let on_roster_cids: Vec<u64> = {
        let rows = sqlx::query(sql::GET_ALL_ROSTER_CONTROLLER_CIDS)
            .fetch_all(db)
            .await?;
        rows.iter()
            .map(|row| row.try_get("cid").expect("no 'cid' column"))
            .collect()
    };
    let online_controllers = {
        let vatsim = vatsim_utils::live_api::Vatsim::new().await?;
        vatsim.get_v3_data().await?.controllers
    };
    let start_of_month = chrono::Utc::now().format("%Y-%m-01").to_string();

    for controller in online_controllers {
        let cid = controller.cid;
        if !on_roster_cids.contains(&cid) {
            continue;
        }
        if !position_in_facility_airspace(config, &controller.callsign) {
            // ignore controlling in other facilities and observers
            continue;
        }
        debug!("Spot-updating activity for {cid}");
        if let Err(e) =
            update_single_activity(config, db, &start_of_month, cid, &controller.logon_time).await
        {
            error!("Error spot-updating CID {cid}: {e}")
        }
        // wait 8 seconds to adhere to the VATSIM API rate limits
        time::sleep(Duration::from_secs(8)).await;
    }

    Ok(())
}
