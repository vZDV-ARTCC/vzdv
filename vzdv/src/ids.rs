#![allow(dead_code)]
use anyhow::Result;
use serde::Deserialize;
use std::collections::HashMap;

use crate::{
    aviation::{AirportWeather, WeatherConditions},
    sql::Atis,
};

#[derive(Deserialize, Debug, Clone)]
pub struct AirportProcedure {
    pub try_match: Option<TryMatchProcedure>,
    pub rules: Vec<FlowRule>,
    pub flows: HashMap<String, Flow>,
}

impl AirportProcedure {
    pub fn determine_flow(&self, weather: &AirportWeather, atis_list: &[Atis]) -> Result<&Flow> {
        let icao = format!("K{}", &weather.name);
        // If there's an ATIS up for this airport, just use that
        if let Some(atis) = atis_list.iter().find(|a| a.facility == icao) {
            return self.flows.get(&atis.preset).ok_or_else(|| {
                anyhow::anyhow!("Flow {} not found for airport {}", atis.preset, icao,)
            });
        }

        // If we have a procedure to try to "match" another airport like KAPA -> KDEN,
        // then see if they have an `Atis` up,
        // then find the corresponding flow
        // TODO: Determine based on IMC/VMC as well
        // if let Some(match_proc) = &self.try_match {
        //     let match_icao = &match_proc.icao;
        //     if let Some(matched_preset) = atis_list
        //         .iter()
        //         .find(|a| a.facility == match_proc.icao)
        //         .map(|flow| &flow.preset)
        //     {
        //         if let Some(selected_preset) = match_proc.match_flows.get(matched_preset) {
        //             return self
        //                 .flows.get(selected_preset)
        //                 .ok_or_else(|| anyhow!("Flow {} for {} was matched from {}, but is not present in rules config!", selected_preset, icao, match_icao));
        //         } else {
        //             bail!(
        //                 "Could not find matching flow for {}. Checked {}",
        //                 icao,
        //                 matched_preset
        //             );
        //         }
        //     }
        // }

        let wind_kts = if weather.wind.2 > 0 {
            weather.wind.2
        } else {
            weather.wind.1
        };
        let is_calm = wind_kts <= 1;
        let rule = self
            .rules
            .iter()
            .find(|rule| {
                let is_and_matches_calm = is_calm && rule.calm;
                let within_directional_bounds = rule
                    .direction_bounds
                    .as_ref()
                    .is_some_and(|r| r.is_within_bounds(weather.wind.0))
                    || rule.direction_bounds.is_none();
                let within_speed_bounds = rule
                    .speed_bounds
                    .as_ref()
                    .is_some_and(|r| r.is_within_bounds(wind_kts))
                    || rule.speed_bounds.is_none();
                let matches_conditions = rule.conds.contains(&weather.conditions);

                is_and_matches_calm
                    || (!rule.calm
                        && within_directional_bounds
                        && within_speed_bounds
                        && matches_conditions)
            })
            .ok_or_else(|| anyhow::anyhow!("No matching procedure rule found"))?;

        self.flows.get(&rule.use_flow).ok_or_else(|| {
            anyhow::anyhow!(
                "Flow {} not found for rule at {} {:?}",
                rule.use_flow,
                icao,
                rule
            )
        })
    }
}

#[derive(Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct TryMatchProcedure {
    pub icao: String,
    pub match_flows: HashMap<String, String>,
}

#[derive(Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Flow {
    pub name: String,
    pub dep_rwys: Vec<String>,
    pub arr_rwys: Vec<String>,
}

#[derive(Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct FlowRule {
    #[serde(default)]
    pub calm: bool,
    pub conds: Vec<WeatherConditions>,
    pub use_flow: String,
    pub direction_bounds: Option<WindDirectionBounds>,
    pub speed_bounds: Option<WindSpeedBounds>,
}

#[derive(Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct WindSpeedBounds {
    min_kts: u8,
    max_kts: u8,
}

impl WindSpeedBounds {
    #[inline]
    pub fn is_within_bounds(&self, wind_kts: u8) -> bool {
        wind_kts >= self.min_kts && wind_kts <= self.max_kts
    }
}

#[derive(Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct WindDirectionBounds {
    pub wind_from: u16,
    pub clock_dir: ClockDir,
    pub wind_to: u16,
}

impl WindDirectionBounds {
    #[inline]
    pub fn is_within_bounds(&self, wind_dir: u16) -> bool {
        // If clock_dir is CW, then consider range (wind_from..=wind_to)
        // If clock_dir is CCW, then consider range (wind_to..=wind_from)
        // Adding 360 if we know the wind dir wraps around 360

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

#[cfg(test)]
mod tests {
    use chrono::Utc;

    use crate::config::ConfigIDS;
    use std::fs;

    use super::*;

    fn load_config() -> ConfigIDS {
        let file_s = fs::read_to_string("../ids.json").unwrap();
        serde_json::from_str(&file_s).unwrap()
    }

    /// Even if winds favor another flow, choose whichever vATIS has sent
    #[test]
    fn atis_override() {
        let config = load_config();
        let procedure = config.0.get("KAPA").unwrap();
        let weather = AirportWeather {
            ceiling: 0,
            conditions: WeatherConditions::VFR,
            name: "APA".into(),
            raw: "".into(),
            visibility: 10,
            wind: (180, 5, 10),
        };
        let atis = Atis {
            airport_conditions: "".into(),
            atis_letter: "A".into(),
            atis_type: "".into(),
            facility: "KAPA".into(),
            id: 0,
            notams: "".into(),
            preset: "NORTH VMC".into(),
            timestamp: Utc::now(),
            version: "".into(),
        };

        let flow = procedure.determine_flow(&weather, &[atis]).unwrap();
        assert_eq!(flow.name, "NORTH VMC")
    }

    #[test]
    fn flow_from_calm_winds() {
        let config = load_config();
        let procedure = config.0.get("KAPA").unwrap();
        let weather = AirportWeather {
            ceiling: 0,
            conditions: WeatherConditions::VFR,
            name: "APA".into(),
            raw: "".into(),
            visibility: 10,
            wind: (350, 1, 0),
        };

        let flow = procedure.determine_flow(&weather, &[]).unwrap();
        assert_eq!(flow.name, "SOUTH VMC")
    }

    #[test]
    fn flow_from_non_calm_winds() {
        let config = load_config();
        let procedure = config.0.get("KAPA").unwrap();
        let weather = AirportWeather {
            ceiling: 0,
            conditions: WeatherConditions::VFR,
            name: "APA".into(),
            raw: "".into(),
            visibility: 10,
            wind: (350, 5, 10),
        };

        let flow = procedure.determine_flow(&weather, &[]).unwrap();
        assert_eq!(flow.name, "NORTH VMC")
    }

    #[test]
    fn dir_bounds_cw_north_wrap() {
        let bounds = WindDirectionBounds {
            wind_from: 260,
            clock_dir: ClockDir::Clockwise,
            wind_to: 79,
        };

        assert!(bounds.is_within_bounds(350));
        assert!(bounds.is_within_bounds(0));
        assert!(!bounds.is_within_bounds(180));
    }

    // ASE Tests

    #[test]
    fn ase_vmc_flow() {
        let config = load_config();
        let procedure = config.0.get("KASE").unwrap();
        let weather = AirportWeather {
            ceiling: 0,
            conditions: WeatherConditions::VFR,
            name: "ASE".into(),
            raw: "".into(),
            visibility: 10,
            wind: (350, 5, 9),
        };

        let flow = procedure.determine_flow(&weather, &[]).unwrap();
        assert_eq!(flow.name, "VMC")
    }

    #[test]
    fn ase_vmc_15_tw_flow() {
        let config = load_config();
        let procedure = config.0.get("KASE").unwrap();
        let weather = AirportWeather {
            ceiling: 0,
            conditions: WeatherConditions::VFR,
            name: "ASE".into(),
            raw: "".into(),
            visibility: 10,
            wind: (350, 5, 15),
        };

        let flow = procedure.determine_flow(&weather, &[]).unwrap();
        assert_eq!(flow.name, "VMC 15 TAILWIND")
    }

    #[test]
    fn ase_vmc_33_tw_flow() {
        let config = load_config();
        let procedure = config.0.get("KASE").unwrap();
        let weather = AirportWeather {
            ceiling: 0,
            conditions: WeatherConditions::VFR,
            name: "ASE".into(),
            raw: "".into(),
            visibility: 10,
            wind: (150, 5, 15),
        };

        let flow = procedure.determine_flow(&weather, &[]).unwrap();
        assert_eq!(flow.name, "VMC 33 TAILWIND")
    }

    #[test]
    fn ase_imc_flow() {
        let config = load_config();
        let procedure = config.0.get("KASE").unwrap();
        let weather = AirportWeather {
            ceiling: 0,
            conditions: WeatherConditions::IFR,
            name: "ASE".into(),
            raw: "".into(),
            visibility: 10,
            wind: (350, 5, 9),
        };

        let flow = procedure.determine_flow(&weather, &[]).unwrap();
        assert_eq!(flow.name, "IMC")
    }

    #[test]
    fn ase_imc_15_tw_flow() {
        let config = load_config();
        let procedure = config.0.get("KASE").unwrap();
        let weather = AirportWeather {
            ceiling: 0,
            conditions: WeatherConditions::IFR,
            name: "ASE".into(),
            raw: "".into(),
            visibility: 10,
            wind: (350, 5, 15),
        };

        let flow = procedure.determine_flow(&weather, &[]).unwrap();
        assert_eq!(flow.name, "IMC 15 TAILWIND")
    }

    #[test]
    fn ase_imc_33_tw_flow() {
        let config = load_config();
        let procedure = config.0.get("KASE").unwrap();
        let weather = AirportWeather {
            ceiling: 0,
            conditions: WeatherConditions::IFR,
            name: "ASE".into(),
            raw: "".into(),
            visibility: 10,
            wind: (150, 5, 15),
        };

        let flow = procedure.determine_flow(&weather, &[]).unwrap();
        assert_eq!(flow.name, "IMC 33 TAILWIND")
    }
}
