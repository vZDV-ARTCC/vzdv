#![allow(dead_code)]
use anyhow::{Result, anyhow, bail};
use serde::Deserialize;
use std::collections::HashMap;

use crate::{
    aviation::{AirportWeather, WeatherConditions},
    sql::Atis,
};

type WindDirection = u16;
/// This really shouldn't exceed 255kts lol
type WindSpeed = u8;
type CloudLayerAGL = u16;

#[derive(Deserialize, Debug, Clone)]
pub struct AirportProcedure {
    pub try_match: Option<TryMatchProcedure>,
    pub rules: Vec<FlowRule>,
}

impl AirportProcedure {
    pub fn determine_flow(
        &self,
        weather: &AirportWeather,
        atis_list: Vec<&Atis>,
    ) -> Result<&FlowRule> {
        // If we have a procedure to try to "match" another airport like KAPA -> KDEN,
        // then see if they have an `Atis` up,
        // then find the corresponding flow
        // TODO: Determine based on IMC/VMC as well
        if let Some(match_proc) = &self.try_match {
            let match_icao = &match_proc.icao;

            if let Some(matched_preset) = atis_list
                .iter()
                .find(|a| a.facility == match_proc.icao)
                .map(|flow| &flow.preset)
            {
                if let Some(selected_preset) = match_proc.match_flows.get(matched_preset) {
                    return self
                        .rules
                        .iter()
                        .find(|r| &r.flow_name == selected_preset)
                        .ok_or_else(|| anyhow!("Flow {} for {} was matched from {}, but is not present in rules config!", selected_preset, weather.name, match_icao));
                } else {
                    bail!(
                        "Could not find matching flow for {}. Checked {}",
                        weather.name,
                        matched_preset
                    );
                }
            }
        }

        let wind_kts = if weather.wind.2 > 0 {
            weather.wind.2
        } else {
            weather.wind.1
        };
        let is_calm = wind_kts <= 1;
        self.rules
            .iter()
            .find(|rule| {
                ((rule.calm && is_calm)
                    || rule
                        .bounds
                        .as_ref()
                        .is_some_and(|r| r.is_wind_within_bounds(weather.wind.0)))
                    && rule.conds.contains(&weather.conditions)
            })
            .ok_or_else(|| anyhow::anyhow!("No matching procedure rule found"))
    }
}

#[derive(Deserialize, Debug, Clone)]
pub struct TryMatchProcedure {
    pub icao: String,
    pub match_flows: HashMap<String, String>,
}

#[derive(Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct FlowRule {
    #[serde(default)]
    pub calm: bool,
    pub conds: Vec<WeatherConditions>,
    pub flow_name: String,
    pub bounds: Option<WindBounds>,
    pub dep_rwys: Vec<String>,
    pub arr_rwys: Vec<String>,
}

#[derive(Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct WindBounds {
    pub wind_from: WindDirection,
    pub clock_dir: ClockDir,
    pub wind_to: WindDirection,
}

impl WindBounds {
    pub fn is_wind_within_bounds(&self, wind_dir: WindDirection) -> bool {
        // If clock_dir is CW, then consider range (wind_from..=wind_to)
        // If clock_dir is CCW, then consider range (wind_to..=wind_from)
        // Adding 360 if we know if the wind dir wraps around 360

        let range = match self.clock_dir {
            ClockDir::Clockwise => {
                let lower_bound = self.wind_from;
                let upper_bound = if self.wind_to < self.wind_from {
                    self.wind_to + 360
                } else {
                    self.wind_to
                };
                lower_bound..=upper_bound
            }
            ClockDir::CounterClockwise => {
                let lower_bound = self.wind_to;
                let upper_bound = if self.wind_from < self.wind_to {
                    self.wind_from + 360
                } else {
                    self.wind_from
                };
                lower_bound..=upper_bound
            }
        };

        // The (+360) is solely for when the wind is exactly 000° (360°)
        range.contains(&wind_dir) || range.contains(&(wind_dir + 360))
    }
}

#[derive(Deserialize, Debug, Clone)]
pub enum ClockDir {
    #[serde(rename = "cw")]
    Clockwise,
    #[serde(rename = "ccw")]
    CounterClockwise,
}
