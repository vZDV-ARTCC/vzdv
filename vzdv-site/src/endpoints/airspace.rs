//! Endpoints for getting information on the airspace.

use crate::{
    flashed_messages,
    flights::get_relevant_flights,
    shared::{
        AppError, AppState, CacheEntry, SESSION_USER_INFO_KEY, UserInfo, get_all_weather,
        record_log,
    },
};
use axum::{
    Form, Router,
    extract::{Query, State},
    response::{Html, IntoResponse, Redirect, Response},
    routing::{get, post},
};
use itertools::Itertools;
use minijinja::context;
use num_format::{Locale, ToFormattedString};
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
    time::Instant,
};
use tokio::task::JoinSet;
use tower_sessions::Session;
use vatsim_utils::{live_api::Vatsim, rest_api::get_ratings_times};
use vzdv::{
    GENERAL_HTTP_CLIENT,
    aviation::parse_metar,
    kden::{determine_runway_config, fetch_runway_assignments, wind_components},
};

/// How far away from the selected airport to show pilots in the pilot glance page.
const GLANCE_DISTANCE: f64 = 20.0;

/// Table of all the airspace's airports.
async fn page_airports(
    State(state): State<Arc<AppState>>,
    session: Session,
) -> Result<Html<String>, AppError> {
    let user_info: Option<UserInfo> = session.get(SESSION_USER_INFO_KEY).await?;
    let template = state.templates.get_template("airspace/airports.jinja")?;
    let airports: Vec<_> = state
        .config
        .airports
        .all
        .iter()
        .sorted_by(|a, b| a.code.cmp(&b.code))
        .collect();
    let rendered = template.render(context! { user_info, airports })?;
    Ok(Html(rendered))
}

/// Table of all airspace-relevant flights.
async fn page_flights(
    State(state): State<Arc<AppState>>,
    session: Session,
) -> Result<Html<String>, AppError> {
    // cache this endpoint's returned data for 15 seconds
    let cache_key = "ONLINE_FLIGHTS_FULL".to_string();
    if let Some(cached) = state.cache.get(&cache_key) {
        let elapsed = Instant::now() - cached.inserted;
        if elapsed.as_secs() < 15 {
            return Ok(Html(cached.data));
        }
        state.cache.invalidate(&cache_key);
    }

    let vatsim_data = Vatsim::new().await?.get_v3_data().await?;
    let flights = {
        let all = get_relevant_flights(&state.config, &vatsim_data.pilots);
        let mut flights = HashSet::with_capacity(all.plan_within.len()); // won't be all of them, but might save one allocation
        flights.extend(all.actually_within);
        flights.extend(all.plan_from);
        flights.extend(all.plan_to);
        flights.extend(all.plan_within);
        let flights: Vec<_> = flights
            .iter()
            .cloned()
            .sorted_by(|a, b| a.callsign.cmp(&b.callsign))
            .collect();
        flights
    };

    let user_info: Option<UserInfo> = session.get(SESSION_USER_INFO_KEY).await?;
    let template = state.templates.get_template("airspace/flights.jinja")?;
    let rendered = template.render(context! { user_info, flights })?;
    state
        .cache
        .insert(cache_key, CacheEntry::new(rendered.clone()));
    Ok(Html(rendered))
}

/// Larger view of the weather.
async fn page_weather(
    State(state): State<Arc<AppState>>,
    session: Session,
) -> Result<Html<String>, AppError> {
    // cache this endpoint's returned data for 5 minutes
    let cache_key = "WEATHER_FULL".to_string();
    if let Some(cached) = state.cache.get(&cache_key) {
        let elapsed = Instant::now() - cached.inserted;
        if elapsed.as_secs() < 300 {
            return Ok(Html(cached.data));
        }
        state.cache.invalidate(&cache_key);
    }

    let weather = match get_all_weather(&state).await {
        Ok(weather) => weather,
        Err(_) => {
            return Err(AppError::HttpResponse("METAR API returned an error", 500));
        }
    };

    let user_info: Option<UserInfo> = session.get(SESSION_USER_INFO_KEY).await?;
    let template = state.templates.get_template("airspace/weather.jinja")?;
    let rendered = template.render(context! { user_info, weather })?;
    state
        .cache
        .insert(cache_key, CacheEntry::new(rendered.clone()));
    Ok(Html(rendered))
}

/// Form for groups to submit requests for staff-ups.
async fn page_staffing_request(
    State(state): State<Arc<AppState>>,
    session: Session,
) -> Result<Html<String>, AppError> {
    let user_info: Option<UserInfo> = session.get(SESSION_USER_INFO_KEY).await?;
    let flashed_messages = flashed_messages::drain_flashed_messages(session).await?;
    let template = state
        .templates
        .get_template("airspace/staffing_request.jinja")?;
    let rendered = template.render(context! { user_info, flashed_messages })?;
    Ok(Html(rendered))
}

#[derive(Debug, Deserialize)]
struct StaffingRequestForm {
    departure: String,
    dt_start: String,
    pilot_count: i16,
    contact: String,
    arrival: String,
    dt_end: String,
    banner: String,
    organization: String,
    comments: String,
}

/// Submit the staffing request form.
async fn page_staffing_request_post(
    State(state): State<Arc<AppState>>,
    session: Session,
    Form(staffing_request): Form<StaffingRequestForm>,
) -> Result<Redirect, AppError> {
    let user_info: Option<UserInfo> = session.get(SESSION_USER_INFO_KEY).await.unwrap();
    if let Some(user_info) = user_info {
        let resp = GENERAL_HTTP_CLIENT
            .post(&state.config.discord.webhooks.staffing_request)
            .json(&json!({
                "content": "",
                "embeds": [{
                    "title": "New staffing request",
                    "fields": [
                        {
                            "name": "From",
                            "value": format!("{} {} ({})", user_info.first_name, user_info.last_name, user_info.cid)
                        },
                        {
                            "name": "Departure",
                            "value": staffing_request.departure
                        },
                        {
                            "name": "Arrival",
                            "value": staffing_request.arrival
                        },
                        {
                            "name": "Start time",
                            "value": staffing_request.dt_start
                        },
                        {
                            "name": "End time",
                            "value": staffing_request.dt_end
                        },
                        {
                            "name": "Pilot count",
                            "value": staffing_request.pilot_count
                        },
                        {
                            "name": "Contact",
                            "value": staffing_request.contact
                        },
                        {
                            "name": "Banner",
                            "value": staffing_request.banner
                        },
                        {
                            "name": "Organization",
                            "value": staffing_request.organization
                        },
                        {
                            "name": "Comments",
                            "value": staffing_request.comments
                        }
                    ]
                }]
            }))
            .send()
            .await?;
        record_log(
            format!("{} submitted a staffing request", user_info.cid),
            &state.db,
            true,
        )
        .await?;
        if resp.status().is_success() {
            flashed_messages::push_flashed_message(
                session,
                flashed_messages::MessageLevel::Success,
                "Request submitted",
            )
            .await?;
        } else {
            flashed_messages::push_flashed_message(
                session,
                flashed_messages::MessageLevel::Error,
                "The message could not be processed. You may want to contact the EC (or WM).",
            )
            .await?;
        }
    } else {
        flashed_messages::push_flashed_message(
            session,
            flashed_messages::MessageLevel::Error,
            "You must be logged in to submit a staffing request",
        )
        .await?;
    }
    Ok(Redirect::to("/airspace/staffing_request"))
}

/// Page to show pilot info near an airport.
async fn page_pilot_glance(
    State(state): State<Arc<AppState>>,
    session: Session,
) -> Result<Response, AppError> {
    let user_info: Option<UserInfo> = session.get(SESSION_USER_INFO_KEY).await.unwrap();
    if user_info.is_none() {
        return Ok(Redirect::to("/").into_response());
    }
    let template = state
        .templates
        .get_template("airspace/pilot_glance.jinja")?;
    let rendered = template.render(context! { user_info })?;
    Ok(Html(rendered).into_response())
}

#[derive(Debug, Serialize)]
struct PilotGlance {
    callsign: String,
    aircraft: String,
    time_piloting: String,
    time_controlling: String,
    transponder: String,
    assigned_transponder: String,
}

/// API endpoint to get pilot data near an airport.
async fn page_pilot_glance_data(
    State(state): State<Arc<AppState>>,
    session: Session,
    Query(params): Query<HashMap<String, String>>,
) -> Result<Response, AppError> {
    let user_info: Option<UserInfo> = session.get(SESSION_USER_INFO_KEY).await.unwrap();
    if user_info.is_none() {
        return Ok(StatusCode::NOT_FOUND.into_response());
    }
    let airport = match params.get("airport") {
        Some(a) => a.to_uppercase(),
        None => {
            return Ok(StatusCode::NOT_FOUND.into_response());
        }
    };

    // cache this endpoint's returned data for 1 minute
    let cache_key = format!("PILOT_GLANCE-{airport}");
    if let Some(cached) = state.cache.get(&cache_key) {
        let elapsed = Instant::now() - cached.inserted;
        if elapsed.as_secs() < 60 {
            return Ok(Html(cached.data).into_response());
        }
        state.cache.invalidate(&cache_key);
    }

    let airport_info = match vatsim_utils::distance::AIRPORTS_MAP.get(airport.as_str()) {
        Some(info) => info,
        None => {
            return Ok(StatusCode::NOT_FOUND.into_response());
        }
    };
    let data = Vatsim::new().await?.get_v3_data().await?;
    let pilots: Vec<_> = data
        .pilots
        .iter()
        .filter(|pilot| {
            vatsim_utils::distance::haversine(
                airport_info.latitude,
                airport_info.longitude,
                pilot.latitude,
                pilot.longitude,
            ) <= GLANCE_DISTANCE
        })
        .collect();

    let mut timing_map = HashMap::new();
    let mut futures = JoinSet::new();
    pilots
        .iter()
        .map(|pilot| pilot.cid)
        .collect::<HashSet<u64>>()
        .iter()
        .for_each(|&cid| {
            futures.spawn(get_ratings_times(cid));
        });
    while let Some(res) = futures.join_next().await {
        if let Ok(Ok(data)) = res {
            timing_map.insert(data.id as u64, (data.pilot, data.atc));
        }
    }

    let glance_data: Vec<_> = pilots
        .iter()
        .map(|pilot| {
            let (t_pilot, t_atc) = timing_map.get(&pilot.cid).unwrap_or(&(0.0, 0.0));
            (pilot, t_pilot, t_atc)
        })
        .sorted_by(|(_, time_a, _), (_, time_b, _)| {
            time_a
                .partial_cmp(time_b)
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .map(|(pilot, t_pilot, t_atc)| PilotGlance {
            callsign: pilot.callsign.clone(),
            aircraft: pilot
                .flight_plan
                .as_ref()
                .map(|fp| fp.aircraft.clone())
                .unwrap_or_else(|| String::from("?")),
            time_piloting: (t_pilot.round() as i64).to_formatted_string(&Locale::en),
            time_controlling: (t_atc.round() as i64).to_formatted_string(&Locale::en),
            transponder: pilot.transponder.clone(),
            assigned_transponder: pilot
                .flight_plan
                .as_ref()
                .map(|fp| fp.assigned_transponder.clone())
                .unwrap_or_else(|| pilot.transponder.clone()),
        })
        .collect();

    let template = state
        .templates
        .get_template("airspace/pilot_glance_data.jinja")?;
    let rendered = template.render(context! { user_info, glance_data, airport })?;
    state
        .cache
        .insert(cache_key, CacheEntry::new(rendered.clone()));
    Ok(Html(rendered).into_response())
}

/// Render some inforamtion about KDEN, including current weather, ATIS, and
/// recommended runway selection with head/tail/crosswind components.
async fn page_denver(
    State(state): State<Arc<AppState>>,
    session: Session,
) -> Result<Html<String>, AppError> {
    let user_info: Option<UserInfo> = session.get(SESSION_USER_INFO_KEY).await.unwrap();
    let template = state.templates.get_template("airspace/kden.jinja")?;
    let rendered = template.render(context! { user_info })?;
    Ok(Html(rendered))
}

#[derive(Debug, Deserialize, Serialize)]
struct RealWorldAtis {
    #[serde(rename = "type")]
    direction: String,
    code: char,
    datis: String,
}

/// Use the FAA Digital ATIS lookup site's API to retrieve the current real world KDEN ATIS.
async fn get_real_world_kden_atis() -> Result<Vec<RealWorldAtis>, AppError> {
    let resp = GENERAL_HTTP_CLIENT
        .get("https://datis.clowd.io/api/KDEN")
        .send()
        .await?;
    if !resp.status().is_success() {
        return Err(AppError::HttpResponse(
            "getting real world KDEN ATIS",
            resp.status().as_u16(),
        ));
    }
    let data = resp.json().await?;
    Ok(data)
}

/// Return an HTML snippet containing information for the parent page.
async fn page_denver_data(
    State(state): State<Arc<AppState>>,
    session: Session,
) -> Result<Html<String>, AppError> {
    // cache this endpoint's returned data for 5 minutes
    let cache_key = "KDEN_INFO".to_string();
    if let Some(cached) = state.cache.get(&cache_key) {
        let elapsed = Instant::now() - cached.inserted;
        if elapsed.as_secs() < 300 {
            return Ok(Html(cached.data));
        }
        state.cache.invalidate(&cache_key);
    }

    let user_info: Option<UserInfo> = session.get(SESSION_USER_INFO_KEY).await.unwrap();
    let metar_resp = GENERAL_HTTP_CLIENT
        .get("https://metar.vatsim.net/KDEN")
        .send()
        .await?;
    if !metar_resp.status().is_success() {
        return Err(AppError::HttpResponse(
            "VATSIM METAR API",
            metar_resp.status().as_u16(),
        ));
    }
    let metar = metar_resp.text().await?;
    let weather =
        parse_metar(&metar).map_err(|e| AppError::GenericFallback("parsing METAR for KDEN", e))?;
    let runway_config = determine_runway_config(&weather);
    let real_world = get_real_world_kden_atis().await?;
    let wind_components = wind_components(&weather);
    let runway_selection = fetch_runway_assignments()
        .await
        .map_err(|e| AppError::GenericFallback("determining recommended runways", e))?;

    let wind = {
        let dir = weather.wind.0 as i16 - 10;
        let dir = if dir < 0 {
            360 + dir
        } else if dir > 360 {
            dir - 360
        } else {
            dir
        };
        format!(
            "{} at {}{}",
            dir,
            weather.wind.1,
            if weather.wind.2 > 0 {
                format!(" gust {}", weather.wind.2)
            } else {
                String::new()
            }
        )
    };

    let template = state.templates.get_template("airspace/kden_data.jinja")?;
    let rendered = template.render(context! {
        user_info,
        weather,
        wind,
        runway_config => runway_config.name(),
        departing => runway_config.departing(),
        landing => runway_config.landing(),
        wind_components,
        runway_selection,
        real_world,
    })?;
    state
        .cache
        .insert(cache_key, CacheEntry::new(rendered.clone()));
    Ok(Html(rendered))
}

/// This file's routes and templates.
pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/airspace/airports", get(page_airports))
        .route("/airspace/flights", get(page_flights))
        .route("/airspace/weather", get(page_weather))
        .route("/airspace/staffing_request", get(page_staffing_request))
        .route("/airspace/pilot_glance", get(page_pilot_glance))
        .route("/airspace/pilot_glance/data", get(page_pilot_glance_data))
        .route(
            "/airspace/staffing_request",
            post(page_staffing_request_post),
        )
        .route("/airspace/KDEN", get(page_denver))
        .route("/airspace/KDEN/data", get(page_denver_data))
}
