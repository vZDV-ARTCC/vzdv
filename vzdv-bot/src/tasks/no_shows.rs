use anyhow::Result;
use log::{debug, error, info, warn};
use sqlx::{Pool, Sqlite};
use std::{collections::HashMap, sync::Arc, time::Duration};
use tokio::time::sleep;
use twilight_http::Client;
use twilight_model::id::Id;
use vzdv::{
    config::Config,
    get_controller_cids_and_names, get_staff_member_by_role,
    sql::{self, Controller, NoShow},
};

/// Create the message to send to the Discord user.
fn create_message(
    no_show: &NoShow,
    cid_name_map: &HashMap<u32, (String, String)>,
    count: usize,
    config: &Arc<Config>,
) -> String {
    format!(
        "## Warning\n\nYou have been added to the **{} no-show list** by {} {}.\nYou have been on this list {} time(s).\n\nFor more information, reach out to the {} at `{}@{}`.\n\n\nResponses to this DM are not monitored.",
        no_show.entry_type,
        cid_name_map.get(&no_show.reported_by).unwrap().0,
        cid_name_map.get(&no_show.reported_by).unwrap().1,
        count,
        if no_show.entry_type == "training" {
            "TA"
        } else {
            "EC"
        },
        if no_show.entry_type == "training" {
            "ta"
        } else {
            "ec"
        },
        config.staff.email_domain
    )
}

/// Open a DM with the Discord user and send a message.
async fn send_dm(http: &Arc<Client>, user_id: &str, message: &str) -> Result<()> {
    let channel = http
        .create_private_channel(Id::new(user_id.parse()?))
        .await?
        .model()
        .await?;
    http.create_message(channel.id).content(message)?.await?;
    Ok(())
}

async fn handle_single(
    entry: &NoShow,
    entries: &[NoShow],
    cid_name_map: &HashMap<u32, (String, String)>,
    config: &Arc<Config>,
    db: &Pool<Sqlite>,
    http: &Arc<Client>,
) -> Result<()> {
    debug!("Need to notify {} of no-show", entry.cid);
    let controller: Controller = sqlx::query_as(sql::GET_CONTROLLER_BY_CID)
        .bind(entry.cid)
        .fetch_one(db)
        .await?;
    if let Some(ref discord_user_id) = controller.discord_id {
        let occurrence_count = entries
            .iter()
            .filter(|e| e.cid == entry.cid && e.entry_type == entry.entry_type)
            .count();
        // notify the controller
        send_dm(
            http,
            discord_user_id,
            &create_message(entry, cid_name_map, occurrence_count, config),
        )
        .await?;
        sqlx::query(sql::UPDATE_NO_SHOW_NOTIFIED)
            .bind(entry.id)
            .execute(db)
            .await?;
        // if this isn't the first instance, notify the appropriate staff member
        if occurrence_count > 1 {
            let staff_role = if entry.entry_type == "training" {
                "TA"
            } else {
                "EC"
            };
            let staff_member = get_staff_member_by_role(db, staff_role).await?;
            if let Some(staff_member) = staff_member.first() {
                if let Some(ref discord_user_id) = staff_member.discord_id {
                    send_dm(
                                http,
                                discord_user_id,
                                &format!(
                                    "Controller {} has been added to the {} no-show list {} times.\n\nSee the no-show list [here](https://{}/admin/no_show_list).",
                                    entry.cid, entry.entry_type, occurrence_count, config.staff.email_domain
                                ),
                            )
                            .await?;
                    info!(
                        "Notified {} {} ({}) of {} no-show occurrences for {}",
                        staff_member.first_name,
                        staff_member.last_name,
                        staff_member.cid,
                        occurrence_count,
                        entry.cid
                    );
                } else {
                    warn!(
                        "Staff member {} does not have their Discord linked",
                        staff_member.cid
                    );
                }
            } else {
                warn!("Could not find staff member with role {staff_role}");
            }
        }
        info!("Notified {} of their new no-show entry", entry.cid);
    } else {
        debug!(
            "Controller {} does not have their Discord linked; cannot notify",
            entry.cid
        );
    }
    Ok(())
}

/// Single loop execution.
async fn tick(config: &Arc<Config>, db: &Pool<Sqlite>, http: &Arc<Client>) -> Result<()> {
    debug!("Checking for new no-show entries");

    let entries: Vec<NoShow> = sqlx::query_as(sql::GET_ALL_NO_SHOW).fetch_all(db).await?;
    if entries.is_empty() {
        return Ok(());
    }
    let cid_name_map = get_controller_cids_and_names(db).await?;
    for entry in &entries {
        if !entry.notified
            && let Err(e) = handle_single(entry, &entries, &cid_name_map, config, db, http).await
        {
            error!("Error processing no-show tick for entry {}: {e}", entry.id);
        }
    }

    Ok(())
}

// Processing loop.
pub async fn process(config: Arc<Config>, db: Pool<Sqlite>, http: Arc<Client>) {
    sleep(Duration::from_secs(30)).await;
    debug!("Starting no-show processing");

    loop {
        if let Err(e) = tick(&config, &db, &http).await {
            error!("Error in no-show processing tick: {e}");
        }
        sleep(Duration::from_secs(60 * 10)).await; // 10 minutes
    }
}
