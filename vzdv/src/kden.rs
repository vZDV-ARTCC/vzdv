//! Data about KDEN.

use crate::{
    GENERAL_HTTP_CLIENT,
    aviation::{AirportWeather, wind_between},
};
use anyhow::{Result, bail};
use scraper::{Html, Selector};
use serde::Serialize;

/// KDEN runway configurations.
#[derive(Debug, PartialEq)]
pub enum DenverConfig {
    NorthCalm,
    SouthCalm,
    NorthEast,
    NorthWest,
    SouthEast,
    SouthWest,
    NorthAll,
    SouthAll,
    EastAll,
    WestAll,
}

impl DenverConfig {
    pub fn name(&self) -> &'static str {
        match self {
            DenverConfig::NorthCalm => "North Calm",
            DenverConfig::SouthCalm => "South Calm",
            DenverConfig::NorthEast => "North East",
            DenverConfig::NorthWest => "North West",
            DenverConfig::SouthEast => "South East",
            DenverConfig::SouthWest => "South West",
            DenverConfig::NorthAll => "North All",
            DenverConfig::SouthAll => "South All",
            DenverConfig::EastAll => "East All",
            DenverConfig::WestAll => "West All",
        }
    }

    pub fn departing(&self) -> &'static str {
        match self {
            DenverConfig::NorthCalm => "8, 25, 34L",
            DenverConfig::SouthCalm => "8, 25, 16L or 17L",
            DenverConfig::NorthEast => "7, 35L, 35R",
            DenverConfig::NorthWest => "25, 34L",
            DenverConfig::SouthEast => "8, 16L or 17L",
            DenverConfig::SouthWest => "17L, 17R, 25",
            DenverConfig::NorthAll => "34L, 34R",
            DenverConfig::SouthAll => "Depends",
            DenverConfig::EastAll => "8",
            DenverConfig::WestAll => "25",
        }
    }

    pub fn landing(&self) -> &'static str {
        match self {
            DenverConfig::NorthCalm => "34R, 35L, 35R",
            DenverConfig::SouthCalm => "16L, 16R, 17R",
            DenverConfig::NorthEast => "34R, 35L, 35R",
            DenverConfig::NorthWest => "26, 34R, 35L, 35R",
            DenverConfig::SouthEast => "7, 16L, 16R, 17R",
            DenverConfig::SouthWest => "16L, 16R, 26",
            DenverConfig::NorthAll => "35L, 35R",
            DenverConfig::SouthAll => "Depends",
            DenverConfig::EastAll => "7, 8",
            DenverConfig::WestAll => "25, 26",
        }
    }
}

/// Determine the likiest KDEN runway configuration based on the weather.
pub fn determine_runway_config(weather: &AirportWeather) -> DenverConfig {
    let dir = weather.wind.0;
    let mag = std::cmp::max(weather.wind.1, weather.wind.2); // use gust if present

    if wind_between(dir, 260, 79) && mag <= 10 {
        DenverConfig::NorthCalm
    } else if wind_between(dir, 80, 295) && mag <= 10 {
        DenverConfig::SouthCalm
    } else if wind_between(dir, 350, 79) && (11..=25).contains(&mag) {
        DenverConfig::NorthEast
    } else if wind_between(dir, 260, 349) && (11..=25).contains(&mag) {
        DenverConfig::NorthWest
    } else if wind_between(dir, 80, 169) && (11..=25).contains(&mag) {
        DenverConfig::SouthEast
    } else if wind_between(dir, 170, 259) && (11..=25).contains(&mag) {
        DenverConfig::SouthWest
    } else if wind_between(dir, 300, 39) && mag > 25 {
        DenverConfig::NorthAll
    } else if wind_between(dir, 120, 219) && mag > 25 {
        DenverConfig::SouthAll
    } else if wind_between(dir, 40, 119) && mag > 25 {
        DenverConfig::EastAll
    } else if wind_between(dir, 220, 299) && mag > 25 {
        DenverConfig::WestAll
    } else {
        DenverConfig::NorthCalm // fallback
    }
}

#[derive(Debug, Serialize)]
pub struct WindComponent {
    pub runway: String,
    pub head: u8,
    pub tail: u8,
    pub cross: u8,
}

impl From<(&'static str, f32, f32, f32)> for WindComponent {
    fn from(value: (&'static str, f32, f32, f32)) -> Self {
        WindComponent {
            runway: value.0.to_owned(),
            head: value.1 as u8,
            tail: value.2 as u8,
            cross: value.3 as u8,
        }
    }
}

static RUNWAYS: [(&str, f32); 12] = [
    ("07", 83.0),
    ("25", 263.0),
    ("34L", 353.0),
    ("34R", 353.0),
    ("16R", 173.0),
    ("16L", 173.0),
    ("08", 83.0),
    ("26", 263.1),
    ("35L", 353.0),
    ("35R", 353.0),
    ("17R", 173.0),
    ("17L", 173.0),
];

/// Determine the wind components for all the runways.
pub fn wind_components(weather: &AirportWeather) -> Vec<WindComponent> {
    let dir = weather.wind.0 as f32;
    let mag = std::cmp::max(weather.wind.1, weather.wind.2) as f32; // use gust if present

    RUNWAYS
        .iter()
        .map(|(name, heading)| {
            let angle = (dir - heading).to_radians();
            let cross = mag * angle.sin();
            let head = mag * angle.cos();
            let tail = if head < 0.0 { -head } else { 0.0 };
            let head = if head > 0.0 { head } else { 0.0 };
            (*name, head, tail, cross).into()
        })
        .collect()
}

/// Using a third-party site, fetch and parse the runway assignments
/// by SID.
pub async fn fetch_runway_assignments() -> Result<Vec<Vec<String>>> {
    let resp = GENERAL_HTTP_CLIENT
        .get("https://den.aerobahn.com/ids4.html")
        .send()
        .await?;
    if !resp.status().is_success() {
        bail!(
            "HTTP {} from runway assignment page",
            resp.status().as_u16()
        );
    }
    let document = Html::parse_document(&resp.text().await?);
    let rows: Vec<_> = document
        .select(&Selector::parse("tr").unwrap())
        .skip(1)
        .map(|row| {
            let cells: Vec<_> = row
                .select(&Selector::parse("td").unwrap())
                .take(3)
                .map(|cell| cell.inner_html())
                .collect();
            cells
        })
        .collect();
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::{DenverConfig, determine_runway_config};
    use crate::aviation::parse_metar;

    #[test]
    fn test_determine_runway_config() {
        let mut weather = parse_metar("KDEN 030253Z 00000KT 10SM SCT100 BKN160 13/M12 A2943 RMK AO2 PK WND 21036/0211 SLP924 T01331117 58005").unwrap();
        assert_eq!(determine_runway_config(&weather), DenverConfig::NorthCalm);

        weather.wind = (250, 10, 0);
        assert_eq!(determine_runway_config(&weather), DenverConfig::SouthCalm);

        weather.wind = (189, 11, 22);
        assert_eq!(determine_runway_config(&weather), DenverConfig::SouthWest);

        weather.wind = (210, 20, 35);
        assert_eq!(determine_runway_config(&weather), DenverConfig::SouthAll);
    }
}
