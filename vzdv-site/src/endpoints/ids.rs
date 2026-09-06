//! Endpoints for the integrated IDS.

use crate::{
    flashed_messages,
    shared::{AppError, AppState, SESSION_USER_INFO_KEY, UserInfo},
};
use axum::{
    Router,
    extract::{Json as JsonE, Path, State},
    response::{Html, IntoResponse, Json as JsonR, Redirect, Response},
    routing::{get, post},
};
use chrono::{DateTime, Utc};
use log::{debug, error};
use minijinja::context;
use reqwest::StatusCode;
use serde::Serialize;
use serde_json::json;
use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::Mutex;
use tower_sessions::Session;
use vzdv::aviation::{AirportWeather, WeatherConditions};
use vzdv::ids::{AirportProcedure, DepCorridor, ResolvedFlow, sorted_rwy_names};
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
    if let Err(status) = roster_access(&user_info) {
        return Ok(access_denied(status));
    }
    let data: Vec<Atis> = sqlx::query_as(sql::GET_ALL_ATIS_ENTRIES)
        .fetch_all(&state.db)
        .await?;
    Ok(JsonR(data).into_response())
}

/// A single row in the IDS display table.
#[derive(Debug, Clone, Serialize)]
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
    altimeter: Option<String>,
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
        altimeter: None,
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
    current_flows: &HashMap<String, ResolvedFlow>,
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
    let altimeter = weather.and_then(|w| w.altimeter).map(|a| format!("{a:.2}"));
    let raw_metar = weather.map(|w| w.raw.clone());

    let is_split = matches!(procedure, AirportProcedure::Split(_));

    let (resolved, error) = match weather {
        Some(w) => (
            procedure
                .determine_flow_with_matches(w, atis_list, current_flows)
                .ok(),
            None,
        ),
        None => {
            if can_determine_without_weather(procedure, atis_list, icao) {
                let fallback = fallback_weather(icao);
                (
                    procedure
                        .determine_flow_with_matches(&fallback, atis_list, current_flows)
                        .ok(),
                    None,
                )
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
        altimeter,
        raw_metar,
        error,
    };

    if let Some(flow) = resolved {
        row.dep_rwys = sorted_rwy_names(&flow.dep_rwys).join(", ");
        row.arr_rwys = sorted_rwy_names(&flow.arr_rwys).join(", ");
        row.flow_name = flow.dep_name.clone();
        row.dep_name = flow.dep_name;
        row.arr_name = flow.arr_name;
    } else if row.error.is_none() {
        row.error = Some("Could not determine flow".to_string());
    }

    row
}

/// A single row in the departure detail table.
#[derive(Debug, Clone, Serialize)]
struct DepRow {
    corridor: String,
    /// Departure corridor direction.
    direction: String,
    /// Runway for this corridor.
    rwy: String,
    /// Gate names assigned to this corridor.
    gates: Vec<String>,
}

/// A single row in the arrival detail table.
#[derive(Debug, Clone, Serialize)]
struct ArrRow {
    /// Runway name to display in the first cell (empty string for continuation rows).
    rwy_label: String,
    /// Number of rows this runway spans.
    rwy_rowspan: usize,
    /// Gate names assigned to this runway.
    gates: Vec<String>,
}

/// Per-airport detail data passed to the template.
#[derive(Debug, Clone, Serialize)]
struct AirportDetail {
    icao: String,
    dep_flow: Option<String>,
    arr_flow: Option<String>,
    suggested_flow: Option<String>,
    dep_rows: Vec<DepRow>,
    arr_rows: Vec<ArrRow>,
    error: Option<String>,
}

/// Build the departure rows by resolving each corridor name against the airport's
/// `dep_corridors` definition.
fn build_dep_rows(
    dep_rwys: &HashMap<String, Vec<String>>,
    dep_corridors: &HashMap<String, DepCorridor>,
) -> anyhow::Result<Vec<DepRow>> {
    let mut rows = Vec::new();
    for (rwy, corridors) in dep_rwys {
        for corridor in corridors {
            let Some(info) = dep_corridors.get(corridor) else {
                anyhow::bail!("Could not determine departure corridor for corridor {corridor}");
            };
            rows.push(DepRow {
                corridor: corridor.clone(),
                direction: info.direction.clone(),
                rwy: rwy.clone(),
                gates: info.gates.clone(),
            });
        }
    }
    rows.sort_by(|a, b| {
        a.direction
            .cmp(&b.direction)
            .then_with(|| a.rwy.cmp(&b.rwy))
            .then_with(|| a.corridor.cmp(&b.corridor))
    });
    Ok(rows)
}

/// Build the arrival runway→gate rows for one side of the flow.
fn build_arr_rows(rwy_map: &HashMap<String, Vec<String>>) -> Vec<ArrRow> {
    let mut rows = Vec::new();
    let mut rwys: Vec<String> = rwy_map.keys().cloned().collect();
    rwys.sort();
    for rwy in rwys {
        let gates = rwy_map.get(&rwy).cloned().unwrap_or_default();
        rows.push(ArrRow {
            rwy_label: rwy.clone(),
            rwy_rowspan: 1,
            gates,
        });
    }
    rows
}

fn build_airport_detail(
    icao: &str,
    procedure: &AirportProcedure,
    weather: Option<&AirportWeather>,
    atis: &[Atis],
    current_flows: &HashMap<String, ResolvedFlow>,
) -> AirportDetail {
    let (resolved, error) = match weather {
        Some(w) => (
            procedure
                .determine_flow_with_matches(w, atis, current_flows)
                .ok(),
            None,
        ),
        None if can_determine_without_weather(procedure, atis, icao) => (
            procedure
                .determine_flow_with_matches(&fallback_weather(icao), atis, current_flows)
                .ok(),
            None,
        ),
        None => (None, Some("No METAR or complete ATIS data".to_string())),
    };
    let suggested_flow = weather.and_then(|w| procedure.suggest_flow(w));
    match resolved {
        Some(flow) => {
            let (dep_rows, dep_error) = match procedure {
                AirportProcedure::Split(proc) => {
                    match build_dep_rows(&flow.dep_rwys, &proc.dep_corridors) {
                        Ok(rows) => (rows, None),
                        Err(e) => (vec![], Some(e.to_string())),
                    }
                }
                AirportProcedure::Combined(_) => (vec![], None),
            };
            AirportDetail {
                icao: icao.to_string(),
                dep_flow: flow.dep_name,
                arr_flow: flow.arr_name,
                suggested_flow,
                dep_rows,
                arr_rows: build_arr_rows(&flow.arr_rwys),
                error: dep_error,
            }
        }
        None => AirportDetail {
            icao: icao.to_string(),
            dep_flow: None,
            arr_flow: None,
            suggested_flow,
            dep_rows: vec![],
            arr_rows: vec![],
            error: error.or_else(|| Some("Could not determine flow".to_string())),
        },
    }
}

const REFRESH_INTERVAL: Duration = Duration::from_secs(60);

#[derive(Default)]
pub struct IdsCache {
    current: Mutex<Option<CachedSnapshot>>,
}

struct CachedSnapshot {
    inserted: Instant,
    snapshot: Arc<IdsSnapshot>,
    weather: Vec<AirportWeather>,
}

#[derive(Clone)]
struct IdsSnapshot {
    updated_at: DateTime<Utc>,
    warning: Option<String>,
    rows: Vec<IdsRow>,
    airports: HashMap<String, AirportDetail>,
}

fn resolve_snapshot_flow(
    icao: &str,
    procedure: &AirportProcedure,
    weather: Option<&AirportWeather>,
    atis: &[Atis],
    current_flows: &HashMap<String, ResolvedFlow>,
) -> Option<ResolvedFlow> {
    match weather {
        Some(weather) => procedure
            .determine_flow_with_matches(weather, atis, current_flows)
            .ok(),
        None if can_determine_without_weather(procedure, atis, icao) => procedure
            .determine_flow_with_matches(&fallback_weather(icao), atis, current_flows)
            .ok(),
        None => None,
    }
}

fn build_snapshot(
    config: &vzdv::config::ConfigIDS,
    weather: &[AirportWeather],
    atis: &[Atis],
    warning: Option<String>,
) -> IdsSnapshot {
    let weather_map = weather_by_icao(weather);
    let mut current_flows = HashMap::new();
    for use_try_match in [false, true] {
        for (icao, procedure) in &config.0 {
            let has_try_match = matches!(
                procedure,
                AirportProcedure::Combined(proc) if proc.try_match.is_some()
            );
            if has_try_match != use_try_match {
                continue;
            }
            if let Some(flow) = resolve_snapshot_flow(
                icao,
                procedure,
                weather_map.get(icao).copied(),
                atis,
                &current_flows,
            ) {
                current_flows.insert(icao.clone(), flow);
            }
        }
    }

    let mut rows = Vec::new();
    let mut airports = HashMap::new();
    for (icao, procedure) in &config.0 {
        let weather = weather_map.get(icao).copied();
        rows.push(build_ids_row(
            icao,
            procedure,
            weather,
            atis,
            &current_flows,
        ));
        airports.insert(
            icao.clone(),
            build_airport_detail(icao, procedure, weather, atis, &current_flows),
        );
    }
    rows.sort_by(|a, b| a.icao.cmp(&b.icao));
    IdsSnapshot {
        updated_at: Utc::now(),
        warning,
        rows,
        airports,
    }
}

async fn get_snapshot(state: &AppState) -> Result<Arc<IdsSnapshot>, AppError> {
    let mut cached = state.ids_cache.current.lock().await;
    if let Some(entry) = cached.as_ref()
        && entry.inserted.elapsed() < REFRESH_INTERVAL
    {
        return Ok(entry.snapshot.clone());
    }

    let atis: Vec<Atis> = match sqlx::query_as(sql::GET_ALL_ATIS_ENTRIES)
        .fetch_all(&state.db)
        .await
    {
        Ok(atis) => atis,
        Err(e) => {
            let Some(entry) = cached.as_mut() else {
                return Err(e.into());
            };
            error!("Could not refresh IDS ATIS data: {e}");
            let mut snapshot = (*entry.snapshot).clone();
            snapshot.warning = Some(
                "IDS could not be refreshed; showing the last available snapshot.".to_string(),
            );
            entry.snapshot = Arc::new(snapshot);
            entry.inserted = Instant::now();
            return Ok(entry.snapshot.clone());
        }
    };
    let weather_result = tokio::time::timeout(
        Duration::from_secs(10),
        crate::shared::get_all_weather(state),
    )
    .await
    .map_err(anyhow::Error::from)
    .and_then(|result| result);
    let (weather, warning) = match weather_result {
        Ok(weather) => (weather, None),
        Err(e) => {
            error!("Could not refresh IDS weather: {e}");
            let weather = cached
                .as_mut()
                .map(|entry| std::mem::take(&mut entry.weather))
                .unwrap_or_default();
            let warning = if weather.is_empty() {
                "Weather unavailable; using ATIS where possible."
            } else {
                "Weather could not be refreshed; runway estimates use the last available weather."
            };
            (weather, Some(warning.to_string()))
        }
    };
    let snapshot = Arc::new(build_snapshot(&state.ids_config, &weather, &atis, warning));
    *cached = Some(CachedSnapshot {
        inserted: Instant::now(),
        snapshot: snapshot.clone(),
        weather,
    });
    Ok(snapshot)
}

pub async fn refresh_snapshots(state: Arc<AppState>) {
    loop {
        if let Err(e) = get_snapshot(&state).await {
            error!("Could not refresh IDS snapshot: {e}");
        }
        tokio::time::sleep(REFRESH_INTERVAL).await;
    }
}

fn roster_access(user_info: &Option<UserInfo>) -> Result<(), StatusCode> {
    match user_info {
        Some(user) if user.on_roster => Ok(()),
        Some(_) => Err(StatusCode::FORBIDDEN),
        None => Err(StatusCode::UNAUTHORIZED),
    }
}

fn access_denied(status: StatusCode) -> Response {
    (
        status,
        JsonR(json!({"error": "IDS access requires an active roster membership."})),
    )
        .into_response()
}

async fn data_home(
    State(state): State<Arc<AppState>>,
    session: Session,
) -> Result<Response, AppError> {
    let user_info: Option<UserInfo> = session.get(SESSION_USER_INFO_KEY).await?;
    if let Err(status) = roster_access(&user_info) {
        return Ok(access_denied(status));
    }
    let snapshot = get_snapshot(&state).await?;
    Ok(JsonR(json!({
        "updated_at": snapshot.updated_at,
        "warning": snapshot.warning,
        "rows": snapshot.rows,
    }))
    .into_response())
}

async fn data_airport(
    State(state): State<Arc<AppState>>,
    session: Session,
    Path(icao): Path<String>,
) -> Result<Response, AppError> {
    let user_info: Option<UserInfo> = session.get(SESSION_USER_INFO_KEY).await?;
    if let Err(status) = roster_access(&user_info) {
        return Ok(access_denied(status));
    }
    let icao = icao.to_uppercase();
    if !state.ids_config.0.contains_key(&icao) {
        return Ok((
            StatusCode::NOT_FOUND,
            JsonR(json!({"error": "Airport is not configured in the IDS."})),
        )
            .into_response());
    }
    let snapshot = get_snapshot(&state).await?;
    Ok(JsonR(json!({
        "updated_at": snapshot.updated_at,
        "warning": snapshot.warning,
        "detail": snapshot.airports.get(&icao),
    }))
    .into_response())
}

/// Show the detail page for a single airport.
async fn page_airport(
    State(state): State<Arc<AppState>>,
    session: Session,
    Path(icao): Path<String>,
) -> Result<Response, AppError> {
    let user_info: Option<UserInfo> = session.get(SESSION_USER_INFO_KEY).await?;
    if roster_access(&user_info).is_err() {
        return Ok(Redirect::to("/").into_response());
    }
    let icao = icao.to_uppercase();
    if !state.ids_config.0.contains_key(&icao) {
        return Ok(StatusCode::NOT_FOUND.into_response());
    }
    let snapshot = get_snapshot(&state).await?;
    let template = state.templates.get_template("ids/airport.jinja")?;
    let flashed_messages = flashed_messages::drain_flashed_messages(session).await?;
    let rendered = template.render(context! {
        user_info, flashed_messages,
        detail => snapshot.airports.get(&icao),
        updated_at => snapshot.updated_at.to_rfc3339(),
        warning => snapshot.warning,
    })?;
    Ok(Html(rendered).into_response())
}

/// Show the base IDS page.
async fn page_home(
    State(state): State<Arc<AppState>>,
    session: Session,
) -> Result<Response, AppError> {
    let user_info: Option<UserInfo> = session.get(SESSION_USER_INFO_KEY).await?;
    if roster_access(&user_info).is_err() {
        return Ok(Redirect::to("/").into_response());
    }
    let snapshot = get_snapshot(&state).await?;
    let template = state.templates.get_template("ids/base.jinja")?;
    let flashed_messages = flashed_messages::drain_flashed_messages(session).await?;
    let rendered = template.render(context! {
        user_info, flashed_messages,
        rows => snapshot.rows,
        updated_at => snapshot.updated_at.to_rfc3339(),
        warning => snapshot.warning,
    })?;
    Ok(Html(rendered).into_response())
}

/// This file's routes and templates.
pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/ids", get(page_home))
        .route("/ids/data", get(data_home))
        .route("/ids/{icao}", get(page_airport))
        .route("/ids/{icao}/data", get(data_airport))
        .route("/ids/vatis/submit", post(receive_vatis_post))
        .route("/ids/vatis/current", get(show_atis_data))
        .layer(axum::middleware::map_response(
            |mut response: Response| async move {
                response.headers_mut().insert(
                    axum::http::header::CACHE_CONTROL,
                    axum::http::HeaderValue::from_static("private, no-store"),
                );
                response
            },
        ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shared::CacheEntry;
    use axum::{
        Extension,
        body::{Body, to_bytes},
        http::Request,
    };
    use tower::ServiceExt;
    use tower_sessions::MemoryStore;

    fn user(on_roster: bool, is_home: bool) -> UserInfo {
        UserInfo {
            cid: 123,
            first_name: "Test".into(),
            last_name: "Controller".into(),
            is_home,
            on_roster,
            is_some_staff: false,
            is_named_staff: false,
            is_training_staff: false,
            is_event_staff: false,
            is_admin: false,
        }
    }

    fn session() -> Session {
        Session::new(None, Arc::new(MemoryStore::default()), None)
    }

    async fn state() -> Arc<AppState> {
        let db = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::raw_sql(sql::CREATE_TABLES)
            .execute(&db)
            .await
            .unwrap();
        let mut templates = crate::load_templates().unwrap();
        templates.set_loader(minijinja::path_loader(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/templates"
        )));
        let state = Arc::new(AppState {
            config: vzdv::config::Config::default(),
            sectors_config: serde_json::from_str(include_str!("../../../splits.json")).unwrap(),
            ids_config: serde_json::from_str(include_str!("../../../ids.json")).unwrap(),
            ids_cache: IdsCache::default(),
            db,
            templates,
            cache: mini_moka::sync::Cache::new(30),
        });
        let weather = [
            vzdv::aviation::parse_metar("KDEN 030253Z 18005KT 10SM CLR A2992").unwrap(),
            vzdv::aviation::parse_metar("KAPA 030253Z 18005KT 10SM CLR A2992").unwrap(),
        ];
        state.cache.insert(
            "METAR_FULL".into(),
            CacheEntry::new(serde_json::to_string(&weather).unwrap()),
        );
        state
    }

    async fn register_user(state: &AppState, on_roster: bool, home: &str) {
        sqlx::query(sql::INSERT_USER_SIMPLE)
            .bind(123)
            .bind("Test")
            .bind("Controller")
            .bind(1)
            .bind(home)
            .execute(&state.db)
            .await
            .unwrap();
        sqlx::query(sql::SET_CONTROLLER_ON_ROSTER)
            .bind(123)
            .bind(on_roster)
            .execute(&state.db)
            .await
            .unwrap();
    }

    async fn request(state: &Arc<AppState>, session: &Session, path: &str) -> Response {
        router()
            .layer(axum::middleware::from_fn_with_state(
                state.clone(),
                crate::middleware::extend_session,
            ))
            .layer(Extension(session.clone()))
            .with_state(state.clone())
            .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
            .await
            .unwrap()
    }

    async fn response_json(response: Response) -> serde_json::Value {
        assert_eq!(response.headers()["content-type"], "application/json");
        assert_eq!(response.headers()["cache-control"], "private, no-store");
        serde_json::from_slice(&to_bytes(response.into_body(), 1_000_000).await.unwrap()).unwrap()
    }

    async fn expire(state: &AppState) {
        state
            .ids_cache
            .current
            .lock()
            .await
            .as_mut()
            .unwrap()
            .inserted = Instant::now() - REFRESH_INTERVAL;
    }

    async fn insert_atis(state: &AppState) {
        sqlx::query(sql::INSERT_ATIS_ENTRY)
            .bind("KAPA")
            .bind("NORTH VMC")
            .bind("B")
            .bind("combined")
            .bind("")
            .bind("")
            .bind(Utc::now())
            .bind("test")
            .execute(&state.db)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn roster_gate_protects_pages_and_json_before_cache_access() {
        let state = state().await;
        for logged_in in [false, true] {
            let session = session();
            if logged_in {
                session
                    .insert(SESSION_USER_INFO_KEY, user(true, true))
                    .await
                    .unwrap();
            }
            for path in ["/ids", "/ids/KDEN"] {
                let response = request(&state, &session, path).await;
                assert_eq!(response.status(), StatusCode::SEE_OTHER);
                assert_eq!(response.headers()["location"], "/");
            }
            for path in ["/ids/data", "/ids/KDEN/data", "/ids/vatis/current"] {
                let response = request(&state, &session, path).await;
                assert_eq!(
                    response.status(),
                    if logged_in {
                        StatusCode::FORBIDDEN
                    } else {
                        StatusCode::UNAUTHORIZED
                    }
                );
                assert!(response_json(response).await.get("error").is_some());
            }
        }
        assert!(state.ids_cache.current.lock().await.is_none());
    }

    #[tokio::test]
    async fn roster_members_get_shared_json_and_removal_revokes_access() {
        for home in ["ZDV", "ZLA"] {
            let state = state().await;
            register_user(&state, true, home).await;
            let session = session();
            let mut legacy = serde_json::to_value(user(false, home == "ZDV")).unwrap();
            legacy.as_object_mut().unwrap().remove("on_roster");
            session.insert(SESSION_USER_INFO_KEY, legacy).await.unwrap();
            let response = request(&state, &session, "/ids/data").await;
            assert_eq!(response.status(), StatusCode::OK);
            let overview = response_json(response).await;
            assert_eq!(overview["rows"].as_array().unwrap().len(), 15);
            assert!(
                session
                    .get::<UserInfo>(SESSION_USER_INFO_KEY)
                    .await
                    .unwrap()
                    .unwrap()
                    .on_roster
            );
            let detail = response_json(request(&state, &session, "/ids/kden/data").await).await;
            assert_eq!(detail["updated_at"], overview["updated_at"]);
            assert_eq!(detail["detail"]["icao"], "KDEN");
            assert!(detail["detail"]["dep_rows"][0]["corridor"].is_string());
            assert_eq!(
                request(&state, &session, "/ids/ZZZZ/data").await.status(),
                StatusCode::NOT_FOUND
            );
            for path in ["/ids", "/ids/KDEN", "/ids/KAPA"] {
                let response = request(&state, &session, path).await;
                assert_eq!(response.status(), StatusCode::OK);
                let html = String::from_utf8(
                    to_bytes(response.into_body(), 1_000_000)
                        .await
                        .unwrap()
                        .to_vec(),
                )
                .unwrap();
                assert!(html.contains("/static/ids_page.js"));
                assert!(html.contains("id=\"ids-row-template\""));
            }
            sqlx::query(sql::SET_CONTROLLER_ON_ROSTER)
                .bind(123)
                .bind(false)
                .execute(&state.db)
                .await
                .unwrap();
            assert_eq!(
                request(&state, &session, "/ids/data").await.status(),
                StatusCode::FORBIDDEN
            );
            assert!(
                !session
                    .get::<UserInfo>(SESSION_USER_INFO_KEY)
                    .await
                    .unwrap()
                    .unwrap()
                    .on_roster
            );
        }
    }

    #[tokio::test]
    async fn concurrent_reads_share_cache_and_refresh_after_expiry() {
        let state = state().await;
        let mut tasks = tokio::task::JoinSet::new();
        for _ in 0..10 {
            let state = state.clone();
            tasks.spawn(async move { get_snapshot(&state).await.unwrap() });
        }
        let first = tasks.join_next().await.unwrap().unwrap();
        while let Some(result) = tasks.join_next().await {
            assert!(Arc::ptr_eq(&first, &result.unwrap()));
        }
        insert_atis(&state).await;
        assert!(Arc::ptr_eq(&first, &get_snapshot(&state).await.unwrap()));
        expire(&state).await;
        let second = get_snapshot(&state).await.unwrap();
        assert!(!Arc::ptr_eq(&first, &second));
        let row = second.rows.iter().find(|row| row.icao == "KAPA").unwrap();
        assert_eq!(row.flow_name.as_deref(), Some("NORTH VMC"));
        assert_eq!(row.atis_info, "COMBINED B");
    }

    #[tokio::test]
    async fn weather_failure_retains_weather_while_atis_updates() {
        let state = state().await;
        let first = get_snapshot(&state).await.unwrap();
        insert_atis(&state).await;
        state
            .cache
            .insert("METAR_FULL".into(), CacheEntry::new("invalid JSON".into()));
        expire(&state).await;
        let second = get_snapshot(&state).await.unwrap();
        assert!(
            second
                .warning
                .as_ref()
                .unwrap()
                .contains("last available weather")
        );
        let row = second.rows.iter().find(|row| row.icao == "KAPA").unwrap();
        assert_eq!(row.wind.as_deref(), Some("180@5"));
        assert_eq!(row.atis_info, "COMBINED B");
        assert!(second.updated_at >= first.updated_at);
        assert!(Arc::ptr_eq(&second, &get_snapshot(&state).await.unwrap()));
    }

    #[tokio::test]
    async fn database_failure_retains_snapshot_and_limits_retries() {
        let state = state().await;
        let first = get_snapshot(&state).await.unwrap();
        state.db.close().await;
        expire(&state).await;
        let second = get_snapshot(&state).await.unwrap();
        assert!(second.warning.is_some());
        assert_eq!(first.updated_at, second.updated_at);
        assert_eq!(first.rows[0].dep_rwys, second.rows[0].dep_rwys);
        assert!(Arc::ptr_eq(&second, &get_snapshot(&state).await.unwrap()));
    }

    #[tokio::test]
    async fn templates_render_empty_error_and_escaped_states() {
        let state = state().await;
        let snapshot = get_snapshot(&state).await.unwrap();
        let mut row = snapshot.rows[0].clone();
        row.atis_info = "<img src=x onerror=alert(1)>".into();
        let html = state
            .templates
            .get_template("ids/base.jinja")
            .unwrap()
            .render(context! {
                rows => vec![row], user_info => user(true, false),
                updated_at => snapshot.updated_at.to_rfc3339(), warning => "\"<warning>"
            })
            .unwrap();
        assert!(!html.contains("<img src=x"));
        assert!(html.contains("&lt;img"));
        let empty = state
            .templates
            .get_template("ids/base.jinja")
            .unwrap()
            .render(context! {
                rows => Vec::<IdsRow>::new(), user_info => user(false, true),
                updated_at => snapshot.updated_at.to_rfc3339(),
            })
            .unwrap();
        assert!(empty.contains("No IDS airport data configured"));
        assert!(!empty.contains("href=\"/ids\">IDS"));
        let mut detail = snapshot.airports["KDEN"].clone();
        detail.error = Some("No METAR or complete ATIS data".into());
        detail.dep_rows.clear();
        let html = state
            .templates
            .get_template("ids/airport.jinja")
            .unwrap()
            .render(context! {
                detail, updated_at => snapshot.updated_at.to_rfc3339(),
            })
            .unwrap();
        assert!(html.contains("id=\"ids-detail\" hidden"));
        assert!(html.contains("id=\"ids-rows\""));
        assert!(html.contains("id=\"ids-row-template\""));
    }

    #[tokio::test]
    async fn configured_departure_rows_have_stable_unique_keys() {
        let state = state().await;
        for procedure in state.ids_config.0.values() {
            if let AirportProcedure::Split(procedure) = procedure {
                for flow in procedure.dep_flows.values() {
                    let rows = build_dep_rows(&flow.rwys, &procedure.dep_corridors).unwrap();
                    let keys: std::collections::HashSet<_> =
                        rows.iter().map(|row| &row.corridor).collect();
                    assert_eq!(keys.len(), rows.len());
                }
            }
        }
    }
}
