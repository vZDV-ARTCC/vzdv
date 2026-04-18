//! Endpoints for viewing and registering for events.
//!
//! The CRUD of events themselves is under /admin routes.

use crate::{
    flashed_messages::{self, MessageLevel, push_flashed_message},
    shared::{
        AppError, AppState, SESSION_USER_INFO_KEY, UserInfo, is_user_member_of,
        js_timestamp_to_utc, record_log, reject_if_not_in,
    },
    vatusa::get_controller_info,
};
use axum::{
    Form, Router,
    extract::{DefaultBodyLimit, Multipart, Path, State},
    http::StatusCode,
    response::{Html, IntoResponse, Redirect, Response},
    routing::{get, post},
};
use chrono::Utc;
use log::debug;
use minijinja::context;
use serde::{Deserialize, Serialize};
use sqlx::{Pool, Sqlite};
use std::path::Path as FilePath;
use std::sync::Arc;
use tower_sessions::Session;
use uuid::Uuid;
use vzdv::{
    ControllerRating, PermissionsGroup,
    sql::{self, Controller, Event, EventPosition, EventRegistration},
};

/// Get a list of upcoming events optionally with unpublished events.
pub async fn query_for_events(db: &Pool<Sqlite>, show_all: bool) -> sqlx::Result<Vec<Event>> {
    let now = Utc::now();
    let events: Vec<Event> = if show_all {
        sqlx::query_as(sql::GET_ALL_EVENTS).fetch_all(db).await?
    } else {
        sqlx::query_as(sql::GET_PUBLISHED_EVENTS)
            .fetch_all(db)
            .await?
    };
    let events = events
        .iter()
        .filter(|event| event.end >= now)
        .cloned()
        .collect();
    Ok(events)
}

/// Render a snippet that lists published upcoming events.
///
/// No controls are rendered; instead each event links to the full
/// page for that single event.
async fn snippet_get_upcoming_events(
    State(state): State<Arc<AppState>>,
    session: Session,
) -> Result<Html<String>, AppError> {
    let user_info: Option<UserInfo> = session.get(SESSION_USER_INFO_KEY).await?;
    let show_all = is_user_member_of(&state, &user_info, PermissionsGroup::EventsTeam).await;
    let events = query_for_events(&state.db, show_all).await?;
    let template = state
        .templates
        .get_template("events/upcoming_events_snippet.jinja")?;
    let rendered = template.render(context! { user_info, events })?;
    Ok(Html(rendered))
}

/// Render a full page of upcoming events.
///
/// Basically what the homepage does, but without the rest of the homepage.
async fn get_upcoming_events(
    State(state): State<Arc<AppState>>,
    session: Session,
) -> Result<Html<String>, AppError> {
    let user_info: Option<UserInfo> = session.get(SESSION_USER_INFO_KEY).await?;
    let show_all = is_user_member_of(&state, &user_info, PermissionsGroup::EventsTeam).await;
    let events = query_for_events(&state.db, show_all).await?;
    let is_event_staff = is_user_member_of(&state, &user_info, PermissionsGroup::EventsTeam).await;
    let template = state
        .templates
        .get_template("events/upcoming_events.jinja")?;
    let flashed_messages = flashed_messages::drain_flashed_messages(session).await?;
    let rendered = template.render(context! {
        user_info,
        is_event_staff,
        events,
        flashed_messages
    })?;
    Ok(Html(rendered))
}

#[derive(Debug, Default)]
struct NewEventData {
    name: String,
    description: String,
    banner: String,
    start: String,
    end: String,
}

/// Submit the form to create a new event.
///
/// Event staff only.
async fn post_new_event_form(
    State(state): State<Arc<AppState>>,
    session: Session,
    mut form: Multipart,
) -> Result<Redirect, AppError> {
    let user_info: Option<UserInfo> = session.get(SESSION_USER_INFO_KEY).await?;
    let is_event_staff = is_user_member_of(&state, &user_info, PermissionsGroup::EventsTeam).await;
    if !is_event_staff {
        return Ok(Redirect::to("/"));
    }

    let cid = user_info.unwrap().cid;
    let mut event = NewEventData::default();

    while let Some(field) = form.next_field().await? {
        let name = field.name().ok_or(AppError::MultipartFormGet)?.to_string();
        match name.as_str() {
            "name" => {
                event.name = field.text().await?;
            }
            "description" => {
                event.description = field.text().await?;
            }
            "banner" => {
                event.banner = field.text().await?;
            }
            "banner_file" => {
                let new_uuid = Uuid::new_v4();
                let file_name = field
                    .file_name()
                    .ok_or(AppError::MultipartFormGet)?
                    .to_string();
                let file_data = field.bytes().await?;
                if file_data.is_empty() {
                    continue;
                }
                let new_file_name = format!("{new_uuid}_{file_name}");
                let write_path = FilePath::new("./assets").join(&new_file_name);
                debug!("Writing new file to assets dir as part of event creation: {new_file_name}");
                std::fs::write(write_path, file_data)?;
                event.banner = format!("{}/assets/{new_file_name}", &state.config.hosted_domain);
                // If someone uploads a banner and supplies a remote URL, then whether they get
                // the remote URL or the uploaded is undefined. That's okay for now.
            }
            "start" => {
                event.start = field.text().await?;
            }
            "end" => {
                event.end = field.text().await?;
            }
            _ => {}
        }
    }
    let start = js_timestamp_to_utc(&event.start, "UTC")?;
    let end = js_timestamp_to_utc(&event.end, "UTC")?;

    let result = sqlx::query(sql::CREATE_EVENT)
        .bind(cid)
        .bind(&event.name)
        .bind(start)
        .bind(end)
        .bind(event.description)
        .bind(event.banner)
        .execute(&state.db)
        .await?;
    record_log(
        format!(
            "{cid} created new event {}: \"{}\"",
            result.last_insert_rowid(),
            &event.name
        ),
        &state.db,
        true,
    )
    .await?;
    Ok(Redirect::to(&format!(
        "/events/{}",
        result.last_insert_rowid()
    )))
}

// NOTE: opportunity for some minor speed improvements here by not loading
// controller records twice for each controller assigned to an event.

/// Render the full page for a single event, including controls for signup.
async fn page_event(
    State(state): State<Arc<AppState>>,
    session: Session,
    Path(id): Path<u32>,
) -> Result<Response, AppError> {
    let user_info: Option<UserInfo> = session.get(SESSION_USER_INFO_KEY).await?;
    let event: Option<Event> = sqlx::query_as(sql::GET_EVENT)
        .bind(id)
        .fetch_optional(&state.db)
        .await?;
    let event = match event {
        Some(e) => e,
        None => {
            flashed_messages::push_flashed_message(
                session,
                flashed_messages::MessageLevel::Error,
                "Event not found",
            )
            .await?;
            return Ok(Redirect::to("/").into_response());
        }
    };

    let not_staff_redirect =
        reject_if_not_in(&state, &user_info, PermissionsGroup::EventsTeam).await;
    if !event.published {
        // only event staff can see unpublished events
        if let Some(redirect) = not_staff_redirect {
            return Ok(redirect.into_response());
        }
    }

    let user_controller: Option<Controller> = match &user_info {
        Some(info) => {
            sqlx::query_as(sql::GET_CONTROLLER_BY_CID)
                .bind(info.cid)
                .fetch_optional(&state.db)
                .await?
        }
        None => None,
    };

    let positions_raw = {
        let mut p: Vec<EventPosition> = sqlx::query_as(sql::GET_EVENT_POSITIONS)
            .bind(event.id)
            .fetch_all(&state.db)
            .await?;
        p.sort_by(|a, b| a.name.partial_cmp(&b.name).unwrap());
        p
    };
    let positions = event_positions_extra(&positions_raw, &state.db).await?;
    let registrations = event_registrations_extra(event.id, &positions_raw, &state.db).await?;
    let all_controllers: Vec<Controller> = sqlx::query_as(sql::GET_ALL_CONTROLLERS_ON_ROSTER)
        .fetch_all(&state.db)
        .await?;
    let all_controllers: Vec<(u32, String)> = all_controllers
        .iter()
        .map(|controller| {
            (
                controller.cid,
                format!(
                    "{} {} ({})",
                    controller.first_name,
                    controller.last_name,
                    match controller.operating_initials.as_ref() {
                        Some(oi) => {
                            if oi.is_empty() { "??" } else { oi }
                        }
                        None => "??",
                    }
                ),
            )
        })
        .collect();
    let template = state.templates.get_template("events/event.jinja")?;
    let self_register: Option<EventRegistration> = if let Some(user_info) = &user_info {
        sqlx::query_as(sql::GET_EVENT_REGISTRATION_FOR)
            .bind(id)
            .bind(user_info.cid)
            .fetch_optional(&state.db)
            .await?
    } else {
        None
    };

    let flashed_messages = flashed_messages::drain_flashed_messages(session).await?;
    let rendered = template.render(context! {
        user_info,
        event,
        positions,
        positions_raw,
        registrations,
        all_controllers,
        self_register,
        is_on_roster => user_controller.map(|c| c.is_on_roster).unwrap_or_default(),
        is_event_staff => not_staff_redirect.is_none(),
        event_not_over =>  Utc::now() < event.end,
        flashed_messages,
    })?;
    Ok(Html(rendered).into_response())
}

#[derive(Serialize)]
struct EventPositionDisplay {
    id: u32,
    name: String,
    category: String,
    controller: String,
}

/// Supply event positions with the controller's name, if set.
async fn event_positions_extra(
    positions: &[EventPosition],
    db: &Pool<Sqlite>,
) -> Result<Vec<EventPositionDisplay>, AppError> {
    let mut ret = Vec::with_capacity(positions.len());
    for position in positions {
        if let Some(pos_cid) = position.cid {
            let controller: Option<Controller> = sqlx::query_as(sql::GET_CONTROLLER_BY_CID)
                .bind(pos_cid)
                .fetch_optional(db)
                .await?;
            if let Some(controller) = controller {
                ret.push(EventPositionDisplay {
                    id: position.id,
                    name: position.name.clone(),
                    category: position.category.clone(),
                    controller: format!(
                        "{} {} ({})",
                        controller.first_name,
                        controller.last_name,
                        match controller.operating_initials.as_ref() {
                            Some(oi) => oi,
                            None => "??",
                        }
                    ),
                });
                continue;
            }
        }
        ret.push(EventPositionDisplay {
            id: position.id,
            name: position.name.clone(),
            category: position.category.clone(),
            controller: "unassigned".to_string(),
        });
    }
    ret.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(ret)
}

#[derive(Serialize)]
struct EventRegistrationDisplay {
    controller: String,
    cid: u32,
    choice_1: String,
    choice_2: String,
    choice_3: String,
    notes: String,
    is_assigned: bool,
}

/// Supply event registration data with controller and position names.
async fn event_registrations_extra(
    event_id: u32,
    positions: &[EventPosition],
    db: &Pool<Sqlite>,
) -> Result<Vec<EventRegistrationDisplay>, AppError> {
    let registrations: Vec<EventRegistration> = sqlx::query_as(sql::GET_EVENT_REGISTRATIONS)
        .bind(event_id)
        .fetch_all(db)
        .await?;
    let mut ret = Vec::with_capacity(registrations.len());

    for registration in &registrations {
        let c_1 = positions
            .iter()
            .find(|pos| pos.id == registration.choice_1)
            .map(|pos| pos.name.clone());
        let c_2 = positions
            .iter()
            .find(|pos| pos.id == registration.choice_2)
            .map(|pos| pos.name.clone());
        let c_3 = positions
            .iter()
            .find(|pos| pos.id == registration.choice_3)
            .map(|pos| pos.name.clone());
        let controller_db: Option<Controller> = sqlx::query_as(sql::GET_CONTROLLER_BY_CID)
            .bind(registration.cid)
            .fetch_optional(db)
            .await?;
        let controller = match controller_db {
            Some(ref c) => format!(
                "{} {} ({}) - {}",
                c.first_name,
                c.last_name,
                match c.operating_initials.as_ref() {
                    Some(oi) => oi,
                    None => "??",
                },
                ControllerRating::try_from(c.rating)
                    .map(|r| r.as_str())
                    .unwrap_or(""),
            ),
            None => "???".to_string(),
        };
        let notes = match registration.notes.as_ref() {
            Some(s) => s.clone(),
            None => String::new(),
        };
        ret.push(EventRegistrationDisplay {
            controller,
            cid: controller_db.as_ref().map(|c| c.cid).unwrap_or_default(),
            choice_1: c_1.unwrap_or_default(),
            choice_2: c_2.unwrap_or_default(),
            choice_3: c_3.unwrap_or_default(),
            notes,
            is_assigned: if let Some(record) = controller_db {
                positions
                    .iter()
                    .any(|p| p.cid.unwrap_or_default() == record.cid)
            } else {
                false
            },
        });
    }

    Ok(ret)
}

#[derive(Debug, Default)]
struct UpdatedEventData {
    name: String,
    description: String,
    published: bool,
    banner: String,
    start: String,
    end: String,
    timezone: String,
}

/// Submit a form to update an event, and redirect back to the same page.
///
/// Event staff only.
async fn post_edit_event_form(
    State(state): State<Arc<AppState>>,
    session: Session,
    Path(id): Path<u32>,
    mut form: Multipart,
) -> Result<Redirect, AppError> {
    let user_info: Option<UserInfo> = session.get(SESSION_USER_INFO_KEY).await?;
    if let Some(redirect) = reject_if_not_in(&state, &user_info, PermissionsGroup::EventsTeam).await
    {
        return Ok(redirect);
    }

    let event: Option<Event> = sqlx::query_as(sql::GET_EVENT)
        .bind(id)
        .fetch_optional(&state.db)
        .await?;
    if event.is_none() {
        return Ok(Redirect::to("/"));
    }

    let cid = user_info.unwrap().cid;
    let mut event = UpdatedEventData::default();

    while let Some(field) = form.next_field().await? {
        let name = field.name().ok_or(AppError::MultipartFormGet)?.to_string();
        match name.as_str() {
            "name" => {
                event.name = field.text().await?;
            }
            "description" => {
                event.description = field.text().await?;
            }
            "published" => {
                event.published = true;
            }
            "banner" => {
                event.banner = field.text().await?;
            }
            "banner_file" => {
                let new_uuid = Uuid::new_v4();
                let file_name = field
                    .file_name()
                    .ok_or(AppError::MultipartFormGet)?
                    .to_string();
                let file_data = field.bytes().await?;
                if file_data.is_empty() {
                    continue;
                }
                let new_file_name = format!("{new_uuid}_{file_name}");
                let write_path = FilePath::new("./assets").join(&new_file_name);
                debug!("Writing new file to assets dir as part of event update: {new_file_name}");
                std::fs::write(write_path, file_data)?;
                event.banner = format!("{}/assets/{new_file_name}", &state.config.hosted_domain);
                // If someone uploads a banner and supplies a remote URL, then whether they get
                // the remote URL or the uploaded is undefined. That's okay for now.
            }
            "start" => {
                event.start = field.text().await?;
            }
            "end" => {
                event.end = field.text().await?;
            }
            "timezone" => {
                event.timezone = field.text().await?;
            }
            _ => {}
        }
    }

    let start = js_timestamp_to_utc(&event.start, &event.timezone)?;
    let end = js_timestamp_to_utc(&event.end, &event.timezone)?;

    sqlx::query(sql::UPDATE_EVENT)
        .bind(id)
        .bind(event.name)
        .bind(event.published)
        .bind(start)
        .bind(end)
        .bind(event.description)
        .bind(event.banner)
        .execute(&state.db)
        .await?;
    record_log(format!("{cid} edited event {id}"), &state.db, true).await?;
    Ok(Redirect::to(&format!("/events/{id}")))
}

/// API endpoint to delete an event.
///
/// Event staff only.
async fn api_delete_event(
    State(state): State<Arc<AppState>>,
    session: Session,
    Path(id): Path<u32>,
) -> Result<StatusCode, AppError> {
    let user_info: Option<UserInfo> = session.get(SESSION_USER_INFO_KEY).await?;
    if !is_user_member_of(&state, &user_info, PermissionsGroup::EventsTeam).await {
        return Ok(StatusCode::FORBIDDEN);
    }
    let event: Option<Event> = sqlx::query_as(sql::GET_EVENT)
        .bind(id)
        .fetch_optional(&state.db)
        .await?;
    if event.is_some() {
        let mut tx = state.db.begin().await?;
        sqlx::query(sql::DELETE_EVENT_REGISTRATIONS_FOR)
            .bind(id)
            .execute(&mut *tx)
            .await?;
        sqlx::query(sql::DELETE_EVENT_POSITIONS_FOR)
            .bind(id)
            .execute(&mut *tx)
            .await?;
        sqlx::query(sql::DELETE_EVENT)
            .bind(id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;

        record_log(
            format!("{} deleted event {id}", user_info.unwrap().cid),
            &state.db,
            true,
        )
        .await?;
        flashed_messages::push_flashed_message(
            session,
            flashed_messages::MessageLevel::Info,
            "Event deleted",
        )
        .await?;
        Ok(StatusCode::OK)
    } else {
        Ok(StatusCode::NOT_FOUND)
    }
}

#[derive(Deserialize)]
struct RegisterForm {
    choice_1: u32,
    choice_2: u32,
    choice_3: u32,
    notes: String,
}

/// Submit a form to register for an event or update a registration.
async fn post_register_for_event(
    State(state): State<Arc<AppState>>,
    session: Session,
    Path(id): Path<u32>,
    Form(register_data): Form<RegisterForm>,
) -> Result<Redirect, AppError> {
    let event: Option<Event> = sqlx::query_as(sql::GET_EVENT)
        .bind(id)
        .fetch_optional(&state.db)
        .await?;
    if event.is_none() {
        return Ok(Redirect::to("/events"));
    }
    let user_info: Option<UserInfo> = session.get(SESSION_USER_INFO_KEY).await?;
    let cid = if let Some(user_info) = user_info {
        user_info.cid
    } else {
        return Ok(Redirect::to(&format!("/events/{id}")));
    };

    let c_1 = if register_data.choice_1 == 0u32 {
        None
    } else {
        Some(register_data.choice_1)
    };
    let c_2 = if register_data.choice_2 == 0u32 {
        None
    } else {
        Some(register_data.choice_2)
    };
    let c_3 = if register_data.choice_3 == 0u32 {
        None
    } else {
        Some(register_data.choice_3)
    };

    // upsert the registration
    let notes = if register_data.notes.len() > 500 {
        &register_data.notes[0..500]
    } else {
        &register_data.notes
    };
    sqlx::query(sql::UPSERT_EVENT_REGISTRATION)
        .bind(id)
        .bind(cid)
        .bind(c_1)
        .bind(c_2)
        .bind(c_3)
        .bind(notes)
        .execute(&state.db)
        .await?;
    record_log(
        format!(
            "{cid} registered for event {id}: {} {} {}",
            c_1.unwrap_or_default(),
            c_2.unwrap_or_default(),
            c_3.unwrap_or_default()
        ),
        &state.db,
        true,
    )
    .await?;

    Ok(Redirect::to(&format!("/events/{id}")))
}

/// Completely unregister a controller from an event.
async fn api_register_unregister(
    State(state): State<Arc<AppState>>,
    session: Session,
    Path(id): Path<u32>,
) -> Result<StatusCode, AppError> {
    let user_info: Option<UserInfo> = session.get(SESSION_USER_INFO_KEY).await?;
    let cid = if let Some(user_info) = user_info {
        user_info.cid
    } else {
        return Ok(StatusCode::UNAUTHORIZED);
    };
    // get the registration ID from event ID & CID
    let existing_registration: Option<EventRegistration> =
        sqlx::query_as(sql::GET_EVENT_REGISTRATION_FOR)
            .bind(id)
            .bind(cid)
            .fetch_optional(&state.db)
            .await?;
    if let Some(existing) = existing_registration {
        sqlx::query(sql::DELETE_EVENT_REGISTRATION)
            .bind(existing.id)
            .execute(&state.db)
            .await?;
    }
    // remove the controller from any positions in this event
    sqlx::query(sql::CLEAR_CID_FROM_EVENT_POSITIONS)
        .bind(id)
        .bind(cid)
        .execute(&state.db)
        .await?;
    record_log(
        format!("{cid} removed their registration to event {id}"),
        &state.db,
        true,
    )
    .await?;
    Ok(StatusCode::ACCEPTED)
}

#[derive(Deserialize)]
struct AddPositionForm {
    name: String,
    category: String,
}

/// Submit a form to add a new position to the event.
async fn post_add_position(
    State(state): State<Arc<AppState>>,
    session: Session,
    Path(id): Path<u32>,
    Form(new_position_data): Form<AddPositionForm>,
) -> Result<Redirect, AppError> {
    let user_info: Option<UserInfo> = session.get(SESSION_USER_INFO_KEY).await?;
    if let Some(redirect) = reject_if_not_in(&state, &user_info, PermissionsGroup::EventsTeam).await
    {
        return Ok(redirect);
    }
    if new_position_data.name.is_empty() {
        flashed_messages::push_flashed_message(
            session,
            flashed_messages::MessageLevel::Error,
            "Must specify a value",
        )
        .await?;
        return Ok(Redirect::to(&format!("/events/{id}")));
    }

    let event: Option<Event> = sqlx::query_as(sql::GET_EVENT)
        .bind(id)
        .fetch_optional(&state.db)
        .await?;
    if event.is_some() {
        let name = new_position_data.name.to_uppercase();

        // don't allow position duplicates
        let existing: Vec<EventPosition> = sqlx::query_as(sql::GET_EVENT_POSITIONS)
            .bind(id)
            .fetch_all(&state.db)
            .await?;
        if !existing.iter().any(|position| {
            position.name == name && position.category == new_position_data.category
        }) {
            record_log(
                format!(
                    "{} adding {}/{} to event {id}",
                    user_info.unwrap().cid,
                    &new_position_data.category,
                    &name,
                ),
                &state.db,
                true,
            )
            .await?;
            sqlx::query(sql::INSERT_EVENT_POSITION)
                .bind(id)
                .bind(new_position_data.name.to_uppercase())
                .bind(&new_position_data.category)
                .execute(&state.db)
                .await?;
        }
        Ok(Redirect::to(&format!("/events/{id}")))
    } else {
        Ok(Redirect::to("/"))
    }
}

/// Delete a position from the event.
async fn post_delete_position(
    State(state): State<Arc<AppState>>,
    session: Session,
    Path((id, pos_id)): Path<(u32, u32)>,
) -> Result<Redirect, AppError> {
    let user_info: Option<UserInfo> = session.get(SESSION_USER_INFO_KEY).await?;
    if let Some(redirect) = reject_if_not_in(&state, &user_info, PermissionsGroup::EventsTeam).await
    {
        return Ok(redirect);
    }

    let event: Option<Event> = sqlx::query_as(sql::GET_EVENT)
        .bind(id)
        .fetch_optional(&state.db)
        .await?;
    if event.is_some() {
        // need to clear out any existing registrations that are using that position
        let mut tx = state.db.begin().await?;
        sqlx::query(sql::CLEAR_REGISTRATIONS_FOR_POSITION_1)
            .bind(pos_id)
            .execute(&mut *tx)
            .await?;
        sqlx::query(sql::CLEAR_REGISTRATIONS_FOR_POSITION_2)
            .bind(pos_id)
            .execute(&mut *tx)
            .await?;
        sqlx::query(sql::CLEAR_REGISTRATIONS_FOR_POSITION_3)
            .bind(pos_id)
            .execute(&mut *tx)
            .await?;
        sqlx::query(sql::DELETE_EVENT_POSITION)
            .bind(pos_id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        record_log(
            format!(
                "{} removed position {pos_id} from {id}",
                user_info.unwrap().cid,
            ),
            &state.db,
            true,
        )
        .await?;
        flashed_messages::push_flashed_message(
            session,
            flashed_messages::MessageLevel::Info,
            "Position deleted",
        )
        .await?;
        Ok(Redirect::to(&format!("/events/{id}")))
    } else {
        Ok(Redirect::to("/"))
    }
}

#[derive(Deserialize)]
struct SetPositionForm {
    position_id: u32,
    controller: u32,
    controller_cid: Option<String>,
}

/// Return a controller record, possibly creating it with VATUSA info.
async fn controller_by_cid(db: &Pool<Sqlite>, cid: u32) -> Result<u32, AppError> {
    let controller: Option<Controller> = sqlx::query_as(sql::GET_CONTROLLER_BY_CID)
        .bind(cid)
        .fetch_optional(db)
        .await?;
    if controller.is_some() {
        return Ok(cid);
    }
    // retrieve unknown controller info
    let info = get_controller_info(cid, None).await?;
    // insert in DB
    sqlx::query(sql::INSERT_USER_SIMPLE)
        .bind(cid)
        .bind(&info.first_name)
        .bind(&info.last_name)
        .bind(info.rating)
        .bind(&info.facility)
        .execute(db)
        .await?;
    Ok(cid)
}

/// Set a controller (or no-one) for a position.
async fn post_set_position(
    State(state): State<Arc<AppState>>,
    session: Session,
    Path(id): Path<u32>,
    Form(new_position_data): Form<SetPositionForm>,
) -> Result<Redirect, AppError> {
    let user_info: Option<UserInfo> = session.get(SESSION_USER_INFO_KEY).await?;
    if let Some(redirect) = reject_if_not_in(&state, &user_info, PermissionsGroup::EventsTeam).await
    {
        return Ok(redirect);
    }

    let event: Option<Event> = sqlx::query_as(sql::GET_EVENT)
        .bind(id)
        .fetch_optional(&state.db)
        .await?;
    if event.is_some() {
        let cid = if new_position_data.controller != 0 {
            Some(new_position_data.controller)
        } else if let Some(new_cid) = new_position_data.controller_cid {
            if new_cid.is_empty() {
                None
            } else {
                let new_cid = new_cid.parse()?;
                Some(controller_by_cid(&state.db, new_cid).await?)
            }
        } else {
            None
        };
        sqlx::query(sql::UPDATE_EVENT_POSITION_CONTROLLER)
            .bind(new_position_data.position_id)
            .bind(cid)
            .execute(&state.db)
            .await?;
        record_log(
            format!(
                "{} updated event {id} position {} to cid {}",
                user_info.unwrap().cid,
                new_position_data.position_id,
                new_position_data.controller
            ),
            &state.db,
            true,
        )
        .await?;
        Ok(Redirect::to(&format!("/events/{id}")))
    } else {
        Ok(Redirect::to("/"))
    }
}

#[derive(Debug, Deserialize)]
struct NoShowForm {
    cid: u32,
    notes: String,
}

/// Record a no-show entry.
async fn post_no_show(
    State(state): State<Arc<AppState>>,
    session: Session,
    Path(id): Path<u32>,
    Form(no_show_form): Form<NoShowForm>,
) -> Result<Redirect, AppError> {
    let user_info: Option<UserInfo> = session.get(SESSION_USER_INFO_KEY).await?;
    if let Some(redirect) = reject_if_not_in(&state, &user_info, PermissionsGroup::EventsTeam).await
    {
        return Ok(redirect);
    }
    let user_info = user_info.unwrap();
    let event: Option<Event> = sqlx::query_as(sql::GET_EVENT)
        .bind(id)
        .fetch_optional(&state.db)
        .await?;
    if event.is_none() {
        return Ok(Redirect::to("/"));
    }
    sqlx::query(sql::CREATE_NEW_NO_SHOW_ENTRY)
        .bind(no_show_form.cid)
        .bind(user_info.cid)
        .bind("event")
        .bind(Utc::now())
        .bind(format!("Event {id}: {}", no_show_form.notes))
        .execute(&state.db)
        .await?;
    record_log(
        format!(
            "{} submitted a no-show event record for {} for event {id}",
            user_info.cid, no_show_form.cid
        ),
        &state.db,
        true,
    )
    .await?;

    push_flashed_message(session, MessageLevel::Success, "Added no show entry").await?;
    Ok(Redirect::to(&format!("/events/{id}")))
}

/// This file's routes and templates.
pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/events/upcoming", get(snippet_get_upcoming_events))
        .route(
            "/events",
            get(get_upcoming_events)
                .post(post_new_event_form)
                .layer(DefaultBodyLimit::disable()), // no upload limit on this endpoint
        )
        .route(
            "/events/{id}",
            get(page_event)
                .delete(api_delete_event)
                .post(post_edit_event_form)
                .layer(DefaultBodyLimit::disable()), // no upload limit on this endpoint
        )
        .route("/events/{id}/register", post(post_register_for_event))
        .route("/events/{id}/unregister", post(api_register_unregister))
        .route("/events/{id}/add_position", post(post_add_position))
        .route(
            "/events/{id}/delete_position/{pos_id}",
            post(post_delete_position),
        )
        .route("/events/{id}/set_position", post(post_set_position))
        .route("/events/{id}/no_show", post(post_no_show))
}
