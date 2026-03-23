//! HTTP endpoints for controller pages.

use crate::{
    flashed_messages::{MessageLevel, drain_flashed_messages, push_flashed_message},
    shared::{
        AppError, AppState, SESSION_USER_INFO_KEY, UserInfo, js_timestamp_to_utc, post_audit,
        record_log, reject_if_not_in, remove_controller_from_roster, strip_some_tags,
    },
    vatusa::{
        self, NewTrainingRecord, TrainingDataType, TrainingRecord, get_training_records,
        save_training_record,
    },
};
use axum::{
    Form, Router,
    extract::{Path, State},
    response::{Html, IntoResponse, Redirect, Response},
    routing::{delete, get, post},
};
use chrono::{DateTime, Datelike, Utc};
use itertools::Itertools;
use log::{debug, error, info, warn};
use minijinja::context;
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use sqlx::{Pool, Sqlite};
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};
use tower_sessions::Session;
use uuid::Uuid;
use vzdv::{
    ControllerRating, PermissionsGroup, StaffPosition, controller_can_see,
    email::send_mail_raw,
    get_controller_cids_and_names, retrieve_all_in_use_ois,
    sql::{self, AuxiliaryTrainingData, Certification, Controller, Feedback, SoloCert, StaffNote},
    vatsim,
    vatusa::get_multiple_controller_names,
};

/// Roles the current user is able to set.
async fn roles_to_set(
    db: &Pool<Sqlite>,
    user_info: &Option<UserInfo>,
) -> Result<HashSet<String>, AppError> {
    let controller: Option<Controller> = match user_info {
        Some(ui) => {
            sqlx::query_as(sql::GET_CONTROLLER_BY_CID)
                .bind(ui.cid)
                .fetch_optional(db)
                .await?
        }
        None => None,
    };
    let mut roles_to_set = Vec::new();
    let user_roles: Vec<_> = match &controller {
        Some(c) => c.roles.split_terminator(',').collect(),
        None => {
            return Ok(HashSet::new());
        }
    };
    if user_roles.contains(&"FE") {
        roles_to_set.push(StaffPosition::AFE);
    } else if user_roles.contains(&"EC") {
        roles_to_set.push(StaffPosition::AEC);
    } else if user_roles.contains(&"TA") {
        roles_to_set.push(StaffPosition::MTR);
        roles_to_set.push(StaffPosition::ATA);
    } else if controller_can_see(&controller, PermissionsGroup::Admin) {
        roles_to_set.push(StaffPosition::ATM);
        roles_to_set.push(StaffPosition::DATM);
        roles_to_set.push(StaffPosition::TA);
        roles_to_set.push(StaffPosition::FE);
        roles_to_set.push(StaffPosition::EC);
        roles_to_set.push(StaffPosition::WM);
        roles_to_set.push(StaffPosition::ATA);
        roles_to_set.push(StaffPosition::AFE);
        roles_to_set.push(StaffPosition::AEC);
        roles_to_set.push(StaffPosition::AWM);
        roles_to_set.push(StaffPosition::INS);
        roles_to_set.push(StaffPosition::MTR);
    }

    Ok(roles_to_set
        .iter()
        .map(|position| position.as_str().to_owned())
        .collect::<HashSet<String>>())
}

/// Permissions that a user might have.
#[derive(Debug, Serialize)]
struct TrainingPermissions {
    /// Is a site admin (Sr Staff & WM)
    is_admin: bool,
    /// Admin + TA + Instructors (not Mentors)
    can_grant_rating_solos: bool,
    /// Any training staff
    can_grant_cert_solos: bool,
}

/// Check various training permissions of the current user.
async fn user_is_training_special(
    user_info: &Option<UserInfo>,
    db: &Pool<Sqlite>,
) -> Result<TrainingPermissions, AppError> {
    let controller: Option<Controller> = sqlx::query_as(sql::GET_CONTROLLER_BY_CID)
        .bind(user_info.as_ref().map(|ui| ui.cid).unwrap_or_default())
        .fetch_optional(db)
        .await?;
    let is_admin = controller_can_see(&controller, PermissionsGroup::Admin);
    let roles: Vec<_> = controller
        .as_ref()
        .map(|c| c.roles.split(',').collect())
        .unwrap_or_default();
    let is_at_least_s3 = controller
        .as_ref()
        .map(|c| c.rating)
        .unwrap_or(ControllerRating::OBS.as_id())
        >= ControllerRating::S3.as_id();
    Ok(TrainingPermissions {
        is_admin,
        can_grant_rating_solos: is_admin
            || roles.contains(&"TA")
            || roles.contains(&"INS")
            || (roles.contains(&"MTR") && is_at_least_s3),
        can_grant_cert_solos: is_admin
            || roles.contains(&"TA")
            || roles.contains(&"INS")
            || roles.contains(&"MTR"),
    })
}

/// Overview page for a user.
///
/// Shows additional information and controls for different staff
/// members (some, training, admin).
async fn page_controller(
    State(state): State<Arc<AppState>>,
    session: Session,
    Path(cid): Path<u32>,
) -> Result<Response, AppError> {
    #[derive(Serialize)]
    struct CertNameValue<'a> {
        name: &'a str,
        value: &'a str,
    }

    #[derive(Serialize)]
    struct StaffNoteDisplay {
        id: u32,
        by: String,
        by_cid: u32,
        date: DateTime<Utc>,
        comment: String,
    }

    let user_info: Option<UserInfo> = session.get(SESSION_USER_INFO_KEY).await?;
    let controller: Option<Controller> = sqlx::query_as(sql::GET_CONTROLLER_BY_CID)
        .bind(cid)
        .fetch_optional(&state.db)
        .await?;
    let controller = match controller {
        Some(c) => c,
        None => {
            push_flashed_message(session, MessageLevel::Error, "Controller not found").await?;
            return Ok(Redirect::to("/facility/roster").into_response());
        }
    };
    let rating_str = ControllerRating::try_from(controller.rating)
        .map_err(|err| AppError::GenericFallback("parsing unknown controller rating", err))?
        .as_str();

    let db_certs: Vec<Certification> = sqlx::query_as(sql::GET_ALL_CERTIFICATIONS_FOR)
        .bind(cid)
        .fetch_all(&state.db)
        .await?;
    let mut certifications: Vec<CertNameValue> =
        Vec::with_capacity(state.config.training.certifications.len());
    let none = String::from("None");
    for name in &state.config.training.certifications {
        let db_match = db_certs.iter().find(|cert| &cert.name == name);
        let value: &str = match db_match {
            Some(row) => &row.value,
            None => &none,
        };
        certifications.push(CertNameValue { name, value });
    }

    let roles: Vec<_> = controller.roles.split_terminator(',').collect();
    let training_perms = user_is_training_special(&user_info, &state.db).await?;

    let viewing_themselves = user_info.as_ref().map(|info| info.cid).unwrap_or_default() == cid;
    let feedback: Vec<Feedback> = if training_perms.is_admin {
        sqlx::query_as(sql::GET_ALL_FEEDBACK_FOR)
            .bind(cid)
            .fetch_all(&state.db)
            .await?
    } else if viewing_themselves {
        sqlx::query_as(sql::GET_APPROVED_FEEDBACK_FOR)
            .bind(cid)
            .fetch_all(&state.db)
            .await?
    } else {
        Vec::new()
    };

    let all_controllers = get_controller_cids_and_names(&state.db)
        .await
        .map_err(|e| AppError::GenericFallback("getting names and CIDs from DB", e))?;
    let staff_notes: Vec<StaffNoteDisplay> = if training_perms.is_admin {
        let notes: Vec<StaffNote> = sqlx::query_as(sql::GET_STAFF_NOTES_FOR)
            .bind(cid)
            .fetch_all(&state.db)
            .await?;
        notes
            .iter()
            .map(|note| StaffNoteDisplay {
                id: note.id,
                by: all_controllers
                    .iter()
                    .find(|c| *c.0 == note.by)
                    .map(|c| format!("{} {} ({})", c.1.0, c.1.1, c.0))
                    .unwrap_or_else(|| format!("{}?", note.cid)),
                by_cid: note.by,
                date: note.date,
                comment: note.comment.clone(),
            })
            .collect()
    } else {
        Vec::new()
    };
    let settable_roles_set = roles_to_set(&state.db, &user_info).await?;
    let mut settable_roles: Vec<_> = settable_roles_set.iter().collect();
    settable_roles.sort();

    let solo_certs: Vec<SoloCert> = sqlx::query_as(sql::GET_ALL_SOLO_CERTS_FOR)
        .bind(cid)
        .fetch_all(&state.db)
        .await?;

    let flashed_messages = drain_flashed_messages(session).await?;
    let template = state
        .templates
        .get_template("controller/controller.jinja")?;
    let rendered: String = template.render(context! {
        user_info,
        controller,
        roles,
        rating_str,
        certifications,
        settable_roles,
        feedback,
        staff_notes,
        training_perms,
        solo_certs,
        all_controllers,
        flashed_messages
    })?;
    Ok(Html(rendered).into_response())
}

/// API endpoint to unlink a controller's Discord account.
///
/// For admin staff members.
async fn api_unlink_discord(
    State(state): State<Arc<AppState>>,
    session: Session,
    Path(cid): Path<u32>,
) -> Result<Redirect, AppError> {
    let user_info: Option<UserInfo> = session.get(SESSION_USER_INFO_KEY).await?;
    if let Some(redirect) = reject_if_not_in(&state, &user_info, PermissionsGroup::Admin).await {
        return Ok(redirect);
    }
    sqlx::query(sql::UNSET_CONTROLLER_DISCORD_ID)
        .bind(cid)
        .execute(&state.db)
        .await?;
    push_flashed_message(session, MessageLevel::Info, "Discord unlinked").await?;
    record_log(
        format!(
            "{} unlinked Discord account from {cid}",
            user_info.unwrap().cid
        ),
        &state.db,
        true,
    )
    .await?;
    Ok(Redirect::to(&format!("/controllers/{cid}")))
}

#[derive(Deserialize)]
struct ChangeInitialsForm {
    initials: String,
}

/// Form submission to set a controller's operating initials.
///
/// For admin staff members.
async fn post_change_ois(
    State(state): State<Arc<AppState>>,
    session: Session,
    Path(cid): Path<u32>,
    Form(initials_form): Form<ChangeInitialsForm>,
) -> Result<Redirect, AppError> {
    let user_info: Option<UserInfo> = session.get(SESSION_USER_INFO_KEY).await?;
    if let Some(redirect) = reject_if_not_in(&state, &user_info, PermissionsGroup::Admin).await {
        return Ok(redirect);
    }
    let initials = initials_form.initials.to_uppercase();

    // assert unique
    if !initials.is_empty() {
        let in_use = retrieve_all_in_use_ois(&state.db)
            .await
            .map_err(|err| AppError::GenericFallback("accessing DB to get existing OIs", err))?;
        if in_use.contains(&initials) {
            push_flashed_message(session, MessageLevel::Error, "Those OIs are already in use")
                .await?;
            return Ok(Redirect::to(&format!("/controller/{cid}")));
        }
    }

    // update
    sqlx::query(sql::UPDATE_CONTROLLER_OIS)
        .bind(cid)
        .bind(&initials)
        .execute(&state.db)
        .await?;

    push_flashed_message(session, MessageLevel::Info, "Operating initials updated").await?;
    record_log(
        format!(
            "{} updated OIs for {cid} to: '{initials}'",
            user_info.unwrap().cid,
        ),
        &state.db,
        true,
    )
    .await?;
    Ok(Redirect::to(&format!("/controller/{cid}")))
}

/// Form submission to set the controller's certifications.
///
/// Not used to set their network rating; that process is handled
/// through VATUSA/VATSIM. Also does not handle communicating solo
/// certs to any other site.
///
/// For training staff members.
async fn post_change_certs(
    State(state): State<Arc<AppState>>,
    session: Session,
    Path(cid): Path<u32>,
    Form(certs_form): Form<HashMap<String, String>>,
) -> Result<Redirect, AppError> {
    let user_info: Option<UserInfo> = session.get(SESSION_USER_INFO_KEY).await?;
    if let Some(redirect) =
        reject_if_not_in(&state, &user_info, PermissionsGroup::TrainingTeam).await
    {
        return Ok(redirect);
    }

    let by_cid = user_info.unwrap().cid;
    let db_certs: Vec<Certification> = sqlx::query_as(sql::GET_ALL_CERTIFICATIONS_FOR)
        .bind(cid)
        .fetch_all(&state.db)
        .await?;
    for (key, value) in &certs_form {
        let existing = db_certs.iter().find(|c| &c.name == key);
        match existing {
            Some(existing) => {
                sqlx::query(sql::UPDATE_CERTIFICATION)
                    .bind(existing.id)
                    .bind(value)
                    .bind(Utc::now())
                    .bind(by_cid)
                    .execute(&state.db)
                    .await?;
                info!("{by_cid} updated cert for {cid} of {key} -> {value}");
            }
            None => {
                sqlx::query(sql::CREATE_CERTIFICATION)
                    .bind(cid)
                    .bind(key)
                    .bind(value)
                    .bind(Utc::now())
                    .bind(by_cid)
                    .execute(&state.db)
                    .await?;
                info!("{by_cid} created new cert for {cid} of {key} -> {value}");
            }
        }
    }
    record_log(
        format!("{by_cid} updated certs for {cid}"),
        &state.db,
        false,
    )
    .await?;

    push_flashed_message(session, MessageLevel::Info, "Updated certifications").await?;
    Ok(Redirect::to(&format!("/controller/{cid}")))
}

#[derive(Debug, Deserialize)]
struct NewSoloCertForm {
    position: String,
    report: Option<String>,
}

/// Post a new solo cert for the controller.
///
/// For training staff members, but not Mentors.
async fn post_new_solo_cert(
    State(state): State<Arc<AppState>>,
    session: Session,
    Path(cid): Path<u32>,
    Form(new_solo_form): Form<NewSoloCertForm>,
) -> Result<Redirect, AppError> {
    let user_info: Option<UserInfo> = session.get(SESSION_USER_INFO_KEY).await?;
    let training_perms = user_is_training_special(&user_info, &state.db).await?;
    if !training_perms.can_grant_cert_solos {
        push_flashed_message(
            session,
            MessageLevel::Error,
            "Issuance of solo certs is for Instructors",
        )
        .await?;
        return Ok(Redirect::to(&format!("/controller/{cid}")));
    }
    let user_info: UserInfo = user_info.unwrap();

    let controller: Option<Controller> = sqlx::query_as(sql::GET_CONTROLLER_BY_CID)
        .bind(cid)
        .fetch_optional(&state.db)
        .await?;
    if controller.is_none() {
        push_flashed_message(session, MessageLevel::Error, "Controller not found").await?;
        return Ok(Redirect::to("/facility/roster"));
    }

    let now = Utc::now();
    let expiration = now.checked_add_days(chrono::Days::new(45)).unwrap();
    let position = new_solo_form.position.to_uppercase();
    sqlx::query(sql::CREATE_SOLO_CERT)
        .bind(cid)
        .bind(user_info.cid)
        .bind(&position)
        .bind(new_solo_form.report.is_some())
        .bind(now)
        .bind(expiration)
        .execute(&state.db)
        .await?;

    // only permit VATUSA reporting for the training staff who can do so (INS+ & S3+ MTRs)
    if new_solo_form.report.is_some() && training_perms.can_grant_rating_solos {
        debug!("Reporting new solo cert to VATUSA");
        vatusa::report_solo_cert(
            cid,
            &position,
            expiration,
            &state.config.vatsim.vatusa_api_key,
        )
        .await?;
    }

    push_flashed_message(session, MessageLevel::Info, "New solo cert issued").await?;
    record_log(
        format!(
            "{} added solo cert of {} to {}",
            user_info.cid, position, cid
        ),
        &state.db,
        true,
    )
    .await?;
    Ok(Redirect::to(&format!("/controller/{cid}")))
}

/// API endpoint for deleting a solo cert.
///
/// For training staff members, but not Mentors.
async fn api_delete_solo_cert(
    State(state): State<Arc<AppState>>,
    session: Session,
    Path((cid, cert_id)): Path<(u32, u32)>,
) -> Result<StatusCode, AppError> {
    let user_info: Option<UserInfo> = session.get(SESSION_USER_INFO_KEY).await?;
    let training_perms = user_is_training_special(&user_info, &state.db).await?;
    if !training_perms.can_grant_cert_solos {
        return Ok(StatusCode::FORBIDDEN);
    }
    let user_info: UserInfo = user_info.unwrap();

    let controller: Option<Controller> = sqlx::query_as(sql::GET_CONTROLLER_BY_CID)
        .bind(cid)
        .fetch_optional(&state.db)
        .await?;
    if controller.is_none() {
        return Ok(StatusCode::NOT_FOUND);
    }
    let solo_certs: Vec<SoloCert> = sqlx::query_as(sql::GET_ALL_SOLO_CERTS_FOR)
        .bind(cid)
        .fetch_all(&state.db)
        .await?;
    let matching = match solo_certs.iter().find(|c| c.id == cert_id) {
        Some(m) => m,
        None => {
            return Ok(StatusCode::NOT_FOUND);
        }
    };

    sqlx::query(sql::DELETE_SOLO_CERT)
        .bind(matching.id)
        .execute(&state.db)
        .await?;
    // only report to VATUSA if this was reported originally (and this user has permission to do so)
    if matching.reported && training_perms.can_grant_rating_solos {
        debug!("Deleting solo cert from VATUSA");
        if let Err(e) =
            vatusa::delete_solo_cert(cid, &matching.position, &state.config.vatsim.vatusa_api_key)
                .await
        {
            warn!(
                "Could not delete solo cert for {} on {} from VATUSA: {e}",
                cid, &matching.position,
            );
        }
    }

    push_flashed_message(session, MessageLevel::Info, "Solo cert deleted").await?;
    record_log(
        format!(
            "{} deleted solo cert {} for {} of {}",
            user_info.cid, matching.id, matching.cid, matching.position
        ),
        &state.db,
        true,
    )
    .await?;

    Ok(StatusCode::OK)
}

#[derive(Deserialize)]
struct SoloCertEditForm {
    solo_cert_id: u32,
    expiration: String,
}

/// Form submission to change the date a solo cert expires.
async fn post_edit_solo_cert(
    State(state): State<Arc<AppState>>,
    session: Session,
    Path(cid): Path<u32>,
    Form(edit_form): Form<SoloCertEditForm>,
) -> Result<Redirect, AppError> {
    let user_info: Option<UserInfo> = session.get(SESSION_USER_INFO_KEY).await?;
    let training_perms = user_is_training_special(&user_info, &state.db).await?;
    if !training_perms.can_grant_cert_solos {
        return Ok(Redirect::to(&format!("/controller/{cid}")));
    }
    let user_info: UserInfo = user_info.unwrap();

    let controller: Option<Controller> = sqlx::query_as(sql::GET_CONTROLLER_BY_CID)
        .bind(cid)
        .fetch_optional(&state.db)
        .await?;
    if controller.is_none() {
        push_flashed_message(session, MessageLevel::Error, "Unknown controller").await?;
        return Ok(Redirect::to(&format!("/controller/{cid}")));
    }
    let solo_certs: Vec<SoloCert> = sqlx::query_as(sql::GET_ALL_SOLO_CERTS_FOR)
        .bind(cid)
        .fetch_all(&state.db)
        .await?;
    let matching = match solo_certs.iter().find(|c| c.id == edit_form.solo_cert_id) {
        Some(m) => m,
        None => {
            push_flashed_message(session, MessageLevel::Error, "Unknown solo cert id").await?;
            return Ok(Redirect::to(&format!("/controller/{cid}")));
        }
    };

    // create the new expiration date, starting from now and updating with data from the form
    let expiration_date = {
        let mut parts = edit_form.expiration.split('-');
        let mut d = Utc::now();
        d = d.with_year(parts.next().unwrap().parse().unwrap()).unwrap();
        d = d
            .with_month(parts.next().unwrap().parse().unwrap())
            .unwrap();
        d = d.with_day(parts.next().unwrap().parse().unwrap()).unwrap();
        d
    };

    record_log(
        format!(
            "{} updated solo cert {} for {cid} to {expiration_date}",
            user_info.cid, matching.id,
        ),
        &state.db,
        true,
    )
    .await?;

    // update DB
    sqlx::query(sql::UPDATE_SOLO_CERT_EXPIRATION)
        .bind(matching.id)
        .bind(expiration_date)
        .execute(&state.db)
        .await?;

    // report to VATUSA if needed
    if matching.reported {
        debug!("Updating VATUSA of the extended expiration");
        vatusa::delete_solo_cert(cid, &matching.position, &state.config.vatsim.vatusa_api_key)
            .await?;
        vatusa::report_solo_cert(
            cid,
            &matching.position,
            expiration_date,
            &state.config.vatsim.vatusa_api_key,
        )
        .await?;
    }

    push_flashed_message(
        session,
        MessageLevel::Success,
        "Solo cert expiration date modified",
    )
    .await?;

    Ok(Redirect::to(&format!("/controller/{cid}")))
}

#[derive(Deserialize)]
struct NewNoteForm {
    note: String,
}

/// Post a new staff note to the controller.
///
/// For staff members.
async fn post_new_staff_note(
    State(state): State<Arc<AppState>>,
    session: Session,
    Path(cid): Path<u32>,
    Form(note_form): Form<NewNoteForm>,
) -> Result<Redirect, AppError> {
    let user_info: Option<UserInfo> = session.get(SESSION_USER_INFO_KEY).await?;
    if let Some(redirect) = reject_if_not_in(&state, &user_info, PermissionsGroup::SomeStaff).await
    {
        return Ok(redirect);
    }
    let user_info = user_info.unwrap();
    record_log(
        format!("{} added staff note to {cid}", user_info.cid),
        &state.db,
        true,
    )
    .await?;
    sqlx::query(sql::CREATE_STAFF_NOTE)
        .bind(cid)
        .bind(user_info.cid)
        .bind(Utc::now())
        .bind(note_form.note)
        .execute(&state.db)
        .await?;
    push_flashed_message(session, MessageLevel::Info, "Message saved").await?;
    Ok(Redirect::to(&format!("/controller/{cid}")))
}

/// Delete a staff note. The user performing the deletion must be the user who left the note.
///
/// For staff members.
async fn api_delete_staff_note(
    State(state): State<Arc<AppState>>,
    session: Session,
    Path((_cid, note_id)): Path<(u32, u32)>,
) -> Result<StatusCode, AppError> {
    let user_info: Option<UserInfo> = session.get(SESSION_USER_INFO_KEY).await?;
    if reject_if_not_in(&state, &user_info, PermissionsGroup::SomeStaff)
        .await
        .is_some()
    {
        return Ok(StatusCode::FORBIDDEN);
    }
    let user_info = user_info.unwrap();
    let note: Option<StaffNote> = sqlx::query_as(sql::GET_STAFF_NOTE)
        .bind(note_id)
        .fetch_optional(&state.db)
        .await?;
    if let Some(note) = note {
        if note.by == user_info.cid {
            sqlx::query(sql::DELETE_STAFF_NOTE)
                .bind(note_id)
                .execute(&state.db)
                .await?;
            record_log(
                format!("{} removed their note #{}", user_info.cid, note_id),
                &state.db,
                true,
            )
            .await?;
        }
    }
    Ok(StatusCode::OK)
}

/// Render a page snippet that shows training notes and a button to create more.
///
/// For training staff members.
async fn snippet_get_training_records(
    State(state): State<Arc<AppState>>,
    session: Session,
    Path(cid): Path<u32>,
) -> Result<Response, AppError> {
    let user_info: Option<UserInfo> = session.get(SESSION_USER_INFO_KEY).await?;
    if let Some(redirect) =
        reject_if_not_in(&state, &user_info, PermissionsGroup::TrainingTeam).await
    {
        return Ok(redirect.into_response());
    }

    // get the records from VATUSA
    let all_training_records =
        get_training_records(cid, &state.config.vatsim.vatusa_api_key).await?;
    let training_records: Vec<_> = all_training_records
        .iter()
        .filter(|record| record.facility_id == "ZDV")
        .map(|record| {
            let record = record.clone();
            TrainingRecord {
                notes: strip_some_tags(&record.notes).replace("\n", "<br>"),
                ..record
            }
        })
        .collect();

    // include aux data from DB
    let aux_training_data: Vec<AuxiliaryTrainingData> =
        sqlx::query_as(sql::GET_AUX_TRAINING_DATA_FOR)
            .bind(cid)
            .fetch_all(&state.db)
            .await?;

    // combine both Vecs by enum and sort
    let all_training_data: Vec<TrainingDataType> = training_records
        .into_iter()
        .map(TrainingDataType::VatusaRecord)
        .chain(aux_training_data.into_iter().map(TrainingDataType::AuxData))
        .sorted_by(|record_a, record_b| record_b.get_date().cmp(&record_a.get_date()))
        .collect();

    // get trainer cids for name matching
    let trainer_cids: Vec<u32> = all_training_data
        .iter()
        .map(|record| record.trainer())
        .collect::<HashSet<u32>>()
        .iter()
        .copied()
        .collect();
    let trainers = get_multiple_controller_names(&trainer_cids).await;

    let template = state
        .templates
        .get_template("controller/training_notes.jinja")?;
    let rendered: String = template.render(context! { user_info, all_training_data, trainers })?;
    Ok(Html(rendered).into_response())
}

/// Render a page snippet that contains the controller's history.
///
/// Data sourced from VATSIM, VATUSA, and this site's own DB.
///
/// For admin staff members.
async fn snippet_get_history(
    State(state): State<Arc<AppState>>,
    session: Session,
    Path(cid): Path<u32>,
) -> Result<Response, AppError> {
    let user_info: Option<UserInfo> = session.get(SESSION_USER_INFO_KEY).await?;
    if let Some(redirect) = reject_if_not_in(&state, &user_info, PermissionsGroup::Admin).await {
        return Ok(redirect.into_response());
    }

    let vatsim_info = vatsim::get_member_info(cid)
        .await
        .map_err(|e| AppError::GenericFallback("getting VATSIM member info", e))?;
    let controller_info =
        vatusa::get_controller_info(cid, Some(&state.config.vatsim.vatusa_api_key)).await?;
    let rating_history =
        vatusa::get_controller_rating_history(cid, &state.config.vatsim.vatusa_api_key).await?;

    let template = state.templates.get_template("controller/history.jinja")?;
    let rendered: String = template.render(context! {
        user_info,
        vatsim_info,
        controller_info,
        rating_history,
    })?;
    Ok(Html(rendered).into_response())
}

#[derive(Debug, Clone, Deserialize)]
struct NewTrainingRecordForm {
    date: String,
    duration: String,
    position: String,
    location: u8,
    notes: String,
    timezone: String,
}

/// Submit a new training note for the controller.
///
/// For training staff members.
async fn post_add_training_note(
    State(state): State<Arc<AppState>>,
    session: Session,
    Path(cid): Path<u32>,
    Form(record_form): Form<NewTrainingRecordForm>,
) -> Result<Redirect, AppError> {
    let user_info: Option<UserInfo> = session.get(SESSION_USER_INFO_KEY).await?;
    if let Some(redirect) =
        reject_if_not_in(&state, &user_info, PermissionsGroup::TrainingTeam).await
    {
        return Ok(redirect);
    }
    let user_info = user_info.unwrap();
    debug!(
        "{} submitting training note for {cid} with location {}",
        user_info.cid, record_form.location
    );
    let date = js_timestamp_to_utc(&record_form.date, &record_form.timezone)?;
    if record_form.location == 255 {
        // RCE recommendation; don't report to VATUSA
        sqlx::query(sql::ADD_AUX_TRAINING_DATA)
            .bind(cid)
            .bind(user_info.cid)
            .bind(&record_form.position)
            .bind(date)
            .bind(&record_form.notes)
            .execute(&state.db)
            .await?;
        record_log(
            format!("{} recommended {} for an RCE", user_info.cid, cid),
            &state.db,
            true,
        )
        .await?;
        push_flashed_message(session, MessageLevel::Info, "New RCE recommendation saved").await?;
        return Ok(Redirect::to(&format!("/controller/{cid}")));
    }
    let record_form = if record_form.location == 100 {
        // record a no-show DB entry
        sqlx::query(sql::CREATE_NEW_NO_SHOW_ENTRY)
            .bind(cid)
            .bind(user_info.cid)
            .bind("training")
            .bind(date)
            .bind(format!(
                "Auto-rendered from training note. {}",
                record_form.notes
            ))
            .execute(&state.db)
            .await?;
        record_log(
            format!(
                "{} submitted a no-show in a training note for {cid}",
                user_info.cid
            ),
            &state.db,
            true,
        )
        .await?;

        let controller_record: Option<Controller> = sqlx::query_as(sql::GET_CONTROLLER_BY_CID)
            .bind(cid)
            .fetch_optional(&state.db)
            .await?;
        if let Some(data) = controller_record {
            // email the TA
            send_mail_raw(
                &state.config,
                "ta@zdvartcc.org",
                &format!(
                    "A student received a no-show training record: {} {} ({})",
                    data.first_name, data.last_name, cid
                ),
                "ta@zdvartcc.org",
            )
            .await
            .map_err(AppError::EmailError)?;
        }

        // update the form to use the VATUSA no-show field
        NewTrainingRecordForm {
            location: 0,
            ..record_form
        }
    } else {
        // otherwise, don't touch the form data
        record_form.clone()
    };
    let new_record = NewTrainingRecord {
        instructor_id: format!("{}", user_info.cid),
        date,
        position: record_form.position,
        duration: record_form.duration,
        location: record_form.location,
        notes: record_form.notes,
    };
    match save_training_record(&state.config.vatsim.vatusa_api_key, cid, &new_record).await {
        Ok(_) => {
            push_flashed_message(session, MessageLevel::Info, "New training record saved").await?;
            record_log(
                format!("{} submitted new training record for {cid}", user_info.cid),
                &state.db,
                true,
            )
            .await?;
        }
        Err(e) => {
            let reason = if let AppError::VatusaApi(e) = e {
                match e {
                    vzdv::vatusa::VatusaError::Reason(_, _, reason) => reason,
                    _ => String::from("unknown"),
                }
            } else {
                String::from("unknown")
            };
            error!("Error saving new training record for {cid}: {reason}");
            push_flashed_message(
                session,
                MessageLevel::Error,
                &format!("Could not save training record: {reason}"),
            )
            .await?;
        }
    }

    Ok(Redirect::to(&format!("/controller/{cid}")))
}

/// Submit a form to change the controller's roles.
///
/// For admin staff members.
async fn post_set_roles(
    State(state): State<Arc<AppState>>,
    session: Session,
    Path(cid): Path<u32>,
    Form(roles_form): Form<HashMap<String, String>>,
) -> Result<Redirect, AppError> {
    let user_info: Option<UserInfo> = session.get(SESSION_USER_INFO_KEY).await?;
    if let Some(redirect) = reject_if_not_in(&state, &user_info, PermissionsGroup::SomeStaff).await
    {
        return Ok(redirect);
    }
    let roles_can_set = roles_to_set(&state.db, &user_info).await?;
    let user_info = user_info.unwrap();
    let controller: Option<Controller> = sqlx::query_as(sql::GET_CONTROLLER_BY_CID)
        .bind(cid)
        .fetch_optional(&state.db)
        .await?;
    let controller = match controller {
        Some(c) => c,
        None => {
            warn!(
                "{} tried to set roles for unknown controller {cid}",
                user_info.cid
            );
            push_flashed_message(session, MessageLevel::Error, "Unknown controller").await?;
            return Ok(Redirect::to(&format!("/controller/{cid}")));
        }
    };
    let existing_roles: Vec<_> = controller.roles.split_terminator(',').collect();
    let mut resolved_roles = Vec::new();
    let roles_to_set: Vec<_> = roles_form.keys().map(|s| s.as_str()).collect();

    // handle the form's data
    for role in existing_roles {
        if roles_can_set.contains(role) {
            if roles_to_set.contains(&role) {
                // if this user can set the role and it is still set, keep it
                resolved_roles.push(role);
            } else {
                // if this user can set the role and it no longer set, remove it
                // no-op
            }
        } else {
            // if this user cannot set the role, keep it
            resolved_roles.push(role);
        }
    }
    for role in &roles_to_set {
        // protection against form interception
        if roles_can_set.contains(*role) {
            resolved_roles.push(role);
        }
    }

    let new_roles = resolved_roles
        .iter()
        .collect::<HashSet<&&str>>()
        .iter()
        .join(",");

    sqlx::query(sql::SET_CONTROLLER_ROLES)
        .bind(cid)
        .bind(&new_roles)
        .execute(&state.db)
        .await?;
    push_flashed_message(session, MessageLevel::Info, "Roles updated").await?;
    let message = format!(
        "{} is setting roles for {cid} to '{}'; was '{}'",
        user_info.cid, new_roles, controller.roles
    );
    record_log(message.clone(), &state.db, true).await?;
    post_audit(&state.config, message);
    Ok(Redirect::to(&format!("/controller/{cid}")))
}

#[derive(Debug, Deserialize)]
struct RemoveControllerForm {
    reason: String,
}

/// Form submission to remove a controller from the roster.
///
/// For admin staff members.
async fn post_remove_controller(
    State(state): State<Arc<AppState>>,
    session: Session,
    Path(cid): Path<u32>,
    Form(removal_form): Form<RemoveControllerForm>,
) -> Result<Redirect, AppError> {
    let user_info: Option<UserInfo> = session.get(SESSION_USER_INFO_KEY).await?;
    if let Some(redirect) = reject_if_not_in(&state, &user_info, PermissionsGroup::Admin).await {
        return Ok(redirect);
    }
    let user_info = user_info.unwrap();

    match remove_controller_from_roster(
        cid,
        user_info.cid,
        &removal_form.reason,
        &state.db,
        &state.config,
    )
    .await
    {
        Ok(_) => {
            push_flashed_message(
                session,
                MessageLevel::Info,
                "Controller removed from roster",
            )
            .await?;
        }
        Err(e) => {
            error!("Error removing controller {cid} from roster, controller page: {e}");
            push_flashed_message(session, MessageLevel::Error, "Controller removal failed").await?;
        }
    }

    Ok(Redirect::to(&format!("/controller/{cid}")))
}

#[derive(Debug, Deserialize)]
struct LoaUpdateForm {
    loa: String,
}

/// Form submission to set controller LOA.
///
/// For admin staff members.
async fn post_loa(
    State(state): State<Arc<AppState>>,
    session: Session,
    Path(cid): Path<u32>,
    Form(loa_form): Form<LoaUpdateForm>,
) -> Result<Redirect, AppError> {
    let user_info: Option<UserInfo> = session.get(SESSION_USER_INFO_KEY).await?;
    if let Some(redirect) = reject_if_not_in(&state, &user_info, PermissionsGroup::Admin).await {
        return Ok(redirect);
    }
    let user_info = user_info.unwrap();

    let controller: Option<Controller> = sqlx::query_as(sql::GET_CONTROLLER_BY_CID)
        .bind(cid)
        .fetch_optional(&state.db)
        .await?;
    if controller.is_none() {
        warn!(
            "{} tried to update LOA for unknown controller {cid}",
            user_info.cid
        );
        push_flashed_message(session, MessageLevel::Error, "Unknown controller").await?;
        return Ok(Redirect::to(&format!("/controller/{cid}")));
    }

    if loa_form.loa.is_empty() {
        let dt: Option<String> = None;
        sqlx::query(sql::CONTROLLER_UPDATE_LOA)
            .bind(cid)
            .bind(dt)
            .execute(&state.db)
            .await?;
    } else {
        let dt = js_timestamp_to_utc(&loa_form.loa, "UTC")?;
        sqlx::query(sql::CONTROLLER_UPDATE_LOA)
            .bind(cid)
            .bind(dt)
            .execute(&state.db)
            .await?;
    }
    record_log(
        format!(
            "{} updated LOA for {cid} to: {}",
            user_info.cid, loa_form.loa
        ),
        &state.db,
        true,
    )
    .await?;
    Ok(Redirect::to(&format!("/controller/{cid}")))
}

async fn post_vatusa_sync(
    State(state): State<Arc<AppState>>,
    session: Session,
    Path(cid): Path<u32>,
) -> Result<StatusCode, AppError> {
    let user_info: Option<UserInfo> = session.get(SESSION_USER_INFO_KEY).await?;
    if reject_if_not_in(&state, &user_info, PermissionsGroup::TrainingTeam)
        .await
        .is_some()
    {
        push_flashed_message(
            session,
            MessageLevel::Error,
            "You do not have permission to do that",
        )
        .await?;
        return Ok(StatusCode::FORBIDDEN);
    }
    let user_info = user_info.unwrap();
    record_log(
        format!("{} requested VATUSA sync for {cid}", user_info.cid),
        &state.db,
        true,
    )
    .await?;

    // store the IPC action
    sqlx::query(sql::INSERT_INTO_IPC)
        .bind(Uuid::new_v4().to_string())
        .bind("VATUSA_SYNC")
        .bind(format!("{cid}"))
        .execute(&state.db)
        .await?;
    push_flashed_message(
        session,
        MessageLevel::Success,
        "Scheduled quick VATUSA sync",
    )
    .await?;
    Ok(StatusCode::OK)
}

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/controller/{cid}", get(page_controller))
        .route("/controller/{cid}/discord/unlink", post(api_unlink_discord))
        .route("/controller/{cid}/ois", post(post_change_ois))
        .route("/controller/{cid}/certs", post(post_change_certs))
        .route("/controller/{cid}/loa", post(post_loa))
        .route("/controller/{cid}/certs/solo", post(post_new_solo_cert))
        .route(
            "/controller/{cid}/certs/solo/{cert_id}",
            delete(api_delete_solo_cert),
        )
        .route(
            "/controller/{cid}/certs/solo/edit",
            post(post_edit_solo_cert),
        )
        .route("/controller/{cid}/note", post(post_new_staff_note))
        .route(
            "/controller/{cid}/note/{note_id}",
            delete(api_delete_staff_note),
        )
        .route(
            "/controller/{cid}/training_records",
            get(snippet_get_training_records).post(post_add_training_note),
        )
        .route("/controller/{cid}/history", get(snippet_get_history))
        .route("/controller/{cid}/roles", post(post_set_roles))
        .route("/controller/{cid}/remove", post(post_remove_controller))
        .route("/controller/{cid}/vatusa_sync", post(post_vatusa_sync))
}
