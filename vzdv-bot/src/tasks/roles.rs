use anyhow::Result;
use log::{debug, error, info};
use sqlx::{Pool, Sqlite};
use std::{collections::HashSet, sync::Arc, time::Duration};
use tokio::time::sleep;
use twilight_http::Client;
use twilight_model::{
    guild::Member,
    id::{Id, marker::GuildMarker},
};
use vzdv::{
    ControllerRating,
    config::Config,
    sql::{self, Certification, Controller},
};

/// Set the guild member's nickname if needed.
async fn set_nickname(
    guild_id: Id<GuildMarker>,
    member: &Member,
    controller: &Controller,
    config: &Arc<Config>,
    http: &Arc<Client>,
) -> Result<()> {
    let mut name = format!(
        "{} {}.",
        controller.first_name,
        controller.last_name.chars().next().unwrap()
    );
    if let Some(ois) = &controller.operating_initials {
        if !ois.is_empty() {
            name.push_str(" - ");
            name.push_str(ois);
        }
    }

    let is_vatusa_vatgov = member
        .roles
        .contains(&Id::new(config.discord.roles.vatusa_vatgov));
    let roles: Vec<_> = controller.roles.split_terminator(',').collect();
    if is_vatusa_vatgov {
        name.push_str(" | VATUSA");
    } else if roles.contains(&"ATM") {
        name.push_str(" | ATM");
    } else if roles.contains(&"DATM") {
        name.push_str(" | DATM");
    } else if roles.contains(&"TA") {
        name.push_str(" | TA");
    } else if roles.contains(&"EC") {
        name.push_str(" | EC");
    } else if roles.contains(&"FE") {
        name.push_str(" | FE");
    } else if roles.contains(&"WM") {
        name.push_str(" | WM");
    } else if roles.contains(&"ATA") {
        name.push_str(" | ATA");
    } else if roles.contains(&"INS") {
        name.push_str(" | INS")
    } else if roles.contains(&"MTR") {
        name.push_str(" | MTR")
    } else if roles.contains(&"AEC") {
        name.push_str(" | AEC");
    } else if roles.contains(&"AFE") {
        name.push_str(" | AFE");
    } else if roles.contains(&"AWM") {
        name.push_str(" | AWM");
    }

    if member.nick.as_ref() == Some(&name) {
        // don't make the HTTP call if no change is needed
        return Ok(());
    }
    if member.nick.is_some() {
        info!("Updating nick of {} to {name}", member.user.id);
    } else {
        info!("Setting nick of {} to {name}", member.user.id);
    }
    let result = http
        .update_guild_member(guild_id, member.user.id)
        .nick(Some(&name))?
        .await;
    if let Err(e) = result {
        if matches!(e.kind(), twilight_http::error::ErrorType::Response { status, .. } if status.get() == 403 )
        {
            debug!(
                "Could not set nick of {} - insufficient permissions",
                member.user.id
            );
            return Ok(());
        }
        return Err(e.into());
    }

    Ok(())
}

/// Resolve the guild member's roles, adding and removing as necessary.
async fn resolve_roles(
    guild_id: Id<GuildMarker>,
    member: &Member,
    roles: &[(u64, bool)],
    http: &Arc<Client>,
) -> Result<()> {
    let existing: Vec<_> = member.roles.iter().map(|r| r.get()).collect();
    for &(id, should_have) in roles {
        if should_have && !existing.contains(&id) {
            info!(
                "Adding role {id} to {} ({})",
                member.nick.as_ref().unwrap_or(&member.user.name),
                member.user.id.get()
            );
            http.add_guild_member_role(guild_id, member.user.id, Id::new(id))
                .await?;
        } else if !should_have && existing.contains(&id) {
            info!(
                "Removing role {id} from {} ({})",
                member.nick.as_ref().unwrap_or(&member.user.name),
                member.user.id.get()
            );
            http.remove_guild_member_role(guild_id, member.user.id, Id::new(id))
                .await?;
        }
    }
    Ok(())
}

/// Determine which roles the guild member should have.
async fn get_correct_roles(
    config: &Arc<Config>,
    controller: &Option<Controller>,
    certifications: &HashSet<String>,
) -> Result<Vec<(u64, bool)>> {
    let mut to_resolve = Vec::with_capacity(27);

    let home_facility = controller
        .as_ref()
        .map(|c| c.home_facility.as_str())
        .unwrap_or_default();
    let is_on_roster = controller
        .as_ref()
        .map(|c| c.is_on_roster)
        .unwrap_or_default();
    let rating = controller.as_ref().map(|c| c.rating).unwrap_or_default();
    let roles: Vec<_> = controller
        .as_ref()
        .map(|c| c.roles.split_terminator(',').collect())
        .unwrap_or_default();

    // membership
    to_resolve.push((config.discord.roles.home_controller, home_facility == "ZDV"));
    to_resolve.push((
        config.discord.roles.visiting_controller,
        is_on_roster && home_facility != "ZDV",
    ));
    to_resolve.push((config.discord.roles.guest, !is_on_roster));

    // network rating
    to_resolve.push((
        config.discord.roles.administrator,
        rating == ControllerRating::ADM.as_id(),
    ));
    to_resolve.push((
        config.discord.roles.supervisor,
        rating == ControllerRating::SUP.as_id(),
    ));
    to_resolve.push((
        config.discord.roles.instructor_3,
        rating == ControllerRating::I3.as_id(),
    ));
    to_resolve.push((
        config.discord.roles.instructor_1,
        rating == ControllerRating::I1.as_id(),
    ));
    to_resolve.push((
        config.discord.roles.controller_3,
        rating == ControllerRating::C3.as_id(),
    ));
    to_resolve.push((
        config.discord.roles.controller_1,
        rating == ControllerRating::C1.as_id(),
    ));
    to_resolve.push((
        config.discord.roles.student_3,
        rating == ControllerRating::S3.as_id(),
    ));
    to_resolve.push((
        config.discord.roles.student_2,
        rating == ControllerRating::S2.as_id(),
    ));
    to_resolve.push((
        config.discord.roles.student_1,
        rating == ControllerRating::S1.as_id(),
    ));
    to_resolve.push((
        config.discord.roles.observer,
        rating == ControllerRating::OBS.as_id(),
    ));

    // certs
    to_resolve.push((
        config.discord.roles.t1_app,
        certifications.contains("APP T1"),
    ));
    to_resolve.push((
        config.discord.roles.t2_ctr,
        certifications.contains("ENR T2"),
    ));
    to_resolve.push((
        config.discord.roles.t1_twr,
        certifications.contains("LC T1"),
    ));
    to_resolve.push((
        config.discord.roles.t1_gnd,
        certifications.contains("GC T1"),
    ));

    // staff
    if ["ATM", "DATM", "TA"]
        .iter()
        .any(|role| roles.contains(role))
    {
        to_resolve.push((config.discord.roles.sr_staff, true));
        to_resolve.push((config.discord.roles.jr_staff, false));
    } else if ["EC", "FE", "WM", "ATA"]
        .iter()
        .any(|role| roles.contains(role))
    {
        to_resolve.push((config.discord.roles.sr_staff, false));
        to_resolve.push((config.discord.roles.jr_staff, true));
    } else {
        to_resolve.push((config.discord.roles.sr_staff, false));
        to_resolve.push((config.discord.roles.jr_staff, false));
    }

    // staff teams
    let training_team = ["TA", "MTR", "INS"].iter().any(|role| roles.contains(role));
    to_resolve.push((config.discord.roles.training_staff, training_team));
    let event_team = ["EC", "AEC"].iter().any(|role| roles.contains(role));
    to_resolve.push((config.discord.roles.event_team, event_team));
    let fe_team = ["FE", "AFE"].iter().any(|role| roles.contains(role));
    to_resolve.push((config.discord.roles.fe_team, fe_team));
    let web_team = ["WM", "AWM"].iter().any(|role| roles.contains(role));
    to_resolve.push((config.discord.roles.web_team, web_team));

    Ok(to_resolve)
}

/// Handle everything for a single guild member.
///
/// Returns a `bool` of if a controller was located by Discord user ID.
pub async fn process_single_member(
    member: &Member,
    guild_id: Id<GuildMarker>,
    config: &Arc<Config>,
    db: &Pool<Sqlite>,
    http: &Arc<Client>,
) -> bool {
    let nick = member.nick.as_ref().unwrap_or(&member.user.name);
    let user_id = member.user.id.get();

    if user_id == config.discord.owner_id {
        debug!("Skipping over guild owner {nick} ({user_id})");
        return false;
    }
    if member.user.bot {
        debug!("Skipping over bot user {nick} ({user_id})");
        return false;
    }
    debug!("Processing user {nick} ({user_id})");
    let controller: Option<Controller> = match sqlx::query_as(sql::GET_CONTROLLER_BY_DISCORD_ID)
        .bind(user_id.to_string())
        .fetch_optional(db)
        .await
    {
        Ok(c) => c,
        Err(e) => {
            error!("Error getting controller by Discord ID: {e}");
            return false;
        }
    };

    let cert_names: HashSet<_> = if let Some(ref c) = controller {
        let certifications: Vec<Certification> =
            match sqlx::query_as(sql::GET_ALL_CERTIFICATIONS_FOR)
                .bind(c.cid)
                .fetch_all(db)
                .await
            {
                Ok(e) => e,
                Err(e) => {
                    error!("Error getting certs for CID {}: {e}", c.id);
                    Vec::new()
                }
            };
        certifications
            .iter()
            .filter(|c| c.value == "certified")
            .map(|c| c.name.clone())
            .collect()
    } else {
        HashSet::new()
    };
    // determine the roles the guild member should have and update accordingly
    match get_correct_roles(config, &controller, &cert_names).await {
        Ok(to_resolve) => {
            if let Err(e) = resolve_roles(guild_id, member, &to_resolve, http).await {
                error!("Error resolving roles for {nick} ({user_id}): {e}");
            }
        }
        Err(e) => {
            error!("Error determining roles for {nick} ({user_id}): {e}");
        }
    }

    // nickname
    if let Some(controller) = controller {
        if member
            .roles
            .iter()
            .any(|r| r.get() == config.discord.roles.ignore)
        {
            debug!("{nick} ({user_id}) has bot ignore role; not setting nickname");
        } else if let Err(e) = set_nickname(guild_id, member, &controller, config, http).await {
            error!("Error setting nickname of {nick} ({user_id}): {e}");
        }
    }
    true
}

/// Single loop execution.
async fn tick(config: &Arc<Config>, db: &Pool<Sqlite>, http: &Arc<Client>) -> Result<()> {
    info!("Role tick");
    let guild_id = Id::new(config.discord.guild_id);
    let members = http
        .guild_members(guild_id)
        .limit(1_000)?
        .await?
        .model()
        .await?;
    debug!("Found {} Discord members", members.len());
    for member in &members {
        process_single_member(member, guild_id, config, db, http).await;
        // short wait between members
        sleep(Duration::from_secs(1)).await;
    }
    debug!("Roles tick complete");

    Ok(())
}

// Processing loop.
pub async fn process(config: Arc<Config>, db: Pool<Sqlite>, http: Arc<Client>) {
    sleep(Duration::from_secs(30)).await;
    debug!("Starting roles processing");

    loop {
        if let Err(e) = tick(&config, &db, &http).await {
            error!("Error in roles processing tick: {e}");
        }
        sleep(Duration::from_secs(60 * 10)).await; // 10 minutes
    }
}
