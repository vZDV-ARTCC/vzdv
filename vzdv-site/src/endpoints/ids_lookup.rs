//! Weather and chart lookups for any airport, shown beside the IDS.

use crate::{
    endpoints::ids::{WeatherSummary, roster_access},
    shared::{AppError, AppState, SESSION_USER_INFO_KEY, UserInfo},
};
use anyhow::bail;
use axum::{
    Router,
    extract::{Query, State},
    response::{Html, IntoResponse, Response},
    routing::get,
};
use chrono::{DateTime, TimeDelta, Utc};
use itertools::Itertools;
use log::warn;
use mini_moka::sync::Cache;
use minijinja::context;
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use std::{sync::Arc, time::Duration};
use tower_sessions::Session;
use vzdv::{
    GENERAL_HTTP_CLIENT,
    aviation::{highlight_metar, parse_metar},
    charts::{AirportCharts, ChartCycle, fetch_airport_charts},
};

/// How long a looked-up METAR is reused.
const WEATHER_TTL: Duration = Duration::from_secs(120);
/// How long to remember that an airport has no charts.
const MISSING_CHARTS_TTL: TimeDelta = TimeDelta::minutes(10);
/// How often to recheck charts that AviationAPI hasn't moved to the current cycle yet.
const STALE_CHARTS_TTL: TimeDelta = TimeDelta::hours(1);
const LOOKUP_TIMEOUT: Duration = Duration::from_secs(10);

/// Recent lookups, so repeating one doesn't hit the upstream APIs.
pub struct LookupCache {
    weather: Cache<String, Option<WeatherLookup>>,
    charts: Cache<String, CachedCharts>,
}

impl Default for LookupCache {
    fn default() -> Self {
        Self {
            weather: Cache::builder()
                .max_capacity(500)
                .time_to_live(WEATHER_TTL)
                .build(),
            charts: Cache::builder().max_capacity(500).build(),
        }
    }
}

#[derive(Clone)]
struct CachedCharts {
    charts: Option<Arc<AirportCharts>>,
    expires: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize)]
struct WeatherLookup {
    icao: String,
    #[serde(flatten)]
    summary: WeatherSummary,
}

#[derive(Deserialize)]
struct LookupQuery {
    #[serde(default)]
    icao: String,
}

/// Uppercase a typed airport identifier, or `None` if it can't be one.
fn normalize(input: &str) -> Option<String> {
    let ident = input.trim().to_uppercase();
    (ident.chars().all(|c| c.is_ascii_alphanumeric()) && (3..=4).contains(&ident.len()))
        .then_some(ident)
}

/// Identifiers to try, in order: "DEN" is probably KDEN, but could be an FAA
/// identifier like 1V6.
fn candidates(ident: &str) -> Vec<String> {
    if ident.len() == 3 {
        vec![format!("K{ident}"), ident.to_string()]
    } else {
        vec![ident.to_string()]
    }
}

/// A short message in place of a lookup result.
fn message(status: StatusCode, text: &'static str) -> Response {
    (
        status,
        Html(format!("<p class=\"text-warning small mb-0\">{text}</p>")),
    )
        .into_response()
}

/// The response to send if the user can't use the IDS.
async fn denied(session: &Session) -> Result<Option<Response>, AppError> {
    let user_info: Option<UserInfo> = session.get(SESSION_USER_INFO_KEY).await?;
    Ok(roster_access(&user_info).err().map(|status| {
        message(
            status,
            "IDS access requires an active roster membership. Sign in and reload.",
        )
    }))
}

/// One station's current METAR from VATSIM, with spacing normalized.
async fn fetch_metar(ident: &str) -> anyhow::Result<Option<String>> {
    let resp = GENERAL_HTTP_CLIENT
        .get(format!("https://metar.vatsim.net/{ident}"))
        .timeout(LOOKUP_TIMEOUT)
        .send()
        .await?;
    if !resp.status().is_success() {
        bail!("VATSIM METAR API returned status {}", resp.status());
    }
    let text = resp.text().await?;
    // the API matches by prefix, so KDE also returns KDED, KDEH, ...
    Ok(text
        .lines()
        .find(|line| line.split_whitespace().next() == Some(ident))
        .map(|line| line.split_whitespace().join(" ")))
}

/// Summarize a METAR, leaving out what can't be read from non-US formats.
fn summarize_metar(metar: &str) -> WeatherSummary {
    let Ok(weather) = parse_metar(metar) else {
        return WeatherSummary {
            raw_metar: Some(metar.to_string()),
            metar_parts: highlight_metar(metar),
            ..Default::default()
        };
    };
    let mut summary = WeatherSummary::new(Some(&weather));
    let mut groups = metar.split_whitespace();
    // flight rules need the visibility in statute miles
    if !groups.clone().any(|group| group.ends_with("SM")) {
        summary.conditions = None;
    }
    if summary.altimeter.is_none() {
        summary.altimeter = groups
            .find(|group| {
                group.len() == 5
                    && group.starts_with('Q')
                    && group[1..].bytes().all(|b| b.is_ascii_digit())
            })
            .map(String::from);
    }
    summary
}

/// Show the current METAR for any airport.
async fn lookup_weather(
    State(state): State<Arc<AppState>>,
    session: Session,
    Query(query): Query<LookupQuery>,
) -> Result<Response, AppError> {
    if let Some(response) = denied(&session).await? {
        return Ok(response);
    }
    let Some(ident) = normalize(&query.icao) else {
        return Ok(message(
            StatusCode::BAD_REQUEST,
            "Enter a 3 or 4 character airport identifier.",
        ));
    };
    let weather = match state.ids_lookups.weather.get(&ident) {
        Some(cached) => cached,
        None => {
            let mut found = None;
            for candidate in candidates(&ident) {
                match fetch_metar(&candidate).await {
                    Ok(Some(metar)) => {
                        found = Some(WeatherLookup {
                            summary: summarize_metar(&metar),
                            icao: candidate,
                        });
                        break;
                    }
                    Ok(None) => {}
                    Err(e) => {
                        warn!("Could not look up the METAR for {candidate}: {e}");
                        return Ok(message(
                            StatusCode::BAD_GATEWAY,
                            "Weather is unavailable right now. Try again shortly.",
                        ));
                    }
                }
            }
            state
                .ids_lookups
                .weather
                .insert(ident.clone(), found.clone());
            found
        }
    };
    let template = state.templates.get_template("ids/_weather_lookup.jinja")?;
    let rendered = template.render(context! { ident, weather })?;
    Ok(Html(rendered).into_response())
}

/// Keep charts until the next cycle, but look again sooner if there were none
/// or AviationAPI hadn't moved to the current cycle yet.
fn charts_expiry(charts: Option<&AirportCharts>, now: DateTime<Utc>) -> DateTime<Utc> {
    let cycle = ChartCycle::containing(now);
    let recheck = match charts {
        None => Some(MISSING_CHARTS_TTL),
        Some(charts) if charts.cycle.is_some_and(|c| c < cycle) => Some(STALE_CHARTS_TTL),
        Some(_) => None,
    };
    recheck.map_or(cycle.end(), |ttl| (now + ttl).min(cycle.end()))
}

/// Show an airport's charts.
async fn lookup_charts(
    State(state): State<Arc<AppState>>,
    session: Session,
    Query(query): Query<LookupQuery>,
) -> Result<Response, AppError> {
    if let Some(response) = denied(&session).await? {
        return Ok(response);
    }
    let Some(ident) = normalize(&query.icao) else {
        return Ok(message(
            StatusCode::BAD_REQUEST,
            "Enter a 3 or 4 character airport identifier.",
        ));
    };
    let now = Utc::now();
    let cached = state
        .ids_lookups
        .charts
        .get(&ident)
        .filter(|cached| now < cached.expires);
    let charts = match cached {
        Some(cached) => cached.charts,
        None => {
            let mut found = None;
            for candidate in candidates(&ident) {
                match fetch_airport_charts(&candidate).await {
                    Ok(Some(charts)) => {
                        found = Some(Arc::new(charts));
                        break;
                    }
                    Ok(None) => {}
                    Err(e) => {
                        warn!("Could not look up charts for {candidate}: {e}");
                        return Ok(message(
                            StatusCode::BAD_GATEWAY,
                            "Charts are unavailable right now. Try again shortly.",
                        ));
                    }
                }
            }
            let expires = charts_expiry(found.as_deref(), now);
            state.ids_lookups.charts.insert(
                ident.clone(),
                CachedCharts {
                    charts: found.clone(),
                    expires,
                },
            );
            found
        }
    };
    let current = ChartCycle::containing(now);
    let cycle = charts.as_ref().and_then(|c| c.cycle);
    let template = state.templates.get_template("ids/_chart_lookup.jinja")?;
    let rendered = template.render(context! {
        ident,
        charts,
        cycle => cycle.unwrap_or(current).ident(),
        awaiting_cycle => cycle.filter(|c| *c < current).map(|_| current.ident()),
    })?;
    Ok(Html(rendered).into_response())
}

/// This file's routes, merged into the IDS router.
pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/ids/lookup/weather", get(lookup_weather))
        .route("/ids/lookup/charts", get(lookup_charts))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::endpoints::ids::tests::{body_text, register_user, request, session, state, user};
    use vzdv::charts::{Chart, ChartGroup};

    fn time(text: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(text).unwrap().to_utc()
    }

    fn kden_charts(cycle: Option<ChartCycle>) -> AirportCharts {
        let chart = |name: &str| Chart {
            name: name.to_string(),
            url: format!("https://charts.example/{name}.pdf"),
        };
        AirportCharts {
            ident: "KDEN".into(),
            name: "DENVER INTL".into(),
            location: "DENVER, CO".into(),
            diagrams: vec![chart("AIRPORT DIAGRAM")],
            groups: vec![ChartGroup {
                label: "Arrivals".into(),
                charts: vec![chart("<LANDR> FOUR")],
            }],
            cycle,
        }
    }

    #[test]
    fn identifiers_are_normalized() {
        assert_eq!(normalize(" kden ").as_deref(), Some("KDEN"));
        assert_eq!(normalize("1v6").as_deref(), Some("1V6"));
        for bad in ["", "KD", "KDENX", "KD N", "K/EN", "KDÉN"] {
            assert_eq!(normalize(bad), None, "{bad}");
        }
        assert_eq!(candidates("DEN"), ["KDEN", "DEN"]);
        assert_eq!(candidates("KDEN"), ["KDEN"]);
    }

    #[test]
    fn metars_are_summarized() {
        let us = summarize_metar("KDEN 041853Z 22013G25KT 10SM FEW085 24/07 A3034");
        assert_eq!(us.conditions.as_deref(), Some("VFR"));
        assert_eq!(us.wind.as_deref(), Some("220@13G25"));
        assert_eq!(us.altimeter.as_deref(), Some("30.34"));

        // no flight category from metric visibility; QNH instead of an altimeter
        let metric = summarize_metar("EGLL 041850Z AUTO 29005KT 9999 NCD 19/09 Q1027");
        assert_eq!(metric.conditions, None);
        assert_eq!(metric.wind.as_deref(), Some("290@5"));
        assert_eq!(metric.altimeter.as_deref(), Some("Q1027"));

        let unreadable = summarize_metar("KXYZ 041850Z /////KT");
        assert_eq!(unreadable.wind, None);
        assert_eq!(
            unreadable.raw_metar.as_deref(),
            Some("KXYZ 041850Z /////KT")
        );
    }

    #[test]
    fn charts_are_kept_until_the_next_cycle() {
        let now = time("2026-10-04T19:00:00Z");
        let current = ChartCycle::containing(now);
        let previous = ChartCycle::containing(current.start - TimeDelta::days(1));
        assert_eq!(
            charts_expiry(Some(&kden_charts(Some(current))), now),
            time("2026-10-29T09:01:00Z")
        );
        assert_eq!(
            charts_expiry(Some(&kden_charts(None)), now),
            time("2026-10-29T09:01:00Z")
        );
        assert_eq!(
            charts_expiry(Some(&kden_charts(Some(previous))), now),
            now + STALE_CHARTS_TTL
        );
        assert_eq!(charts_expiry(None, now), now + MISSING_CHARTS_TTL);
        // never past the end of the cycle
        let late = time("2026-10-29T09:00:00Z");
        assert_eq!(charts_expiry(None, late), time("2026-10-29T09:01:00Z"));
    }

    #[tokio::test]
    async fn lookups_need_roster_access() {
        let state = state().await;
        for logged_in in [false, true] {
            let session = session();
            if logged_in {
                session
                    .insert(SESSION_USER_INFO_KEY, user(false, true))
                    .await
                    .unwrap();
            }
            for path in [
                "/ids/lookup/weather?icao=KDEN",
                "/ids/lookup/charts?icao=KDEN",
            ] {
                let response = request(&state, &session, path).await;
                assert_eq!(
                    response.status(),
                    if logged_in {
                        StatusCode::FORBIDDEN
                    } else {
                        StatusCode::UNAUTHORIZED
                    }
                );
                assert_eq!(response.headers()["cache-control"], "private, no-store");
                assert!(body_text(response).await.contains("roster membership"));
            }
        }
    }

    async fn roster_session(state: &AppState) -> Session {
        register_user(state, true, "ZDV").await;
        let session = session();
        session
            .insert(SESSION_USER_INFO_KEY, user(true, true))
            .await
            .unwrap();
        session
    }

    #[tokio::test]
    async fn invalid_identifiers_are_rejected() {
        let state = state().await;
        let session = roster_session(&state).await;
        for path in [
            "/ids/lookup/weather?icao=K%3CX",
            "/ids/lookup/charts?icao=",
            "/ids/lookup/charts",
        ] {
            let response = request(&state, &session, path).await;
            assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{path}");
            assert!(body_text(response).await.contains("airport identifier"));
        }
    }

    #[tokio::test]
    async fn cached_lookups_render() {
        let state = state().await;
        let session = roster_session(&state).await;
        state.ids_lookups.weather.insert(
            "DEN".into(),
            Some(WeatherLookup {
                icao: "KDEN".into(),
                summary: summarize_metar("KDEN 041853Z 22013KT 10SM FEW085 24/07 A3034"),
            }),
        );
        state.ids_lookups.weather.insert("ZZZ".into(), None);
        state.ids_lookups.weather.insert(
            "KCOS".into(),
            Some(WeatherLookup {
                icao: "KCOS".into(),
                summary: summarize_metar("KCOS 041854Z 15017KT 1 1/2SM BR OVC008 A3044"),
            }),
        );
        let html =
            body_text(request(&state, &session, "/ids/lookup/weather?icao=KCOS").await).await;
        assert!(html.contains("IFR"), "{html}");
        assert!(
            html.contains("15017KT <span class=\"metar-ifr\" title=\"IFR\">1 1&#x2f;2SM</span> BR <span class=\"metar-ifr\" title=\"IFR\">OVC008</span> A3044"),
            "{html}"
        );
        let html = body_text(request(&state, &session, "/ids/lookup/weather?icao=den").await).await;
        assert!(html.contains("KDEN 041853Z"), "{html}");
        assert!(html.contains("220@13 · 30.34"), "{html}");
        let html = body_text(request(&state, &session, "/ids/lookup/weather?icao=ZZZ").await).await;
        assert!(html.contains("No METAR for ZZZ"), "{html}");

        let now = Utc::now();
        let current = ChartCycle::containing(now);
        let previous = ChartCycle::containing(current.start - TimeDelta::days(1));
        for (ident, cycle) in [("KDEN", current), ("DEN", previous)] {
            state.ids_lookups.charts.insert(
                ident.into(),
                CachedCharts {
                    charts: Some(Arc::new(kden_charts(Some(cycle)))),
                    expires: now + TimeDelta::hours(1),
                },
            );
        }
        let html = body_text(request(&state, &session, "/ids/lookup/charts?icao=kden").await).await;
        assert!(html.contains("AIRPORT DIAGRAM.pdf\" target=\"_blank\""));
        assert!(html.contains("&lt;LANDR&gt; FOUR"));
        assert!(html.contains("data-chart-filter"));
        assert!(html.contains(&format!("AIRAC {}", current.ident())));
        assert!(!html.contains("isn't available yet"));
        let html = body_text(request(&state, &session, "/ids/lookup/charts?icao=DEN").await).await;
        assert!(html.contains(&format!("AIRAC {}", previous.ident())));
        assert!(html.contains("isn't available yet"));

        state.ids_lookups.charts.insert(
            "ZZZZ".into(),
            CachedCharts {
                charts: None,
                expires: now + TimeDelta::hours(1),
            },
        );
        let html = body_text(request(&state, &session, "/ids/lookup/charts?icao=ZZZZ").await).await;
        assert!(html.contains("No charts found for ZZZZ"));
    }
}
