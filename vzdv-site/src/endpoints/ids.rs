//! Endpoints for the integrated IDS.

use crate::{
    flashed_messages,
    shared::{AppError, AppState, SESSION_USER_INFO_KEY, UserInfo, reject_if_not_in},
};
use axum::{
    Router,
    extract::{Json as JsonE, State},
    response::{Html, IntoResponse, Json as JsonR, Response},
    routing::{get, post},
};
use log::{debug, error};
use minijinja::context;
use reqwest::StatusCode;
use serde::Serialize;
use std::{collections::HashMap, sync::Arc};
use tower_sessions::Session;
use vzdv::aviation::{AirportWeather, WeatherConditions};
use vzdv::ids::AirportProcedure;
use vzdv::sql::{self, Atis};

/// Receive HTTP POST events from vATIS being ran by facility controllers.
///
/// Note that there doesn't seem to be a way to _authenticate_ that the data
/// is actually coming from vATIS ....
async fn receive_vatis_post(
    State(state): State<Arc<AppState>>,
    JsonE(payload): JsonE<Atis>,
) -> Result<StatusCode, AppError> {
    let existing: Vec<Atis> = sqlx::query_as(sql::GET_ALL_ATIS_ENTRIES)
        .fetch_all(&state.db)
        .await?;
    let matching: Vec<_> = existing
        .iter()
        .filter(|entry| entry.facility == payload.facility && entry.atis_type == payload.atis_type)
        .map(|entry| entry.id)
        .collect();
    // can't use `.for_each` because of async
    for index in matching {
        if let Err(e) = sqlx::query(sql::DELETE_ATIS_ENTRY)
            .bind(index)
            .execute(&state.db)
            .await
        {
            error!("Could not delete matching ATIS {index}: {e}");
        }
    }
    sqlx::query(sql::INSERT_ATIS_ENTRY)
        .bind(&payload.facility)
        .bind(&payload.preset)
        .bind(&payload.atis_letter)
        .bind(&payload.atis_type)
        .bind(&payload.airport_conditions)
        .bind(&payload.notams)
        .bind(payload.timestamp)
        .bind(&payload.version)
        .execute(&state.db)
        .await?;
    debug!("New ATIS data stored");
    Ok(StatusCode::OK)
}

async fn show_atis_data(
    State(state): State<Arc<AppState>>,
    session: Session,
) -> Result<Response, AppError> {
    let user_info: Option<UserInfo> = session.get(SESSION_USER_INFO_KEY).await?;
    if let Some(redirect) =
        reject_if_not_in(&state, &user_info, vzdv::PermissionsGroup::LoggedIn).await
    {
        return Ok(redirect.into_response());
    }
    let data: Vec<Atis> = sqlx::query_as(sql::GET_ALL_ATIS_ENTRIES)
        .fetch_all(&state.db)
        .await?;
    Ok(JsonR(data).into_response())
}

/// A single row in the IDS display table.
#[derive(Debug, Serialize)]
struct IdsRow {
    icao: String,
    dep_rwys: String,
    arr_rwys: String,
    /// For combined airports, the single flow name.
    flow_name: Option<String>,
    /// For split airports, the departure ATIS preset.
    dep_name: Option<String>,
    /// For split airports, the arrival ATIS preset.
    arr_name: Option<String>,
    is_split: bool,
    atis_info: String,
    conditions: Option<String>,
    wind: Option<String>,
    raw_metar: Option<String>,
    error: Option<String>,
}

/// Build a weather map keyed by ICAO (e.g., "KDEN").
fn weather_by_icao(weather: &[AirportWeather]) -> HashMap<String, &AirportWeather> {
    weather
        .iter()
        .map(|w| (format!("K{}", w.name), w))
        .collect()
}

/// Determine if a flow can be resolved for this airport without weather data.
fn can_determine_without_weather(
    procedure: &AirportProcedure,
    atis_list: &[Atis],
    icao: &str,
) -> bool {
    let airport_atis: Vec<_> = atis_list.iter().filter(|a| a.facility == icao).collect();
    match procedure {
        AirportProcedure::Combined(_) => airport_atis.iter().any(|a| a.atis_type == "combined"),
        AirportProcedure::Split(_) => {
            airport_atis.iter().any(|a| a.atis_type == "departure")
                && airport_atis.iter().any(|a| a.atis_type == "arrival")
        }
    }
}

/// Build a fallback weather struct when no METAR is available but ATIS is sufficient.
fn fallback_weather(icao: &str) -> AirportWeather {
    AirportWeather {
        ceiling: 3456,
        conditions: WeatherConditions::VFR,
        name: icao.strip_prefix('K').unwrap_or(icao).to_string(),
        raw: "No METAR available".to_string(),
        visibility: 10,
        wind: (0, 0, 0),
    }
}

/// Build a formatted wind string from an `AirportWeather`.
fn format_wind(weather: &AirportWeather) -> String {
    let (dir, mag, gust) = weather.wind;
    if gust > 0 {
        format!("{:03}@{mag}G{gust}", dir)
    } else {
        format!("{:03}@{mag}", dir)
    }
}

/// Construct a single row for the IDS table.
fn build_ids_row(
    icao: &str,
    procedure: &AirportProcedure,
    weather: Option<&AirportWeather>,
    atis_list: &[Atis],
) -> IdsRow {
    let airport_atis: Vec<&Atis> = atis_list.iter().filter(|a| a.facility == icao).collect();

    let atis_info = if airport_atis.is_empty() {
        "No ATIS".to_string()
    } else {
        airport_atis
            .iter()
            .map(|a| format!("{} {}", a.atis_type.to_uppercase(), a.atis_letter))
            .collect::<Vec<_>>()
            .join(", ")
    };

    let conditions = weather.map(|w| format!("{:?}", w.conditions));
    let wind = weather.map(format_wind);
    let raw_metar = weather.map(|w| w.raw.clone());

    let is_split = matches!(procedure, AirportProcedure::Split(_));

    let (resolved, error) = match weather {
        Some(w) => (procedure.determine_flow(w, atis_list).ok(), None),
        None => {
            if can_determine_without_weather(procedure, atis_list, icao) {
                let fallback = fallback_weather(icao);
                (procedure.determine_flow(&fallback, atis_list).ok(), None)
            } else {
                (None, Some("No METAR or complete ATIS data".to_string()))
            }
        }
    };

    let mut row = IdsRow {
        icao: icao.to_string(),
        dep_rwys: String::new(),
        arr_rwys: String::new(),
        flow_name: None,
        dep_name: None,
        arr_name: None,
        is_split,
        atis_info,
        conditions,
        wind,
        raw_metar,
        error,
    };

    if let Some(flow) = resolved {
        row.dep_rwys = flow.dep_rwys.join(", ");
        row.arr_rwys = flow.arr_rwys.join(", ");
        row.flow_name = flow.dep_name.clone();
        row.dep_name = flow.dep_name;
        row.arr_name = flow.arr_name;
    } else if row.error.is_none() {
        row.error = Some("Could not determine flow".to_string());
    }

    row
}

/// Show the base IDS page.
async fn page_home(
    State(state): State<Arc<AppState>>,
    session: Session,
) -> Result<Response, AppError> {
    let user_info: Option<UserInfo> = session.get(SESSION_USER_INFO_KEY).await?;
    if let Some(redirect) =
        reject_if_not_in(&state, &user_info, vzdv::PermissionsGroup::LoggedIn).await
    {
        return Ok(redirect.into_response());
    }
    let template = state.templates.get_template("ids/base.jinja")?;
    let flashed_messages = flashed_messages::drain_flashed_messages(session).await?;
    let atis: Vec<Atis> = sqlx::query_as(sql::GET_ALL_ATIS_ENTRIES)
        .fetch_all(&state.db)
        .await?;

    let weather = match crate::shared::get_all_weather(&state).await {
        Ok(w) => w,
        Err(e) => {
            error!("Could not fetch weather for IDS page: {e}");
            Vec::new()
        }
    };
    let weather_map = weather_by_icao(&weather);

    let mut rows: Vec<IdsRow> = state
        .ids_config
        .0
        .iter()
        .map(|(icao, procedure)| {
            let weather = weather_map.get(icao).copied();
            build_ids_row(icao, procedure, weather, &atis)
        })
        .collect();
    rows.sort_by(|a, b| a.icao.cmp(&b.icao));

    let rendered = template.render(context! { user_info, flashed_messages, rows })?;
    Ok(Html(rendered).into_response())
}

/// This file's routes and templates.
pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/ids", get(page_home))
        .route("/ids/vatis/submit", post(receive_vatis_post))
        .route("/ids/vatis/current", get(show_atis_data))
}
