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
use itertools::Itertools;
use log::debug;
use minijinja::context;
use serde::{Deserialize, Serialize};
use sqlx::{Pool, Sqlite};
use std::collections::HashMap;
use std::fmt;
use std::path::Path as FilePath;
use std::sync::Arc;
use tower_sessions::Session;
use uuid::Uuid;
use vzdv::{
    ControllerRating, PermissionsGroup,
    sql::{self, Controller, Event, EventCicPosition, EventPosition, EventRegistration},
};

/// Longest allowed name for a CIC position's category.
const CIC_CATEGORY_MAX_LENGTH: usize = 32;

/// Categories that event positions are grouped into.
const POSITION_CATEGORIES: [&str; 3] = ["Enroute", "TRACON", "Local"];

/// One of a controller's ranked choices when registering for an event.
#[derive(Debug, PartialEq)]
enum RegistrationChoice {
    Empty,
    /// A specific position, by ID
    Position(u32),
    /// Any position in the category
    Any(&'static str),
}

impl RegistrationChoice {
    /// Parse a value from the register form's dropdown.
    ///
    /// Positions that aren't part of the event and unknown categories are
    /// treated as empty, as can happen if staff delete a position while
    /// someone has the form open.
    fn parse(value: &str, positions: &[EventPosition]) -> Self {
        if let Some(category) = value.strip_prefix("any:") {
            return match POSITION_CATEGORIES.iter().find(|&&c| c == category) {
                Some(category) => Self::Any(category),
                None => Self::Empty,
            };
        }
        match value.parse() {
            Ok(id) if positions.iter().any(|position| position.id == id) => Self::Position(id),
            _ => Self::Empty,
        }
    }

    fn position(&self) -> Option<u32> {
        match self {
            Self::Position(id) => Some(*id),
            _ => None,
        }
    }

    fn any_category(&self) -> Option<&'static str> {
        match self {
            Self::Any(category) => Some(category),
            _ => None,
        }
    }
}

impl fmt::Display for RegistrationChoice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => write!(f, "0"),
            Self::Position(id) => write!(f, "{id}"),
            Self::Any(category) => write!(f, "any:{category}"),
        }
    }
}

/// Format a controller's name for display on the event page
fn format_controller_name(controller: &Controller) -> String {
    format!(
        "{} {} ({})",
        controller.first_name,
        controller.last_name,
        match controller.operating_initials.as_deref() {
            Some(oi) if !oi.is_empty() => oi,
            _ => "??",
        }
    )
}

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

    let mut tx = state.db.begin().await?;
    let result = sqlx::query(sql::CREATE_EVENT)
        .bind(cid)
        .bind(&event.name)
        .bind(start)
        .bind(end)
        .bind(event.description)
        .bind(event.banner)
        .execute(&mut *tx)
        .await?;
    sqlx::query(sql::INSERT_DEFAULT_EVENT_CIC_POSITIONS)
        .bind(result.last_insert_rowid())
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
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
    let cic_positions = event_cic_positions_extra(event.id, &state.db).await?;
    let registrations =
        event_registrations_extra(event.id, &positions_raw, &cic_positions, &state.db).await?;
    let any_pools = any_category_pools(&registrations);
    let registered_controllers = event_registered_controllers(event.id, &state.db).await?;
    let all_controllers: Vec<Controller> = sqlx::query_as(sql::GET_ALL_CONTROLLERS_ON_ROSTER)
        .fetch_all(&state.db)
        .await?;
    let all_controllers: Vec<(u32, String)> = all_controllers
        .iter()
        .map(|controller| (controller.cid, format_controller_name(controller)))
        .sorted_by(|a, b| a.1.cmp(&b.1))
        .collect();
    let enroute_splits: Vec<sql::EnrouteSectorSplit> =
        sqlx::query_as(sql::GET_ALL_ENROUTE_SECTOR_SPLITS)
            .fetch_all(&state.db)
            .await?;
    let assigned_split: Option<sql::EnrouteSectorSplit> =
        sqlx::query_as(sql::GET_EVENT_ENROUTE_SECTOR_SPLIT)
            .bind(event.id)
            .fetch_optional(&state.db)
            .await?;

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
        cic_positions,
        positions_raw,
        position_categories => POSITION_CATEGORIES,
        registrations,
        any_pools,
        registered_controllers,
        all_controllers,
        enroute_splits,
        assigned_split,
        self_register,
        is_on_roster => user_controller.map(|c| c.is_on_roster).unwrap_or_default(),
        is_event_staff => not_staff_redirect.is_none(),
        event_not_over =>  Utc::now() < event.end,
        cic_category_max_length => CIC_CATEGORY_MAX_LENGTH,
        flashed_messages,
    })?;
    Ok(Html(rendered).into_response())
}

#[derive(Serialize)]
struct EventPositionDisplay {
    id: u32,
    name: String,
    category: String,
    cid: Option<u32>,
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
                    cid: Some(controller.cid),
                    controller: format_controller_name(&controller),
                });
                continue;
            }
        }
        ret.push(EventPositionDisplay {
            id: position.id,
            name: position.name.clone(),
            category: position.category.clone(),
            cid: None,
            controller: "unassigned".to_string(),
        });
    }
    ret.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(ret)
}

#[derive(Serialize)]
struct EventCicPositionDisplay {
    id: u32,
    category: String,
    cid: Option<u32>,
    controller: String,
}

/// Supply the event's CIC positions with the controller's name, if set.
async fn event_cic_positions_extra(
    event_id: u32,
    db: &Pool<Sqlite>,
) -> Result<Vec<EventCicPositionDisplay>, AppError> {
    let positions: Vec<EventCicPosition> = sqlx::query_as(sql::GET_EVENT_CIC_POSITIONS)
        .bind(event_id)
        .fetch_all(db)
        .await?;
    let mut ret = Vec::with_capacity(positions.len());
    for position in positions {
        let controller: Option<Controller> = match position.cid {
            Some(cid) => {
                sqlx::query_as(sql::GET_CONTROLLER_BY_CID)
                    .bind(cid)
                    .fetch_optional(db)
                    .await?
            }
            None => None,
        };
        ret.push(EventCicPositionDisplay {
            id: position.id,
            category: position.category,
            cid: controller.as_ref().map(|c| c.cid),
            controller: match &controller {
                Some(c) => format_controller_name(c),
                None => "unassigned".to_string(),
            },
        });
    }
    Ok(ret)
}

/// Controllers registered for the event, for selecting a CIC.
async fn event_registered_controllers(
    event_id: u32,
    db: &Pool<Sqlite>,
) -> Result<Vec<(u32, String)>, AppError> {
    let registrations: Vec<EventRegistration> = sqlx::query_as(sql::GET_EVENT_REGISTRATIONS)
        .bind(event_id)
        .fetch_all(db)
        .await?;
    let mut controllers = Vec::with_capacity(registrations.len());
    for registration in registrations {
        if let Some(controller) = sqlx::query_as::<_, Controller>(sql::GET_CONTROLLER_BY_CID)
            .bind(registration.cid)
            .fetch_optional(db)
            .await?
        {
            controllers.push((controller.cid, format_controller_name(&controller)));
        }
    }
    controllers.sort_by(|a, b| a.1.cmp(&b.1));
    Ok(controllers)
}

#[derive(Serialize)]
struct EventRegistrationDisplay {
    controller: String,
    cid: u32,
    choice_1: String,
    choice_2: String,
    choice_3: String,
    /// Categories chosen as "Any <category>", by choice rank
    any_choices: [Option<String>; 3],
    notes: String,
    is_assigned: bool,
}

/// Name of a registration choice, which is either a position or "Any <category>".
fn registration_choice_name(
    position_id: u32,
    any_category: Option<&str>,
    positions: &[EventPosition],
) -> String {
    match any_category {
        Some(category) => format!("Any {category}"),
        None => positions
            .iter()
            .find(|pos| pos.id == position_id)
            .map(|pos| pos.name.clone())
            .unwrap_or_default(),
    }
}

/// Supply event registration data with controller and position names.
async fn event_registrations_extra(
    event_id: u32,
    positions: &[EventPosition],
    cic_positions: &[EventCicPositionDisplay],
    db: &Pool<Sqlite>,
) -> Result<Vec<EventRegistrationDisplay>, AppError> {
    let registrations: Vec<EventRegistration> = sqlx::query_as(sql::GET_EVENT_REGISTRATIONS)
        .bind(event_id)
        .fetch_all(db)
        .await?;
    let mut ret = Vec::with_capacity(registrations.len());

    for registration in &registrations {
        let c_1 = registration_choice_name(
            registration.choice_1,
            registration.choice_1_any.as_deref(),
            positions,
        );
        let c_2 = registration_choice_name(
            registration.choice_2,
            registration.choice_2_any.as_deref(),
            positions,
        );
        let c_3 = registration_choice_name(
            registration.choice_3,
            registration.choice_3_any.as_deref(),
            positions,
        );
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
            choice_1: c_1,
            choice_2: c_2,
            choice_3: c_3,
            any_choices: [
                registration.choice_1_any.clone(),
                registration.choice_2_any.clone(),
                registration.choice_3_any.clone(),
            ],
            notes,
            is_assigned: if let Some(record) = controller_db {
                positions.iter().any(|p| p.cid == Some(record.cid))
                    || cic_positions.iter().any(|p| p.cid == Some(record.cid))
            } else {
                false
            },
        });
    }

    Ok(ret)
}

#[derive(Serialize)]
struct AnyPoolEntry {
    controller: String,
    cid: u32,
    /// Which of their choices this was
    rank: usize,
    is_assigned: bool,
}

/// Group the controllers who chose "Any <category>" by category,
/// ordered by how highly they ranked it.
fn any_category_pools(
    registrations: &[EventRegistrationDisplay],
) -> HashMap<&'static str, Vec<AnyPoolEntry>> {
    POSITION_CATEGORIES
        .iter()
        .map(|&category| {
            let entries = registrations
                .iter()
                .filter_map(|registration| {
                    let rank = registration
                        .any_choices
                        .iter()
                        .position(|choice| choice.as_deref() == Some(category))?
                        + 1;
                    Some(AnyPoolEntry {
                        controller: registration.controller.clone(),
                        cid: registration.cid,
                        rank,
                        is_assigned: registration.is_assigned,
                    })
                })
                .sorted_by(|a, b| {
                    a.rank
                        .cmp(&b.rank)
                        .then_with(|| a.controller.cmp(&b.controller))
                })
                .collect();
            (category, entries)
        })
        .collect()
}

#[derive(Debug, Default)]
struct UpdatedEventData {
    name: String,
    description: String,
    published: bool,
    banner: String,
    start: String,
    end: String,
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
            _ => {}
        }
    }

    let start = js_timestamp_to_utc(&event.start, "UTC")?;
    let end = js_timestamp_to_utc(&event.end, "UTC")?;

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
        sqlx::query(sql::DELETE_EVENT_CIC_POSITIONS_FOR)
            .bind(id)
            .execute(&mut *tx)
            .await?;
        sqlx::query(sql::DELETE_EVENT_ENROUTE_SECTOR_SPLIT)
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
    choice_1: String,
    choice_2: String,
    choice_3: String,
    notes: String,
}

/// Create or update a controller's registration for an event.
async fn upsert_registration(
    db: &Pool<Sqlite>,
    event_id: u32,
    cid: u32,
    choices: &[RegistrationChoice; 3],
    notes: &str,
) -> sqlx::Result<()> {
    let [c_1, c_2, c_3] = choices;
    sqlx::query(sql::UPSERT_EVENT_REGISTRATION)
        .bind(event_id)
        .bind(cid)
        .bind(c_1.position())
        .bind(c_2.position())
        .bind(c_3.position())
        .bind(notes)
        .bind(c_1.any_category())
        .bind(c_2.any_category())
        .bind(c_3.any_category())
        .execute(db)
        .await?;
    Ok(())
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

    let positions: Vec<EventPosition> = sqlx::query_as(sql::GET_EVENT_POSITIONS)
        .bind(id)
        .fetch_all(&state.db)
        .await?;
    let choices = [
        RegistrationChoice::parse(&register_data.choice_1, &positions),
        RegistrationChoice::parse(&register_data.choice_2, &positions),
        RegistrationChoice::parse(&register_data.choice_3, &positions),
    ];

    // upsert the registration
    let notes = if register_data.notes.len() > 500 {
        &register_data.notes[0..500]
    } else {
        &register_data.notes
    };
    upsert_registration(&state.db, id, cid, &choices, notes).await?;
    let [c_1, c_2, c_3] = &choices;
    record_log(
        format!("{cid} registered for event {id}: {c_1} {c_2} {c_3}"),
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
    sqlx::query(sql::CLEAR_CID_FROM_EVENT_CIC_POSITIONS)
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

#[derive(Deserialize)]
struct AddCicPositionForm {
    category: String,
}

/// Add a new CIC position to the event.
async fn post_add_cic_position(
    State(state): State<Arc<AppState>>,
    session: Session,
    Path(id): Path<u32>,
    Form(new_cic_position_data): Form<AddCicPositionForm>,
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

    let category = new_cic_position_data.category.trim();
    if category.is_empty() || category.chars().count() > CIC_CATEGORY_MAX_LENGTH {
        flashed_messages::push_flashed_message(
            session,
            flashed_messages::MessageLevel::Error,
            &format!("CIC category must be 1-{CIC_CATEGORY_MAX_LENGTH} characters"),
        )
        .await?;
        return Ok(Redirect::to(&format!("/events/{id}")));
    }

    // don't allow category duplicates
    let existing: Vec<EventCicPosition> = sqlx::query_as(sql::GET_EVENT_CIC_POSITIONS)
        .bind(id)
        .fetch_all(&state.db)
        .await?;
    if let Some(duplicate) = existing
        .iter()
        .find(|position| position.category.eq_ignore_ascii_case(category))
    {
        flashed_messages::push_flashed_message(
            session,
            flashed_messages::MessageLevel::Error,
            &format!("There's already a CIC position for {}", duplicate.category),
        )
        .await?;
        return Ok(Redirect::to(&format!("/events/{id}")));
    }

    sqlx::query(sql::INSERT_EVENT_CIC_POSITION)
        .bind(id)
        .bind(category)
        .execute(&state.db)
        .await?;
    record_log(
        format!(
            "{} added {category} CIC position to event {id}",
            user_info.unwrap().cid
        ),
        &state.db,
        true,
    )
    .await?;
    Ok(Redirect::to(&format!("/events/{id}")))
}

/// Delete a CIC position from the event.
async fn post_delete_cic_position(
    State(state): State<Arc<AppState>>,
    session: Session,
    Path((id, pos_id)): Path<(u32, u32)>,
) -> Result<Redirect, AppError> {
    let user_info: Option<UserInfo> = session.get(SESSION_USER_INFO_KEY).await?;
    if let Some(redirect) = reject_if_not_in(&state, &user_info, PermissionsGroup::EventsTeam).await
    {
        return Ok(redirect);
    }
    let result = sqlx::query(sql::DELETE_EVENT_CIC_POSITION)
        .bind(id)
        .bind(pos_id)
        .execute(&state.db)
        .await?;
    if result.rows_affected() == 0 {
        return Ok(Redirect::to(&format!("/events/{id}")));
    }
    record_log(
        format!(
            "{} removed CIC position {pos_id} from event {id}",
            user_info.unwrap().cid
        ),
        &state.db,
        true,
    )
    .await?;
    flashed_messages::push_flashed_message(
        session,
        flashed_messages::MessageLevel::Info,
        "CIC position deleted",
    )
    .await?;
    Ok(Redirect::to(&format!("/events/{id}")))
}

#[derive(Deserialize)]
struct SetCicPositionForm {
    position_id: u32,
    controller: u32,
}

/// Set a registered controller (or no-one) as a CIC.
async fn post_set_cic_position(
    State(state): State<Arc<AppState>>,
    session: Session,
    Path(id): Path<u32>,
    Form(cic_position_data): Form<SetCicPositionForm>,
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

    let cid = (cic_position_data.controller != 0).then_some(cic_position_data.controller);
    if let Some(cid) = cid {
        let registration: Option<EventRegistration> =
            sqlx::query_as(sql::GET_EVENT_REGISTRATION_FOR)
                .bind(id)
                .bind(cid)
                .fetch_optional(&state.db)
                .await?;
        if registration.is_none() {
            flashed_messages::push_flashed_message(
                session,
                flashed_messages::MessageLevel::Error,
                "Only controllers registered for this event can be a CIC",
            )
            .await?;
            return Ok(Redirect::to(&format!("/events/{id}")));
        }
    }
    sqlx::query(sql::UPDATE_EVENT_CIC_POSITION_CONTROLLER)
        .bind(id)
        .bind(cic_position_data.position_id)
        .bind(cid)
        .execute(&state.db)
        .await?;
    record_log(
        format!(
            "{} updated event {id} CIC position {} to cid {}",
            user_info.unwrap().cid,
            cic_position_data.position_id,
            cid.unwrap_or_default()
        ),
        &state.db,
        true,
    )
    .await?;
    Ok(Redirect::to(&format!("/events/{id}")))
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

#[derive(Debug, Deserialize)]
struct AssignEventEnrouteSectorSplitForm {
    split_id: u32,
    split_name: String,
}

async fn post_assign_event_enroute_sector_split(
    State(state): State<Arc<AppState>>,
    session: Session,
    Path(id): Path<u32>,
    Form(form): Form<AssignEventEnrouteSectorSplitForm>,
) -> Result<Redirect, AppError> {
    let user_info: Option<UserInfo> = session.get(SESSION_USER_INFO_KEY).await?;
    if let Some(redirect) = reject_if_not_in(&state, &user_info, PermissionsGroup::EventsTeam).await
    {
        return Ok(redirect);
    }

    sqlx::query(sql::UPSERT_EVENT_ENROUTE_SECTOR_SPLIT)
        .bind(id)
        .bind(form.split_id)
        .execute(&state.db)
        .await?;

    let cid = user_info
        .map(|i| i.cid.to_string())
        .unwrap_or("UNKNOWN_CID".to_string());
    record_log(
        format!(
            "{} assigned enroute sector split {} to event {}",
            cid, form.split_name, id
        ),
        &state.db,
        true,
    )
    .await?;

    flashed_messages::push_flashed_message(
        session,
        flashed_messages::MessageLevel::Info,
        &format!(
            "Assigned enroute sector split {} to event {}",
            form.split_name, id
        ),
    )
    .await?;
    Ok(Redirect::to(&format!("/events/{id}")))
}

async fn post_remove_event_enroute_sector_split(
    State(state): State<Arc<AppState>>,
    session: Session,
    Path(id): Path<u32>,
) -> Result<Redirect, AppError> {
    let user_info: Option<UserInfo> = session.get(SESSION_USER_INFO_KEY).await?;
    if let Some(redirect) = reject_if_not_in(&state, &user_info, PermissionsGroup::EventsTeam).await
    {
        return Ok(redirect);
    }

    sqlx::query(sql::DELETE_EVENT_ENROUTE_SECTOR_SPLIT)
        .bind(id)
        .execute(&state.db)
        .await?;

    let cid = user_info
        .map(|i| i.cid.to_string())
        .unwrap_or("UNKNOWN_CID".to_string());
    record_log(
        format!(
            "{} removed enroute sector split assignment from event {id}",
            cid
        ),
        &state.db,
        true,
    )
    .await?;

    flashed_messages::push_flashed_message(
        session,
        flashed_messages::MessageLevel::Info,
        "Removed enroute sector split assignment",
    )
    .await?;
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
        .route("/events/{id}/set_cic_position", post(post_set_cic_position))
        .route("/events/{id}/add_cic_position", post(post_add_cic_position))
        .route(
            "/events/{id}/delete_cic_position/{pos_id}",
            post(post_delete_cic_position),
        )
        .route("/events/{id}/no_show", post(post_no_show))
        .route(
            "/events/{id}/assign_enroute_sector_split",
            post(post_assign_event_enroute_sector_split),
        )
        .route(
            "/events/{id}/remove_enroute_sector_split",
            post(post_remove_event_enroute_sector_split),
        )
}

#[cfg(test)]
mod tests {
    use super::{RegistrationChoice::*, *};
    use sqlx::{Executor, sqlite::SqlitePoolOptions};

    async fn test_db() -> Pool<Sqlite> {
        let db = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        db.execute(sql::CREATE_TABLES).await.unwrap();
        for cid in [1, 2] {
            sqlx::query(sql::INSERT_USER_SIMPLE)
                .bind(cid)
                .bind("First")
                .bind(format!("Last{cid}"))
                .bind(1)
                .bind("ZDV")
                .execute(&db)
                .await
                .unwrap();
        }
        db
    }

    /// Create an event with the default CIC positions, like `post_new_event_form` does.
    async fn create_event(db: &Pool<Sqlite>) -> u32 {
        let result = sqlx::query(sql::CREATE_EVENT)
            .bind(1)
            .bind("Test event")
            .bind(Utc::now())
            .bind(Utc::now())
            .bind("")
            .bind("")
            .execute(db)
            .await
            .unwrap();
        let id = result.last_insert_rowid() as u32;
        sqlx::query(sql::INSERT_DEFAULT_EVENT_CIC_POSITIONS)
            .bind(id)
            .execute(db)
            .await
            .unwrap();
        id
    }

    async fn register(db: &Pool<Sqlite>, event_id: u32, cid: u32) {
        register_with(db, event_id, cid, [Empty, Empty, Empty]).await;
    }

    async fn register_with(
        db: &Pool<Sqlite>,
        event_id: u32,
        cid: u32,
        choices: [RegistrationChoice; 3],
    ) {
        upsert_registration(db, event_id, cid, &choices, "")
            .await
            .unwrap();
    }

    async fn add_position(db: &Pool<Sqlite>, event_id: u32, name: &str, category: &str) -> u32 {
        sqlx::query(sql::INSERT_EVENT_POSITION)
            .bind(event_id)
            .bind(name)
            .bind(category)
            .execute(db)
            .await
            .unwrap()
            .last_insert_rowid() as u32
    }

    async fn event_positions(db: &Pool<Sqlite>, event_id: u32) -> Vec<EventPosition> {
        sqlx::query_as(sql::GET_EVENT_POSITIONS)
            .bind(event_id)
            .fetch_all(db)
            .await
            .unwrap()
    }

    async fn set_cic(db: &Pool<Sqlite>, event_id: u32, position_id: u32, cid: Option<u32>) {
        sqlx::query(sql::UPDATE_EVENT_CIC_POSITION_CONTROLLER)
            .bind(event_id)
            .bind(position_id)
            .bind(cid)
            .execute(db)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn test_new_event_default_cic_positions() {
        let db = test_db().await;
        let event_id = create_event(&db).await;
        let positions = event_cic_positions_extra(event_id, &db).await.unwrap();
        let categories: Vec<_> = positions.iter().map(|p| p.category.as_str()).collect();
        assert_eq!(categories, ["Enroute", "TRACON", "CAB"]);
        assert!(
            positions
                .iter()
                .all(|p| p.cid.is_none() && p.controller == "unassigned")
        );
    }

    #[tokio::test]
    async fn test_cic_position_assignment() {
        let db = test_db().await;
        let event_id = create_event(&db).await;
        register(&db, event_id, 1).await;
        register(&db, event_id, 2).await;
        let position_id = event_cic_positions_extra(event_id, &db).await.unwrap()[0].id;
        set_cic(&db, event_id, position_id, Some(2)).await;

        let positions = event_cic_positions_extra(event_id, &db).await.unwrap();
        assert_eq!(positions[0].cid, Some(2));
        assert_eq!(positions[0].controller, "First Last2 (??)");

        // a CIC counts as assigned in the sign-ups list
        let registrations = event_registrations_extra(event_id, &[], &positions, &db)
            .await
            .unwrap();
        let assigned: Vec<_> = registrations
            .iter()
            .map(|r| (r.cid, r.is_assigned))
            .sorted()
            .collect();
        assert_eq!(assigned, [(1, false), (2, true)]);

        // unregistering clears the controller from the event's CIC positions
        sqlx::query(sql::CLEAR_CID_FROM_EVENT_CIC_POSITIONS)
            .bind(event_id)
            .bind(2)
            .execute(&db)
            .await
            .unwrap();
        let positions = event_cic_positions_extra(event_id, &db).await.unwrap();
        assert_eq!(positions[0].cid, None);
        assert_eq!(positions[0].controller, "unassigned");
    }

    #[tokio::test]
    async fn test_cic_position_changes_scoped_to_event() {
        let db = test_db().await;
        let event_1 = create_event(&db).await;
        let event_2 = create_event(&db).await;
        register(&db, event_2, 1).await;
        let other_position = event_cic_positions_extra(event_2, &db).await.unwrap()[0].id;

        // changes through the wrong event don't touch the other event's positions
        set_cic(&db, event_1, other_position, Some(1)).await;
        sqlx::query(sql::DELETE_EVENT_CIC_POSITION)
            .bind(event_1)
            .bind(other_position)
            .execute(&db)
            .await
            .unwrap();
        let positions = event_cic_positions_extra(event_2, &db).await.unwrap();
        assert_eq!(positions.len(), 3);
        assert_eq!(positions[0].cid, None);

        sqlx::query(sql::DELETE_EVENT_CIC_POSITIONS_FOR)
            .bind(event_1)
            .execute(&db)
            .await
            .unwrap();
        assert!(
            event_cic_positions_extra(event_1, &db)
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            event_cic_positions_extra(event_2, &db).await.unwrap().len(),
            3
        );
    }

    #[tokio::test]
    async fn test_registration_choice_parse() {
        let db = test_db().await;
        let event_1 = create_event(&db).await;
        let event_2 = create_event(&db).await;
        let position = add_position(&db, event_1, "DEN_APP", "TRACON").await;
        let other_event_position = add_position(&db, event_2, "DEN_TWR", "Local").await;
        let positions = event_positions(&db, event_1).await;

        let parse = |value: &str| RegistrationChoice::parse(value, &positions);
        assert_eq!(parse("0"), Empty);
        assert_eq!(parse(""), Empty);
        assert_eq!(parse("junk"), Empty);
        assert_eq!(parse(&position.to_string()), Position(position));
        assert_eq!(parse(&other_event_position.to_string()), Empty);
        assert_eq!(parse("any:Enroute"), Any("Enroute"));
        assert_eq!(parse("any:TRACON"), Any("TRACON"));
        assert_eq!(parse("any:Local"), Any("Local"));
        assert_eq!(parse("any:tracon"), Empty);
        assert_eq!(parse("any:CAB"), Empty);
    }

    #[tokio::test]
    async fn test_registration_choice_slot_holds_one_kind() {
        let db = test_db().await;
        let event_id = create_event(&db).await;
        let position = add_position(&db, event_id, "DEN_APP", "TRACON").await;
        let get = || async {
            sqlx::query_as::<_, EventRegistration>(sql::GET_EVENT_REGISTRATION_FOR)
                .bind(event_id)
                .bind(1)
                .fetch_one(&db)
                .await
                .unwrap()
        };

        register_with(&db, event_id, 1, [Any("TRACON"), Position(position), Empty]).await;
        let registration = get().await;
        assert_eq!(registration.choice_1, 0);
        assert_eq!(registration.choice_1_any.as_deref(), Some("TRACON"));
        assert_eq!(registration.choice_2, position);
        assert_eq!(registration.choice_2_any, None);
        assert_eq!(registration.choice_3_any, None);

        // updating swaps the kinds without leaving the old value behind
        register_with(&db, event_id, 1, [Position(position), Any("Local"), Empty]).await;
        let registration = get().await;
        assert_eq!(registration.choice_1, position);
        assert_eq!(registration.choice_1_any, None);
        assert_eq!(registration.choice_2, 0);
        assert_eq!(registration.choice_2_any.as_deref(), Some("Local"));
    }

    #[tokio::test]
    async fn test_any_category_pools() {
        let db = test_db().await;
        let event_id = create_event(&db).await;
        let enroute = add_position(&db, event_id, "DEN_01_CTR", "Enroute").await;
        register_with(&db, event_id, 1, [Position(enroute), Any("TRACON"), Empty]).await;
        register_with(&db, event_id, 2, [Any("TRACON"), Empty, Any("Local")]).await;
        sqlx::query(sql::UPDATE_EVENT_POSITION_CONTROLLER)
            .bind(enroute)
            .bind(1)
            .execute(&db)
            .await
            .unwrap();

        let positions = event_positions(&db, event_id).await;
        let registrations = event_registrations_extra(event_id, &positions, &[], &db)
            .await
            .unwrap()
            .into_iter()
            .sorted_by_key(|r| r.cid)
            .collect::<Vec<_>>();
        let choices: Vec<_> = registrations
            .iter()
            .map(|r| [r.choice_1.as_str(), &r.choice_2, &r.choice_3])
            .collect();
        assert_eq!(
            choices,
            [
                ["DEN_01_CTR", "Any TRACON", ""],
                ["Any TRACON", "", "Any Local"]
            ]
        );

        let pools = any_category_pools(&registrations);
        let pool = |category| -> Vec<_> {
            pools[category]
                .iter()
                .map(|e| (e.cid, e.rank, e.is_assigned))
                .collect()
        };
        assert_eq!(pool("Enroute"), []);
        // ordered by rank, and controllers already on a position are marked
        assert_eq!(pool("TRACON"), [(2, 1, false), (1, 2, true)]);
        assert_eq!(pool("Local"), [(2, 3, false)]);
    }
}
