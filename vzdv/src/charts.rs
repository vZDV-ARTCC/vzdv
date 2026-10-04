//! FAA chart listings from AviationAPI, which change with the 28-day chart cycle.

use crate::GENERAL_HTTP_CLIENT;
use anyhow::{Result, bail};
use chrono::{DateTime, Datelike, NaiveDate, NaiveTime, TimeDelta, TimeZone, Utc};
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use std::time::Duration;

const CHARTS_API: &str = "https://api-v2.aviationapi.com/v2/charts";
const CYCLE_LENGTH: TimeDelta = TimeDelta::days(28);
/// Charts take effect at 0901Z on the cycle's first day.
const CYCLE_START_TIME: NaiveTime = NaiveTime::from_hms_opt(9, 1, 0).unwrap();

/// Chart groups after the airport diagram, in display order.
const GROUPS: [(&str, &str); 4] = [
    ("departure", "Departures"),
    ("arrival", "Arrivals"),
    ("approach", "Approaches"),
    ("general", "General"),
];
const DIAGRAM_GROUP: &str = "airport_diagram";

/// A 28-day AIRAC chart cycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct ChartCycle {
    pub start: DateTime<Utc>,
}

impl ChartCycle {
    /// The cycle in effect at a time.
    pub fn containing(time: DateTime<Utc>) -> Self {
        // AIRAC 2001; every later cycle starts a multiple of 28 days after it
        let epoch = Utc.with_ymd_and_hms(2020, 1, 2, 9, 1, 0).unwrap();
        let cycles = (time - epoch)
            .num_seconds()
            .div_euclid(CYCLE_LENGTH.num_seconds());
        Self {
            start: epoch + CYCLE_LENGTH * cycles as i32,
        }
    }

    /// When the next cycle takes effect.
    pub fn end(&self) -> DateTime<Utc> {
        self.start + CYCLE_LENGTH
    }

    /// The AIRAC identifier, e.g. "2610" for the 10th cycle of 2026.
    pub fn ident(&self) -> String {
        let mut number = 1;
        let mut earlier = self.start - CYCLE_LENGTH;
        while earlier.year() == self.start.year() {
            number += 1;
            earlier -= CYCLE_LENGTH;
        }
        format!("{:02}{number:02}", self.start.year() % 100)
    }
}

/// A single chart's PDF.
#[derive(Debug, Clone, Serialize)]
pub struct Chart {
    pub name: String,
    pub url: String,
}

/// One kind of chart, e.g. arrivals.
#[derive(Debug, Clone, Serialize)]
pub struct ChartGroup {
    pub label: String,
    pub charts: Vec<Chart>,
}

/// An airport's published charts.
#[derive(Debug, Clone, Serialize)]
pub struct AirportCharts {
    /// ICAO identifier, or the FAA identifier for airports without one.
    pub ident: String,
    pub name: String,
    pub location: String,
    pub diagrams: Vec<Chart>,
    /// Non-empty groups in display order.
    pub groups: Vec<ChartGroup>,
    /// The cycle the chart PDFs are from, if it could be determined.
    #[serde(skip)]
    pub cycle: Option<ChartCycle>,
}

#[derive(Deserialize)]
struct ApiResponse {
    airport_data: ApiAirport,
    #[serde(default)]
    charts: IndexMap<String, Vec<ApiChart>>,
}

#[derive(Deserialize)]
struct ApiAirport {
    icao_ident: Option<String>,
    #[serde(default)]
    faa_ident: String,
    #[serde(default)]
    airport_name: String,
    #[serde(default)]
    city: String,
    #[serde(default)]
    state_abbr: String,
}

#[derive(Deserialize)]
struct ApiChart {
    chart_name: String,
    pdf_url: String,
}

/// Title-case an unknown group key, e.g. "hot_spot" to "Hot spot".
fn group_label(key: &str) -> String {
    let words = key.replace('_', " ");
    let mut chars = words.chars();
    chars
        .next()
        .map(|first| first.to_uppercase().chain(chars).collect())
        .unwrap_or_default()
}

/// The cycle a chart PDF belongs to, from its URL, e.g. `.../261001/09077ad.pdf`.
fn url_cycle(url: &str) -> Option<ChartCycle> {
    let segment = url.rsplit('/').nth(1)?;
    let date = NaiveDate::parse_from_str(segment, "%y%m%d").ok()?;
    Some(ChartCycle::containing(
        date.and_time(CYCLE_START_TIME).and_utc(),
    ))
}

/// Convert the API's charts, skipping any that aren't plain HTTPS links.
fn convert(charts: Vec<ApiChart>) -> Vec<Chart> {
    charts
        .into_iter()
        .filter(|chart| chart.pdf_url.starts_with("https://"))
        .map(|chart| Chart {
            name: chart.chart_name,
            url: chart.pdf_url,
        })
        .collect()
}

/// Remove a group from the response.
fn take_group(charts: &mut IndexMap<String, Vec<ApiChart>>, key: &str) -> Vec<Chart> {
    convert(charts.shift_remove(key).unwrap_or_default())
}

impl From<ApiResponse> for AirportCharts {
    fn from(mut data: ApiResponse) -> Self {
        let diagrams = take_group(&mut data.charts, DIAGRAM_GROUP);
        let mut groups: Vec<ChartGroup> = GROUPS
            .iter()
            .map(|(key, label)| ChartGroup {
                label: label.to_string(),
                charts: take_group(&mut data.charts, key),
            })
            .collect();
        // anything AviationAPI adds later goes at the end
        groups.extend(data.charts.into_iter().map(|(key, charts)| ChartGroup {
            label: group_label(&key),
            charts: convert(charts),
        }));
        groups.retain(|group| !group.charts.is_empty());
        let cycle = diagrams
            .iter()
            .chain(groups.iter().flat_map(|group| &group.charts))
            .find_map(|chart| url_cycle(&chart.url));

        let airport = data.airport_data;
        let location = [airport.city, airport.state_abbr]
            .into_iter()
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join(", ");
        Self {
            ident: airport
                .icao_ident
                .filter(|icao| !icao.is_empty())
                .unwrap_or(airport.faa_ident),
            name: airport.airport_name,
            location,
            diagrams,
            groups,
            cycle,
        }
    }
}

/// Fetch an airport's charts by ICAO or FAA identifier. `None` if AviationAPI
/// doesn't know the airport.
pub async fn fetch_airport_charts(ident: &str) -> Result<Option<AirportCharts>> {
    let resp = GENERAL_HTTP_CLIENT
        .get(format!("{CHARTS_API}?airport={ident}"))
        .timeout(Duration::from_secs(10))
        .send()
        .await?;
    // AviationAPI answers unknown airports with a 500
    if matches!(resp.status().as_u16(), 404 | 500) {
        return Ok(None);
    }
    if !resp.status().is_success() {
        bail!("AviationAPI returned status {}", resp.status());
    }
    let data: ApiResponse = resp.json().await?;
    Ok(Some(data.into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn time(text: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(text).unwrap().to_utc()
    }

    #[test]
    fn cycles_follow_the_airac_schedule() {
        let cycle = ChartCycle::containing(time("2026-10-04T19:00:00Z"));
        assert_eq!(cycle.start, time("2026-10-01T09:01:00Z"));
        assert_eq!(cycle.end(), time("2026-10-29T09:01:00Z"));
        assert_eq!(cycle.ident(), "2610");

        // charts change at 0901Z, not midnight
        let before = ChartCycle::containing(time("2026-10-01T09:00:59Z"));
        assert_eq!(before.ident(), "2609");
        assert_eq!(before.end(), cycle.start);

        assert_eq!(
            ChartCycle::containing(time("2026-01-22T12:00:00Z")).ident(),
            "2601"
        );
        assert_eq!(
            ChartCycle::containing(time("2026-01-21T12:00:00Z")).ident(),
            "2513"
        );
        assert_eq!(
            ChartCycle::containing(time("2020-01-02T09:01:00Z")).ident(),
            "2001"
        );
        assert_eq!(
            ChartCycle::containing(time("2019-12-31T00:00:00Z")).ident(),
            "1913"
        );
        // 2020 had 14 cycles
        assert_eq!(
            ChartCycle::containing(time("2020-12-31T12:00:00Z")).ident(),
            "2014"
        );
    }

    #[test]
    fn api_response_is_grouped_in_display_order() {
        let data: ApiResponse = serde_json::from_value(serde_json::json!({
            "airport_data": {
                "city": "DENVER", "state_abbr": "CO", "icao_ident": "KDEN",
                "faa_ident": "DEN", "airport_name": "DENVER INTL"
            },
            "charts": {
                "airport_diagram": [{"chart_name": "AIRPORT DIAGRAM", "pdf_url": "https://charts-v2.aviationapi.com/261001/09077ad.pdf"}],
                "general": [{"chart_name": "TAKEOFF MINIMUMS", "pdf_url": "https://charts-v2.aviationapi.com/261001/sw1to.pdf"}],
                "approach": [{"chart_name": "ILS OR LOC RWY 07", "pdf_url": "https://charts-v2.aviationapi.com/261001/09077il7.pdf"}],
                "arrival": [],
                "departure": [{"chart_name": "BAYLR SIX (RNAV)", "pdf_url": "https://charts-v2.aviationapi.com/261001/09077baylr.pdf"}],
                "hot_spot": [{"chart_name": "HOT SPOT", "pdf_url": "https://charts-v2.aviationapi.com/261001/hs.pdf"}]
            }
        }))
        .unwrap();
        let charts = AirportCharts::from(data);
        assert_eq!(charts.ident, "KDEN");
        assert_eq!(charts.location, "DENVER, CO");
        assert_eq!(charts.diagrams.len(), 1);
        let labels: Vec<_> = charts.groups.iter().map(|g| g.label.as_str()).collect();
        assert_eq!(labels, ["Departures", "Approaches", "General", "Hot spot"]);
        assert_eq!(charts.cycle.unwrap().ident(), "2610");
    }

    #[test]
    fn airports_without_icao_use_the_faa_identifier() {
        let data: ApiResponse = serde_json::from_value(serde_json::json!({
            "airport_data": {"city": "CANON CITY", "state_abbr": "CO", "faa_ident": "1V6", "airport_name": "FREMONT COUNTY"},
            "charts": {"airport_diagram": [], "approach": [
                {"chart_name": "RNAV (GPS) RWY 29", "pdf_url": "https://example.com/charts/00043r29.pdf"},
                {"chart_name": "BAD", "pdf_url": "javascript:alert(1)"}
            ]}
        }))
        .unwrap();
        let charts = AirportCharts::from(data);
        assert_eq!(charts.ident, "1V6");
        assert!(charts.diagrams.is_empty());
        assert_eq!(charts.groups.len(), 1);
        assert_eq!(charts.groups[0].charts.len(), 1);
        assert_eq!(charts.cycle, None);
    }
}
