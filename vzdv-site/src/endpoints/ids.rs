//! Endpoints for the integrated IDS.

use crate::{
    flashed_messages,
    shared::{AppError, AppState, CacheEntry, SESSION_USER_INFO_KEY, UserInfo},
    vatis_jwt::Verdict,
};
use axum::{
    Router,
    extract::{DefaultBodyLimit, Json as JsonE, Path, State},
    http::HeaderMap,
    response::{Html, IntoResponse, Json as JsonR, Redirect, Response},
    routing::{get, post},
};
use chrono::{DateTime, TimeDelta, Utc};
use itertools::Itertools;
use log::{debug, error, warn};
use minijinja::context;
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{
    collections::{HashMap, HashSet},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::sync::{Mutex, Notify, RwLock};
use tower_sessions::Session;
use vatsim_utils::live_api::Vatsim;
use vzdv::{
    aviation::AirportWeather,
    config::{ConfigIDS, VatisJwtMode},
    ids::{AirportProcedure, AtisType, DepCorridor, FlowSide, FlowSource, ResolvedFlow},
    sql::{self, Atis},
};

/// Longest accepted text field in a vATIS update.
const MAX_ATIS_TEXT: usize = 8 * 1024;
/// How long a fresh vATIS update counts as online before the VATSIM feed lists the station.
const ATIS_GRACE: TimeDelta = TimeDelta::minutes(5);
const ONLINE_ATIS_CACHE_KEY: &str = "VATSIM_ATIS_ONLINE";

/// An IDS update POSTed by vATIS. Its client-side timestamp is ignored.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct VatisUpdate {
    facility: String,
    #[serde(default)]
    preset: String,
    #[serde(default)]
    atis_letter: String,
    atis_type: AtisType,
    airport_conditions: Option<String>,
    notams: Option<String>,
    text_atis: Option<String>,
    version: Option<String>,
}

impl VatisUpdate {
    /// vATIS sends an update with no letter or preset when the station disconnects.
    fn is_disconnect(&self) -> bool {
        self.atis_letter.is_empty() && self.preset.is_empty()
    }

    fn problem(&self) -> Option<&'static str> {
        let mut letter = self.atis_letter.chars();
        if !matches!((letter.next(), letter.next()), (Some('A'..='Z'), None)) {
            return Some("ATIS letter must be a single letter");
        }
        if self.preset.is_empty() || self.preset.len() > 100 {
            return Some("preset must be 1-100 characters");
        }
        let too_long = [&self.airport_conditions, &self.notams, &self.text_atis]
            .iter()
            .any(|text| text.as_ref().is_some_and(|t| t.len() > MAX_ATIS_TEXT));
        too_long.then_some("ATIS text is too long")
    }
}

/// Check vATIS's token according to the configured mode, returning the status
/// to respond with if the update is rejected.
async fn check_vatis_token(
    state: &AppState,
    headers: &HeaderMap,
    facility: &str,
) -> Option<StatusCode> {
    let mode = state.config.ids.vatis_jwt;
    if mode == VatisJwtMode::Off {
        return None;
    }
    let verdict = state.vatis_keys.verify(headers).await;
    if verdict == Verdict::Valid {
        return None;
    }
    if mode == VatisJwtMode::Enforce {
        warn!("Rejected vATIS update for {facility}: {verdict:?}");
        return Some(StatusCode::UNAUTHORIZED);
    }
    warn!("Accepting vATIS update for {facility} despite failed token check: {verdict:?}");
    None
}

/// Receive HTTP POST events from vATIS being ran by facility controllers.
///
/// Each station's latest update replaces its previous one.
async fn receive_vatis_post(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    JsonE(update): JsonE<VatisUpdate>,
) -> Result<StatusCode, AppError> {
    if let Some(status) = check_vatis_token(&state, &headers, &update.facility).await {
        return Ok(status);
    }
    let facility = update.facility.to_uppercase();
    if !state.ids_config.0.contains_key(&facility) {
        debug!("Ignoring vATIS update for unconfigured facility {facility}");
        return Ok(StatusCode::UNPROCESSABLE_ENTITY);
    }
    if update.is_disconnect() {
        sqlx::query(sql::DELETE_ATIS_FOR)
            .bind(&facility)
            .bind(update.atis_type)
            .execute(&state.db)
            .await?;
        debug!("{facility} {:?} ATIS disconnected", update.atis_type);
    } else {
        if let Some(problem) = update.problem() {
            warn!("Rejected vATIS update for {facility}: {problem}");
            return Ok(StatusCode::UNPROCESSABLE_ENTITY);
        }
        sqlx::query(sql::UPSERT_ATIS_ENTRY)
            .bind(&facility)
            .bind(&update.preset)
            .bind(&update.atis_letter)
            .bind(update.atis_type)
            .bind(update.airport_conditions.unwrap_or_default())
            .bind(update.notams.unwrap_or_default())
            .bind(Utc::now())
            .bind(update.version.unwrap_or_default())
            .bind(update.text_atis.unwrap_or_default())
            .execute(&state.db)
            .await?;
        debug!("New ATIS data stored for {facility}");
    }
    state.ids_cache.invalidate();
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

/// Callsigns of ATISes online on VATSIM, cached for a minute. `None` if the
/// feed is unavailable.
async fn online_atis_callsigns(state: &AppState) -> Option<HashSet<String>> {
    let cache_key = ONLINE_ATIS_CACHE_KEY.to_string();
    if let Some(cached) = state.cache.get(&cache_key)
        && cached.inserted.elapsed() < Duration::from_secs(60)
    {
        return serde_json::from_str(&cached.data).ok();
    }
    let fetch = async { anyhow::Ok(Vatsim::new().await?.get_v3_data().await?) };
    let data = match tokio::time::timeout(Duration::from_secs(10), fetch).await {
        Ok(Ok(data)) => data,
        Ok(Err(e)) => {
            warn!("Could not check online ATISes: {e}");
            return None;
        }
        Err(_) => {
            warn!("Timed out checking online ATISes");
            return None;
        }
    };
    let callsigns: HashSet<String> = data.atis.into_iter().map(|atis| atis.callsign).collect();
    if let Ok(json) = serde_json::to_string(&callsigns) {
        state.cache.insert(cache_key, CacheEntry::new(json));
    }
    Some(callsigns)
}

/// Drop ATISes whose station isn't on VATSIM, e.g. when vATIS crashed
/// without sending its disconnect update.
fn live_atis(atis: Vec<Atis>, online: Option<&HashSet<String>>, now: DateTime<Utc>) -> Vec<Atis> {
    let Some(online) = online else {
        return atis;
    };
    atis.into_iter()
        .filter(|a| {
            online.contains(&a.atis_type.callsign(&a.facility)) || now - a.timestamp < ATIS_GRACE
        })
        .collect()
}

/// An airport's runways and flow names for display.
#[derive(Debug, Clone, Serialize)]
struct FlowSummary {
    dep_rwys: String,
    arr_rwys: String,
    dep_name: Option<String>,
    arr_name: Option<String>,
    dep_source: Option<FlowSource>,
    arr_source: Option<FlowSource>,
    /// Airport this one's flow was matched to (`tryMatch`).
    match_icao: Option<String>,
}

impl FlowSummary {
    fn new(procedure: &AirportProcedure, flow: &ResolvedFlow) -> Self {
        let rwys = |side: &Option<FlowSide>| {
            side.as_ref()
                .map(|side| vzdv::ids::sorted_rwy_names(&side.rwys).join(", "))
                .unwrap_or_default()
        };
        let matched = [&flow.dep, &flow.arr].iter().any(|side| {
            side.as_ref()
                .is_some_and(|s| s.source == FlowSource::Matched)
        });
        Self {
            dep_rwys: rwys(&flow.dep),
            arr_rwys: rwys(&flow.arr),
            dep_name: flow.dep_name().map(String::from),
            arr_name: flow.arr_name().map(String::from),
            dep_source: flow.dep.as_ref().map(|side| side.source),
            arr_source: flow.arr.as_ref().map(|side| side.source),
            match_icao: procedure
                .try_match_icao()
                .filter(|_| matched)
                .map(String::from),
        }
    }
}

/// An airport's current weather for display.
#[derive(Debug, Clone, Serialize, Default)]
struct WeatherSummary {
    conditions: Option<String>,
    wind: Option<String>,
    altimeter: Option<String>,
    raw_metar: Option<String>,
}

impl WeatherSummary {
    fn new(weather: Option<&AirportWeather>) -> Self {
        let Some(weather) = weather else {
            return Self::default();
        };
        let (dir, mag, gust) = weather.wind;
        Self {
            conditions: Some(format!("{:?}", weather.conditions)),
            wind: Some(if gust > 0 {
                format!("{dir:03}@{mag}G{gust}")
            } else {
                format!("{dir:03}@{mag}")
            }),
            altimeter: weather.altimeter.map(|a| format!("{a:.2}")),
            raw_metar: Some(weather.raw.clone()),
        }
    }
}

/// A single row in the IDS overview table.
#[derive(Debug, Clone, Serialize)]
struct IdsRow {
    icao: String,
    is_split: bool,
    #[serde(flatten)]
    flow: FlowSummary,
    #[serde(flatten)]
    weather: WeatherSummary,
    /// Weather-based flow, when it differs from an ATIS-selected one.
    suggestion: Option<String>,
    atis_info: String,
    issue: Option<String>,
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
    rwy: String,
    /// Arrival gates assigned to this runway.
    gates: Vec<String>,
}

/// One online ATIS on the detail page.
#[derive(Debug, Clone, Serialize)]
struct AtisDetail {
    atis_type: AtisType,
    label: &'static str,
    letter: String,
    preset: String,
    /// Zulu time the update was received.
    received: String,
    age: String,
    airport_conditions: String,
    notams: String,
    text_atis: String,
}

/// Per-airport detail data passed to the template.
#[derive(Debug, Clone, Serialize)]
struct AirportDetail {
    icao: String,
    is_split: bool,
    #[serde(flatten)]
    flow: FlowSummary,
    #[serde(flatten)]
    weather: WeatherSummary,
    /// Weather-based flow.
    suggestion: Option<String>,
    /// Whether the suggestion differs from an ATIS-selected flow.
    suggestion_differs: bool,
    issue: Option<String>,
    dep_rows: Vec<DepRow>,
    arr_rows: Vec<ArrRow>,
    atis: Vec<AtisDetail>,
}

fn zulu(time: DateTime<Utc>) -> String {
    time.format("%H%MZ").to_string()
}

fn age(since: DateTime<Utc>, now: DateTime<Utc>) -> String {
    match (now - since).num_minutes().max(0) {
        0 => "just now".to_string(),
        minutes @ 1..=59 => format!("{minutes} min ago"),
        minutes => format!("{}h {:02}m ago", minutes / 60, minutes % 60),
    }
}

/// Describe a flow, e.g. "SOUTH VMC" or "D SOUTH EAST / A SOUTH EAST".
fn describe(flow: &ResolvedFlow, is_split: bool) -> Option<String> {
    if !is_split {
        return flow.dep_name().map(String::from);
    }
    if flow.dep.is_none() && flow.arr.is_none() {
        return None;
    }
    Some(format!(
        "D {} / A {}",
        flow.dep_name().unwrap_or("—"),
        flow.arr_name().unwrap_or("—")
    ))
}

/// Whether an ATIS-selected side differs from the weather-based suggestion.
fn atis_disagrees(current: &ResolvedFlow, suggested: &ResolvedFlow) -> bool {
    let differs = |side: &Option<FlowSide>, suggested: Option<&str>| {
        side.as_ref().is_some_and(|side| {
            side.source == FlowSource::Atis && suggested.is_some_and(|s| s != side.name)
        })
    };
    differs(&current.dep, suggested.dep_name()) || differs(&current.arr, suggested.arr_name())
}

/// Build the departure rows by resolving each corridor name against the
/// airport's `dep_corridors` definition (validated when the config loads).
fn build_dep_rows(side: &FlowSide, dep_corridors: &HashMap<String, DepCorridor>) -> Vec<DepRow> {
    let mut rows: Vec<_> = side
        .rwys
        .iter()
        .flat_map(|(rwy, corridors)| corridors.iter().map(move |corridor| (rwy, corridor)))
        .filter_map(|(rwy, corridor)| {
            let info = dep_corridors.get(corridor)?;
            Some(DepRow {
                corridor: corridor.clone(),
                direction: info.direction.clone(),
                rwy: rwy.clone(),
                gates: info.gates.clone(),
            })
        })
        .collect();
    rows.sort_by(|a, b| {
        a.direction
            .cmp(&b.direction)
            .then_with(|| a.rwy.cmp(&b.rwy))
            .then_with(|| a.corridor.cmp(&b.corridor))
    });
    rows
}

fn build_arr_rows(side: &FlowSide) -> Vec<ArrRow> {
    side.rwys
        .iter()
        .map(|(rwy, gates)| ArrRow {
            rwy: rwy.clone(),
            gates: gates.clone(),
        })
        .sorted_by(|a, b| a.rwy.cmp(&b.rwy))
        .collect()
}

/// Everything known about one airport while building a snapshot.
struct AirportState<'a> {
    icao: &'a str,
    procedure: &'a AirportProcedure,
    weather: Option<&'a AirportWeather>,
    flow: &'a ResolvedFlow,
    suggested: ResolvedFlow,
    /// The airport's ATISes, in `AtisType` order.
    atis: Vec<&'a Atis>,
}

impl AirportState<'_> {
    fn is_split(&self) -> bool {
        matches!(self.procedure, AirportProcedure::Split(_))
    }

    fn issue(&self) -> Option<String> {
        (!self.flow.issues.is_empty()).then(|| self.flow.issues.join("; "))
    }

    fn row(&self) -> IdsRow {
        let atis_info = if self.atis.is_empty() {
            "No ATIS".to_string()
        } else {
            self.atis
                .iter()
                .map(|a| {
                    let label = match a.atis_type {
                        AtisType::Combined => "ATIS",
                        AtisType::Departure => "DEP",
                        AtisType::Arrival => "ARR",
                    };
                    format!("{label} {} {}", a.atis_letter, zulu(a.timestamp))
                })
                .join(", ")
        };
        IdsRow {
            icao: self.icao.to_string(),
            is_split: self.is_split(),
            flow: FlowSummary::new(self.procedure, self.flow),
            weather: WeatherSummary::new(self.weather),
            suggestion: atis_disagrees(self.flow, &self.suggested)
                .then(|| describe(&self.suggested, self.is_split()))
                .flatten(),
            atis_info,
            issue: self.issue(),
        }
    }

    fn detail(&self, now: DateTime<Utc>) -> AirportDetail {
        let (dep_rows, arr_rows) = match self.procedure {
            AirportProcedure::Split(proc) => (
                self.flow
                    .dep
                    .as_ref()
                    .map(|side| build_dep_rows(side, &proc.dep_corridors))
                    .unwrap_or_default(),
                self.flow
                    .arr
                    .as_ref()
                    .map(build_arr_rows)
                    .unwrap_or_default(),
            ),
            // combined flows have no corridors or gates; the summary covers them
            AirportProcedure::Combined(_) => (Vec::new(), Vec::new()),
        };
        let atis = self
            .atis
            .iter()
            .map(|a| AtisDetail {
                atis_type: a.atis_type,
                label: match a.atis_type {
                    AtisType::Combined => "ATIS",
                    AtisType::Departure => "Departure ATIS",
                    AtisType::Arrival => "Arrival ATIS",
                },
                letter: a.atis_letter.clone(),
                preset: a.preset.clone(),
                received: zulu(a.timestamp),
                age: age(a.timestamp, now),
                airport_conditions: a.airport_conditions.clone(),
                notams: a.notams.clone(),
                text_atis: a.text_atis.clone(),
            })
            .collect();
        AirportDetail {
            icao: self.icao.to_string(),
            is_split: self.is_split(),
            flow: FlowSummary::new(self.procedure, self.flow),
            weather: WeatherSummary::new(self.weather),
            suggestion: describe(&self.suggested, self.is_split()),
            suggestion_differs: atis_disagrees(self.flow, &self.suggested),
            issue: self.issue(),
            dep_rows,
            arr_rows,
            atis,
        }
    }
}

const REFRESH_INTERVAL: Duration = Duration::from_secs(60);

#[derive(Default)]
pub struct IdsCache {
    current: RwLock<Option<CachedSnapshot>>,
    /// Held while rebuilding so only one refresh runs at a time.
    refreshing: Mutex<()>,
    /// Set by vATIS updates so the next read rebuilds.
    dirty: AtomicBool,
    /// Wakes the refresh task early.
    changed: Notify,
}

impl IdsCache {
    /// Mark the snapshot stale and wake the refresh task.
    pub fn invalidate(&self) {
        self.dirty.store(true, Ordering::Release);
        self.changed.notify_one();
    }

    async fn fresh(&self) -> Option<Arc<IdsSnapshot>> {
        if self.dirty.load(Ordering::Acquire) {
            return None;
        }
        self.current
            .read()
            .await
            .as_ref()
            .filter(|entry| entry.inserted.elapsed() < REFRESH_INTERVAL)
            .map(|entry| entry.snapshot.clone())
    }

    async fn latest(&self) -> Option<Arc<IdsSnapshot>> {
        self.current
            .read()
            .await
            .as_ref()
            .map(|entry| entry.snapshot.clone())
    }
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

fn build_snapshot(
    config: &ConfigIDS,
    weather: &[AirportWeather],
    atis: &[Atis],
    warning: Option<String>,
    now: DateTime<Utc>,
) -> IdsSnapshot {
    let weather_map: HashMap<String, &AirportWeather> = weather
        .iter()
        .map(|w| (format!("K{}", w.name), w))
        .collect();

    // airports without `tryMatch` first, so the ones matching them see their flows
    let mut flows: HashMap<String, ResolvedFlow> = HashMap::new();
    for (icao, procedure) in config
        .0
        .iter()
        .sorted_by_key(|(_, procedure)| procedure.try_match_icao().is_some())
    {
        let flow = procedure.resolve(icao, weather_map.get(icao).copied(), atis, &flows);
        flows.insert(icao.clone(), flow);
    }

    let mut rows = Vec::new();
    let mut airports = HashMap::new();
    for (icao, procedure) in &config.0 {
        let weather = weather_map.get(icao).copied();
        let airport = AirportState {
            icao,
            procedure,
            weather,
            flow: &flows[icao],
            suggested: procedure.resolve(icao, weather, &[], &flows),
            atis: atis
                .iter()
                .filter(|a| a.facility == *icao)
                .sorted_by_key(|a| a.atis_type)
                .collect(),
        };
        rows.push(airport.row());
        airports.insert(icao.clone(), airport.detail(now));
    }
    rows.sort_by(|a, b| a.icao.cmp(&b.icao));
    IdsSnapshot {
        updated_at: now,
        warning,
        rows,
        airports,
    }
}

async fn get_snapshot(state: &AppState) -> Result<Arc<IdsSnapshot>, AppError> {
    let cache = &state.ids_cache;
    if let Some(snapshot) = cache.fresh().await {
        return Ok(snapshot);
    }
    let _refreshing = match cache.refreshing.try_lock() {
        Ok(guard) => guard,
        Err(_) => {
            // another request is already rebuilding; don't wait on its METAR fetch
            if let Some(snapshot) = cache.latest().await {
                return Ok(snapshot);
            }
            cache.refreshing.lock().await
        }
    };
    if let Some(snapshot) = cache.fresh().await {
        return Ok(snapshot);
    }
    // cleared before reading so updates landing mid-rebuild trigger another
    cache.dirty.store(false, Ordering::Release);

    let atis: Vec<Atis> = match sqlx::query_as(sql::GET_ALL_ATIS_ENTRIES)
        .fetch_all(&state.db)
        .await
    {
        Ok(atis) => atis,
        Err(e) => {
            let mut current = cache.current.write().await;
            let Some(entry) = current.as_mut() else {
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
            let weather = cache
                .current
                .read()
                .await
                .as_ref()
                .map(|entry| entry.weather.clone())
                .unwrap_or_default();
            let warning = if weather.is_empty() {
                "Weather unavailable; using ATIS where possible."
            } else {
                "Weather could not be refreshed; runway estimates use the last available weather."
            };
            (weather, Some(warning.to_string()))
        }
    };
    let now = Utc::now();
    let atis = live_atis(atis, online_atis_callsigns(state).await.as_ref(), now);
    let snapshot = Arc::new(build_snapshot(
        &state.ids_config,
        &weather,
        &atis,
        warning,
        now,
    ));
    *cache.current.write().await = Some(CachedSnapshot {
        inserted: Instant::now(),
        snapshot: snapshot.clone(),
        weather,
    });
    Ok(snapshot)
}

/// Rebuild the snapshot every minute, or right after a vATIS update.
pub async fn refresh_snapshots(state: Arc<AppState>) {
    loop {
        if let Err(e) = get_snapshot(&state).await {
            error!("Could not refresh IDS snapshot: {e}");
        }
        tokio::select! {
            _ = tokio::time::sleep(REFRESH_INTERVAL) => {}
            _ = state.ids_cache.changed.notified() => {}
        }
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
        updated_z => snapshot.updated_at.format("%H:%M:%SZ").to_string(),
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
        updated_z => snapshot.updated_at.format("%H:%M:%SZ").to_string(),
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
        .route(
            "/ids/vatis/submit",
            post(receive_vatis_post).layer(DefaultBodyLimit::max(64 * 1024)),
        )
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
    use crate::vatis_jwt::{
        VatisKeys,
        tests::{TEST_JWKS, token},
    };
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

    async fn state_with(mode: VatisJwtMode) -> Arc<AppState> {
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
        let mut config = vzdv::config::Config::default();
        config.ids.vatis_jwt = mode;
        let state = Arc::new(AppState {
            config,
            sectors_config: serde_json::from_str(include_str!("../../../splits.json")).unwrap(),
            ids_config: serde_json::from_str(include_str!("../../../ids.json")).unwrap(),
            ids_cache: IdsCache::default(),
            vatis_keys: VatisKeys::from_jwks(TEST_JWKS),
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
        set_online(&state, &["KAPA_ATIS"]);
        state
    }

    async fn state() -> Arc<AppState> {
        state_with(VatisJwtMode::Log).await
    }

    /// Stub the VATSIM feed's online ATIS list.
    fn set_online(state: &AppState, callsigns: &[&str]) {
        state.cache.insert(
            ONLINE_ATIS_CACHE_KEY.into(),
            CacheEntry::new(serde_json::to_string(callsigns).unwrap()),
        );
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

    async fn send(state: &Arc<AppState>, session: &Session, request: Request<Body>) -> Response {
        router()
            .layer(axum::middleware::from_fn_with_state(
                state.clone(),
                crate::middleware::extend_session,
            ))
            .layer(Extension(session.clone()))
            .with_state(state.clone())
            .oneshot(request)
            .await
            .unwrap()
    }

    async fn request(state: &Arc<AppState>, session: &Session, path: &str) -> Response {
        send(
            state,
            session,
            Request::builder().uri(path).body(Body::empty()).unwrap(),
        )
        .await
    }

    /// POST a vATIS update, signed like vATIS when `signed` is true.
    async fn post_vatis(
        state: &Arc<AppState>,
        changes: serde_json::Value,
        signed: bool,
    ) -> StatusCode {
        let mut body = json!({
            "facility": "KAPA", "preset": "NORTH VMC", "atisLetter": "B", "atisType": "combined",
            "airportConditions": "VISUAL APCHS IN USE.", "notams": "TWY A CLSD.",
            "textAtis": "CENTENNIAL ATIS INFO B 1453Z.", "timestamp": "2026-10-02T14:53:00.1234567Z",
            "version": "4.2.0"
        });
        for (key, value) in changes.as_object().unwrap() {
            body[key] = value.clone();
        }
        let mut builder =
            Request::post("/ids/vatis/submit").header("content-type", "application/json");
        if signed {
            builder = builder.header("authorization", format!("Bearer {}", token(json!({}))));
        }
        let request = builder.body(Body::from(body.to_string())).unwrap();
        send(state, &session(), request).await.status()
    }

    async fn stored_atis(state: &AppState) -> Vec<Atis> {
        sqlx::query_as(sql::GET_ALL_ATIS_ENTRIES)
            .fetch_all(&state.db)
            .await
            .unwrap()
    }

    async fn body_text(response: Response) -> String {
        String::from_utf8(
            to_bytes(response.into_body(), 1_000_000)
                .await
                .unwrap()
                .to_vec(),
        )
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
            .write()
            .await
            .as_mut()
            .unwrap()
            .inserted = Instant::now() - REFRESH_INTERVAL;
    }

    async fn insert_atis(state: &AppState, preset: &str, received: DateTime<Utc>) {
        sqlx::query(sql::UPSERT_ATIS_ENTRY)
            .bind("KAPA")
            .bind(preset)
            .bind("B")
            .bind(AtisType::Combined)
            .bind("")
            .bind("")
            .bind(received)
            .bind("test")
            .bind("")
            .execute(&state.db)
            .await
            .unwrap();
    }

    fn row<'a>(snapshot: &'a IdsSnapshot, icao: &str) -> &'a IdsRow {
        snapshot.rows.iter().find(|row| row.icao == icao).unwrap()
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
        assert!(state.ids_cache.current.read().await.is_none());
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
            assert!(detail["detail"]["arr_rows"][0]["rwy"].is_string());
            assert_eq!(
                request(&state, &session, "/ids/ZZZZ/data").await.status(),
                StatusCode::NOT_FOUND
            );
            for (path, template) in [
                ("/ids", "ids-airports-template"),
                ("/ids/KDEN", "ids-dep-template"),
                ("/ids/KAPA", "ids-atis-template"),
            ] {
                let response = request(&state, &session, path).await;
                assert_eq!(response.status(), StatusCode::OK);
                let html = body_text(response).await;
                assert!(html.contains("/static/ids_page.js"));
                assert!(html.contains(&format!("id=\"{template}\"")), "{path}");
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
        insert_atis(&state, "NORTH VMC", Utc::now()).await;
        assert!(Arc::ptr_eq(&first, &get_snapshot(&state).await.unwrap()));
        expire(&state).await;
        let second = get_snapshot(&state).await.unwrap();
        assert!(!Arc::ptr_eq(&first, &second));
        let row = row(&second, "KAPA");
        assert_eq!(row.flow.dep_name.as_deref(), Some("NORTH VMC"));
        assert_eq!(row.flow.dep_source, Some(FlowSource::Atis));
        assert!(row.atis_info.starts_with("ATIS B "), "{}", row.atis_info);
    }

    #[tokio::test]
    async fn weather_failure_retains_weather_while_atis_updates() {
        let state = state().await;
        let first = get_snapshot(&state).await.unwrap();
        insert_atis(&state, "NORTH VMC", Utc::now()).await;
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
        let row = row(&second, "KAPA");
        assert_eq!(row.weather.wind.as_deref(), Some("180@5"));
        assert!(row.atis_info.starts_with("ATIS B "));
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
        assert_eq!(first.rows[0].flow.dep_rwys, second.rows[0].flow.dep_rwys);
        assert!(Arc::ptr_eq(&second, &get_snapshot(&state).await.unwrap()));
    }

    #[tokio::test]
    async fn templates_render_empty_issue_and_escaped_states() {
        let state = state().await;
        let snapshot = get_snapshot(&state).await.unwrap();
        let render = |name: &str, ctx: minijinja::Value| {
            state
                .templates
                .get_template(name)
                .unwrap()
                .render(ctx)
                .unwrap()
        };
        let mut row = snapshot.rows[0].clone();
        row.atis_info = "<img src=x onerror=alert(1)>".into();
        let html = render(
            "ids/base.jinja",
            context! {
                rows => vec![row], user_info => user(true, false),
                updated_at => snapshot.updated_at.to_rfc3339(), warning => "\"<warning>"
            },
        );
        assert!(!html.contains("<img src=x"));
        assert!(html.contains("&lt;img"));
        let empty = render(
            "ids/base.jinja",
            context! {
                rows => Vec::<IdsRow>::new(), user_info => user(false, true),
                updated_at => snapshot.updated_at.to_rfc3339(),
            },
        );
        assert!(empty.contains("No IDS airport data configured"));
        assert!(!empty.contains("href=\"/ids\">IDS"));
        let mut detail = snapshot.airports["KDEN"].clone();
        detail.issue = Some("No METAR available".into());
        detail.dep_rows.clear();
        let html = render(
            "ids/airport.jinja",
            context! { detail, updated_at => snapshot.updated_at.to_rfc3339() },
        );
        assert!(html.contains("No METAR available"));
        assert!(html.contains(
            "id=\"ids-dep-table\" class=\"table table-bordered table-sm align-middle\" hidden"
        ));
        assert!(html.contains("id=\"ids-dep-template\""));
        assert!(html.contains("No ATIS online."));
        // combined airports have no corridor or gate tables
        let html = render(
            "ids/airport.jinja",
            context! { detail => snapshot.airports["KAPA"].clone(), updated_at => "" },
        );
        assert!(!html.contains("ids-dep-template"));
        assert!(html.contains("ids-atis-template"));
    }

    /// Every element static/ids_page.js updates exists in the rendered pages.
    #[tokio::test]
    async fn templates_have_every_element_the_script_updates() {
        let state = state().await;
        insert_atis(&state, "NORTH VMC", Utc::now()).await;
        let snapshot = get_snapshot(&state).await.unwrap();
        let render = |name: &str, ctx: minijinja::Value| {
            state
                .templates
                .get_template(name)
                .unwrap()
                .render(ctx)
                .unwrap()
        };
        let common = ["ids-page", "ids-status", "ids-warning", "ids-updated"];
        let overview = render(
            "ids/base.jinja",
            context! { rows => snapshot.rows.clone(), updated_at => "" },
        );
        let overview_ids = [
            "ids-airports-table",
            "ids-airports-rows",
            "ids-airports-empty",
            "ids-airports-template",
        ];
        let overview_fields = [
            "airport-link",
            "dep_rwys",
            "dep_source",
            "arr_rwys",
            "arr_source",
            "split",
            "dep_name",
            "arr_name",
            "flow_name",
            "suggestion",
            "issue",
            "atis_info",
            "conditions",
            "wind",
            "altimeter",
            "raw_metar",
        ];
        let airport_ids = [
            "ids-issue",
            "ids-dep-rwys",
            "ids-dep-source",
            "ids-dep-name",
            "ids-arr-rwys",
            "ids-arr-source",
            "ids-arr-name",
            "ids-suggestion",
            "ids-suggested-flow",
            "ids-conditions",
            "ids-weather",
            "ids-metar",
            "ids-atis-rows",
            "ids-atis-empty",
            "ids-atis-template",
        ];
        let split_ids = [
            "ids-dep-table",
            "ids-dep-rows",
            "ids-dep-empty",
            "ids-dep-template",
            "ids-arr-table",
            "ids-arr-rows",
            "ids-arr-empty",
            "ids-arr-template",
        ];
        let airport_fields = [
            "direction",
            "rwy",
            "gates",
            "label",
            "letter",
            "preset",
            "received",
            "airport_conditions",
            "notams",
            "text_atis",
        ];
        let has = |html: &str, attr: &str, values: &[&str]| {
            for value in values {
                assert!(
                    html.contains(&format!("{attr}=\"{value}\"")),
                    "missing {attr} {value}"
                );
            }
        };
        has(&overview, "id", &common);
        has(&overview, "id", &overview_ids);
        has(&overview, "data-field", &overview_fields);
        for icao in ["KDEN", "KAPA"] {
            let html = render(
                "ids/airport.jinja",
                context! { detail => snapshot.airports[icao].clone(), updated_at => "" },
            );
            has(&html, "id", &common);
            has(&html, "id", &airport_ids);
            if icao == "KDEN" {
                has(&html, "id", &split_ids);
                has(&html, "data-field", &airport_fields);
            }
        }
    }

    #[tokio::test]
    async fn configured_departure_rows_have_stable_unique_keys() {
        let state = state().await;
        for procedure in state.ids_config.0.values() {
            if let AirportProcedure::Split(procedure) = procedure {
                for (name, flow) in &procedure.dep_flows {
                    let side = FlowSide {
                        name: name.clone(),
                        rwys: flow.rwys.clone(),
                        source: FlowSource::Atis,
                    };
                    let rows = build_dep_rows(&side, &procedure.dep_corridors);
                    let keys: HashSet<_> = rows.iter().map(|row| &row.corridor).collect();
                    assert_eq!(keys.len(), rows.len());
                    assert_eq!(rows.len(), flow.rwys.values().flatten().count());
                }
            }
        }
    }

    #[tokio::test]
    async fn vatis_updates_replace_previous_and_disconnect_clears() {
        let state = state().await;
        let first = get_snapshot(&state).await.unwrap();
        assert_eq!(row(&first, "KAPA").atis_info, "No ATIS");

        assert_eq!(post_vatis(&state, json!({}), true).await, StatusCode::OK);
        assert_eq!(
            post_vatis(
                &state,
                json!({ "atisLetter": "C", "preset": "SOUTH VMC" }),
                true
            )
            .await,
            StatusCode::OK
        );
        let stored = stored_atis(&state).await;
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].atis_letter, "C");
        assert_eq!(stored[0].text_atis, "CENTENNIAL ATIS INFO B 1453Z.");
        // received time, not vATIS's clock
        assert!(Utc::now() - stored[0].timestamp < TimeDelta::minutes(1));

        // the update invalidates the cached snapshot right away
        let updated = get_snapshot(&state).await.unwrap();
        assert!(!Arc::ptr_eq(&first, &updated));
        assert!(row(&updated, "KAPA").atis_info.starts_with("ATIS C "));
        let detail = &updated.airports["KAPA"];
        assert_eq!(detail.atis.len(), 1);
        assert_eq!(detail.atis[0].airport_conditions, "VISUAL APCHS IN USE.");
        assert_eq!(detail.atis[0].age, "just now");

        let disconnect = json!({
            "preset": "", "atisLetter": "", "airportConditions": "", "notams": "", "textAtis": ""
        });
        assert_eq!(post_vatis(&state, disconnect, true).await, StatusCode::OK);
        assert!(stored_atis(&state).await.is_empty());
        let cleared = get_snapshot(&state).await.unwrap();
        assert_eq!(row(&cleared, "KAPA").atis_info, "No ATIS");
        assert_eq!(
            row(&cleared, "KAPA").flow.dep_source,
            Some(FlowSource::Matched)
        );
    }

    #[tokio::test]
    async fn vatis_updates_are_validated() {
        let state = state().await;
        for (changes, status) in [
            (
                json!({ "facility": "KBKF" }),
                StatusCode::UNPROCESSABLE_ENTITY,
            ),
            (
                json!({ "atisLetter": "BB" }),
                StatusCode::UNPROCESSABLE_ENTITY,
            ),
            (
                json!({ "atisLetter": "b" }),
                StatusCode::UNPROCESSABLE_ENTITY,
            ),
            (json!({ "preset": "" }), StatusCode::UNPROCESSABLE_ENTITY),
            (
                json!({ "textAtis": "X".repeat(MAX_ATIS_TEXT + 1) }),
                StatusCode::UNPROCESSABLE_ENTITY,
            ),
            (
                json!({ "atisType": "Combined" }),
                StatusCode::UNPROCESSABLE_ENTITY,
            ),
        ] {
            assert_eq!(
                post_vatis(&state, changes.clone(), true).await,
                status,
                "{changes}"
            );
        }
        assert!(stored_atis(&state).await.is_empty());
        // facility case doesn't matter
        assert_eq!(
            post_vatis(&state, json!({ "facility": "kapa" }), true).await,
            StatusCode::OK
        );
        assert_eq!(stored_atis(&state).await[0].facility, "KAPA");
    }

    #[tokio::test]
    async fn token_mode_controls_unsigned_updates() {
        let state = state_with(VatisJwtMode::Enforce).await;
        assert_eq!(
            post_vatis(&state, json!({}), false).await,
            StatusCode::UNAUTHORIZED
        );
        assert!(stored_atis(&state).await.is_empty());
        assert_eq!(post_vatis(&state, json!({}), true).await, StatusCode::OK);

        for mode in [VatisJwtMode::Log, VatisJwtMode::Off] {
            let state = state_with(mode).await;
            assert_eq!(post_vatis(&state, json!({}), false).await, StatusCode::OK);
            assert_eq!(stored_atis(&state).await.len(), 1);
        }
    }

    #[tokio::test]
    async fn offline_atis_is_ignored_after_grace_period() {
        let state = state().await;
        set_online(&state, &[]);
        insert_atis(&state, "NORTH VMC", Utc::now() - TimeDelta::minutes(30)).await;
        let snapshot = get_snapshot(&state).await.unwrap();
        assert_eq!(row(&snapshot, "KAPA").atis_info, "No ATIS");

        // a fresh update counts even before the feed lists the station
        insert_atis(&state, "NORTH VMC", Utc::now()).await;
        expire(&state).await;
        let snapshot = get_snapshot(&state).await.unwrap();
        assert!(row(&snapshot, "KAPA").atis_info.starts_with("ATIS B "));

        // without the feed, trust the stored data
        let now = Utc::now();
        let atis = stored_atis(&state).await;
        assert_eq!(
            live_atis(atis.clone(), None, now + TimeDelta::hours(1)).len(),
            1
        );
        let online = HashSet::from(["KAPA_ATIS".to_string()]);
        assert_eq!(
            live_atis(atis, Some(&online), now + TimeDelta::hours(1)).len(),
            1
        );
    }

    #[tokio::test]
    async fn suggestion_shows_when_atis_disagrees_with_weather() {
        let state = state().await;
        // KDEN is south in the stubbed weather, so KAPA matches it south
        insert_atis(&state, "NORTH VMC", Utc::now()).await;
        let snapshot = get_snapshot(&state).await.unwrap();
        let apa = row(&snapshot, "KAPA");
        assert_eq!(apa.suggestion.as_deref(), Some("SOUTH VMC"));
        assert!(snapshot.airports["KAPA"].suggestion_differs);

        insert_atis(&state, "SOUTH VMC", Utc::now()).await;
        expire(&state).await;
        let snapshot = get_snapshot(&state).await.unwrap();
        assert_eq!(row(&snapshot, "KAPA").suggestion, None);
        assert_eq!(
            snapshot.airports["KAPA"].suggestion.as_deref(),
            Some("SOUTH VMC")
        );
        let den = &snapshot.airports["KDEN"];
        assert_eq!(
            den.suggestion.as_deref(),
            Some("D SOUTH CALM / A SOUTH CALM")
        );
        assert_eq!(den.flow.dep_source, Some(FlowSource::Weather));
    }

    #[test]
    fn ages_read_naturally() {
        let now = Utc::now();
        assert_eq!(age(now, now), "just now");
        assert_eq!(age(now - TimeDelta::minutes(12), now), "12 min ago");
        assert_eq!(age(now - TimeDelta::minutes(65), now), "1h 05m ago");
        // clock skew never shows a negative age
        assert_eq!(age(now + TimeDelta::minutes(2), now), "just now");
    }
}
