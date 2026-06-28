use anyhow::Result;
use serde::Deserialize;
use std::collections::HashMap;

use crate::{
    aviation::{AirportWeather, WeatherConditions},
    sql::Atis,
};

#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedFlow {
    pub dep_rwys: Vec<String>,
    pub arr_rwys: Vec<String>,
    pub dep_name: Option<String>,
    pub arr_name: Option<String>,
}

#[derive(Deserialize, Debug, Clone)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum AirportProcedure {
    Combined(CombinedProcedure),
    Split(SplitProcedure),
}

impl AirportProcedure {
    pub fn determine_flow(
        &self,
        weather: &AirportWeather,
        atis_list: &[Atis],
    ) -> Result<ResolvedFlow> {
        match self {
            AirportProcedure::Combined(proc) => proc.determine_flow(weather, atis_list),
            AirportProcedure::Split(proc) => proc.determine_flow(weather, atis_list),
        }
    }
}

#[derive(Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct CombinedProcedure {
    pub flows: HashMap<String, Flow>,
    pub rules: Vec<FlowRule>,
    #[serde(default)]
    pub try_match: Option<TryMatchProcedure>,
}

impl CombinedProcedure {
    pub fn determine_flow(
        &self,
        weather: &AirportWeather,
        atis_list: &[Atis],
    ) -> Result<ResolvedFlow> {
        let icao = format!("K{}", &weather.name);

        // If there's a combined ATIS up for this airport, use that
        if let Some(atis) = atis_list
            .iter()
            .find(|a| a.facility == icao && a.atis_type == "combined")
        {
            let flow = self.flows.get(&atis.preset).ok_or_else(|| {
                anyhow::anyhow!("Flow '{}' not found for airport {}", atis.preset, icao)
            })?;
            return Ok(ResolvedFlow {
                dep_rwys: flow.dep_rwys.clone(),
                arr_rwys: flow.arr_rwys.clone(),
                dep_name: Some(flow.name.clone()),
                arr_name: Some(flow.name.clone()),
            });
        }

        // Fall through to weather-based rules
        let rule = find_matching_rule(&self.rules, weather)?;
        let flow = self.flows.get(&rule.use_flow).ok_or_else(|| {
            anyhow::anyhow!(
                "Flow '{}' not found for rule at {} {:?}",
                rule.use_flow,
                icao,
                rule
            )
        })?;
        Ok(ResolvedFlow {
            dep_rwys: flow.dep_rwys.clone(),
            arr_rwys: flow.arr_rwys.clone(),
            dep_name: Some(flow.name.clone()),
            arr_name: Some(flow.name.clone()),
        })
    }
}

#[derive(Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct SplitProcedure {
    pub dep_flows: HashMap<String, SplitFlow>,
    pub arr_flows: HashMap<String, SplitFlow>,
    pub rules: Vec<SplitFlowRule>,
}

impl SplitProcedure {
    pub fn determine_flow(
        &self,
        weather: &AirportWeather,
        atis_list: &[Atis],
    ) -> Result<ResolvedFlow> {
        let icao = format!("K{}", &weather.name);
        let airport_atis: Vec<_> = atis_list.iter().filter(|a| a.facility == icao).collect();

        let dep_atis = airport_atis.iter().find(|a| a.atis_type == "departure");
        let arr_atis = airport_atis.iter().find(|a| a.atis_type == "arrival");

        // Resolve departure runways
        let (dep_rwys, dep_name) = if let Some(atis) = dep_atis {
            let flow = self.dep_flows.get(&atis.preset).ok_or_else(|| {
                anyhow::anyhow!(
                    "Departure flow '{}' not found for airport {}",
                    atis.preset,
                    icao
                )
            })?;
            (flow.rwys.clone(), Some(atis.preset.clone()))
        } else {
            // Weather-determine departure
            let rule = find_matching_split_rule(&self.rules, weather)?;
            let flow = self.dep_flows.get(&rule.use_dep_flow).ok_or_else(|| {
                anyhow::anyhow!(
                    "Departure flow '{}' not found for rule at {} {:?}",
                    rule.use_dep_flow,
                    icao,
                    rule
                )
            })?;
            (flow.rwys.clone(), Some(rule.use_dep_flow.clone()))
        };

        // Resolve arrival runways
        let (arr_rwys, arr_name) = if let Some(atis) = arr_atis {
            let flow = self.arr_flows.get(&atis.preset).ok_or_else(|| {
                anyhow::anyhow!(
                    "Arrival flow '{}' not found for airport {}",
                    atis.preset,
                    icao
                )
            })?;
            (flow.rwys.clone(), Some(atis.preset.clone()))
        } else {
            // Weather-determine arrival
            let rule = find_matching_split_rule(&self.rules, weather)?;
            let flow = self.arr_flows.get(&rule.use_arr_flow).ok_or_else(|| {
                anyhow::anyhow!(
                    "Arrival flow '{}' not found for rule at {} {:?}",
                    rule.use_arr_flow,
                    icao,
                    rule
                )
            })?;
            (flow.rwys.clone(), Some(rule.use_arr_flow.clone()))
        };

        Ok(ResolvedFlow {
            dep_rwys,
            arr_rwys,
            dep_name,
            arr_name,
        })
    }
}

#[derive(Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct SplitFlow {
    pub rwys: Vec<String>,
}

#[derive(Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct SplitFlowRule {
    #[serde(default)]
    pub calm: bool,
    pub conds: Vec<WeatherConditions>,
    pub use_dep_flow: String,
    pub use_arr_flow: String,
    pub direction_bounds: Option<WindDirectionBounds>,
    pub speed_bounds: Option<WindSpeedBounds>,
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

fn matches_wind_rule(
    weather: &AirportWeather,
    calm: bool,
    conds: &[WeatherConditions],
    direction_bounds: &Option<WindDirectionBounds>,
    speed_bounds: &Option<WindSpeedBounds>,
) -> bool {
    let wind_kts = if weather.wind.2 > 0 {
        weather.wind.2
    } else {
        weather.wind.1
    };
    let is_calm = wind_kts <= 3;

    let within_directional_bounds = direction_bounds
        .as_ref()
        .is_some_and(|r| r.is_within_bounds(weather.wind.0))
        || direction_bounds.is_none();
    let within_speed_bounds = speed_bounds
        .as_ref()
        .is_some_and(|r| r.is_within_bounds(wind_kts))
        || speed_bounds.is_none();
    let matches_conditions = conds.contains(&weather.conditions);

    if calm {
        // Calm rules: wind must be calm, conditions must match, and direction
        // bounds (if specified) must also match
        is_calm && matches_conditions && within_directional_bounds
    } else {
        within_directional_bounds && within_speed_bounds && matches_conditions
    }
}

fn find_matching_rule<'a>(rules: &'a [FlowRule], weather: &AirportWeather) -> Result<&'a FlowRule> {
    rules
        .iter()
        .find(|rule| {
            matches_wind_rule(
                weather,
                rule.calm,
                &rule.conds,
                &rule.direction_bounds,
                &rule.speed_bounds,
            )
        })
        .ok_or_else(|| anyhow::anyhow!("No matching procedure rule found"))
}

fn find_matching_split_rule<'a>(
    rules: &'a [SplitFlowRule],
    weather: &AirportWeather,
) -> Result<&'a SplitFlowRule> {
    rules
        .iter()
        .find(|rule| {
            matches_wind_rule(
                weather,
                rule.calm,
                &rule.conds,
                &rule.direction_bounds,
                &rule.speed_bounds,
            )
        })
        .ok_or_else(|| anyhow::anyhow!("No matching procedure rule found"))
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
            atis_type: "combined".into(),
            facility: "KAPA".into(),
            id: 0,
            notams: "".into(),
            preset: "NORTH VMC".into(),
            timestamp: Utc::now(),
            version: "".into(),
        };

        let flow = procedure.determine_flow(&weather, &[atis]).unwrap();
        assert_eq!(flow.dep_name.as_deref(), Some("NORTH VMC"))
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
        assert_eq!(flow.dep_name.as_deref(), Some("SOUTH VMC"))
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
        assert_eq!(flow.dep_name.as_deref(), Some("NORTH VMC"))
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
        assert_eq!(flow.dep_name.as_deref(), Some("VMC"))
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
        assert_eq!(flow.dep_name.as_deref(), Some("VMC 15 TAILWIND"))
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
        assert_eq!(flow.dep_name.as_deref(), Some("VMC 33 TAILWIND"))
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
        assert_eq!(flow.dep_name.as_deref(), Some("IMC"))
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
        assert_eq!(flow.dep_name.as_deref(), Some("IMC 15 TAILWIND"))
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
        assert_eq!(flow.dep_name.as_deref(), Some("IMC 33 TAILWIND"))
    }

    // KDEN Split ATIS Tests

    #[test]
    fn kden_split_atis_both_present() {
        let config = load_config();
        let procedure = config.0.get("KDEN").unwrap();
        let weather = AirportWeather {
            ceiling: 0,
            conditions: WeatherConditions::VFR,
            name: "DEN".into(),
            raw: "".into(),
            visibility: 10,
            wind: (180, 5, 10),
        };
        let dep_atis = Atis {
            airport_conditions: "".into(),
            atis_letter: "A".into(),
            atis_type: "departure".into(),
            facility: "KDEN".into(),
            id: 0,
            notams: "".into(),
            preset: "SOUTH ALL".into(),
            timestamp: Utc::now(),
            version: "".into(),
        };
        let arr_atis = Atis {
            airport_conditions: "".into(),
            atis_letter: "N".into(),
            atis_type: "arrival".into(),
            facility: "KDEN".into(),
            id: 1,
            notams: "".into(),
            preset: "SOUTH ALL (VMC)".into(),
            timestamp: Utc::now(),
            version: "".into(),
        };

        let flow = procedure
            .determine_flow(&weather, &[dep_atis, arr_atis])
            .unwrap();
        assert_eq!(flow.dep_name.as_deref(), Some("SOUTH ALL"));
        assert_eq!(flow.arr_name.as_deref(), Some("SOUTH ALL (VMC)"));
        assert_eq!(flow.dep_rwys, vec!["16L", "17L"]);
        assert_eq!(flow.arr_rwys, vec!["16L", "16R", "17R"]);
    }

    #[test]
    fn kden_split_atis_only_departure() {
        let config = load_config();
        let procedure = config.0.get("KDEN").unwrap();
        // VMC, south wind 11-25 kts -> weather should pick SOUTH EAST arr
        let weather = AirportWeather {
            ceiling: 0,
            conditions: WeatherConditions::VFR,
            name: "DEN".into(),
            raw: "".into(),
            visibility: 10,
            wind: (130, 5, 15),
        };
        let dep_atis = Atis {
            airport_conditions: "".into(),
            atis_letter: "A".into(),
            atis_type: "departure".into(),
            facility: "KDEN".into(),
            id: 0,
            notams: "".into(),
            preset: "SOUTH EAST".into(),
            timestamp: Utc::now(),
            version: "".into(),
        };

        let flow = procedure.determine_flow(&weather, &[dep_atis]).unwrap();
        assert_eq!(flow.dep_name.as_deref(), Some("SOUTH EAST"));
        assert_eq!(flow.dep_rwys, vec!["8", "17L", "17R"]);
        // Arrival should be weather-determined: SOUTH EAST
        assert_eq!(flow.arr_name.as_deref(), Some("SOUTH EAST"));
        assert_eq!(flow.arr_rwys, vec!["7", "16L", "16R", "17R"]);
    }

    #[test]
    fn kden_weather_fallback_south_calm_vmc() {
        let config = load_config();
        let procedure = config.0.get("KDEN").unwrap();
        let weather = AirportWeather {
            ceiling: 0,
            conditions: WeatherConditions::VFR,
            name: "DEN".into(),
            raw: "".into(),
            visibility: 10,
            wind: (180, 1, 0),
        };

        let flow = procedure.determine_flow(&weather, &[]).unwrap();
        assert_eq!(flow.dep_name.as_deref(), Some("SOUTH CALM"));
        assert_eq!(flow.arr_name.as_deref(), Some("SOUTH CALM"));
        assert_eq!(flow.dep_rwys, vec!["8", "17L", "25"]);
        assert_eq!(flow.arr_rwys, vec!["16L", "16R", "17R"]);
    }

    #[test]
    fn kden_weather_fallback_south_calm_09007_vmc() {
        let config = load_config();
        let procedure = config.0.get("KDEN").unwrap();
        let weather = AirportWeather {
            ceiling: 3456,
            conditions: WeatherConditions::VFR,
            name: "DEN".into(),
            raw: "KDEN 280853Z 09007KT 10SM CLR 15/03 A2977 RMK AO2 SLP987 T01500028 53021".into(),
            visibility: 10,
            wind: (90, 7, 0),
        };

        let flow = procedure.determine_flow(&weather, &[]).unwrap();
        assert_eq!(flow.dep_name.as_deref(), Some("SOUTH CALM"));
        assert_eq!(flow.arr_name.as_deref(), Some("SOUTH CALM"));
        assert_eq!(flow.dep_rwys, vec!["8", "17L", "25"]);
        assert_eq!(flow.arr_rwys, vec!["16L", "16R", "17R"]);
    }

    #[test]
    fn kden_weather_fallback_south_calm_imc() {
        let config = load_config();
        let procedure = config.0.get("KDEN").unwrap();
        let weather = AirportWeather {
            ceiling: 0,
            conditions: WeatherConditions::IFR,
            name: "DEN".into(),
            raw: "".into(),
            visibility: 10,
            wind: (180, 1, 0),
        };

        let flow = procedure.determine_flow(&weather, &[]).unwrap();
        assert_eq!(flow.dep_name.as_deref(), Some("SOUTH CALM"));
        assert_eq!(flow.arr_name.as_deref(), Some("SOUTH IMC"));
        assert_eq!(flow.dep_rwys, vec!["8", "17L", "25"]);
        assert_eq!(flow.arr_rwys, vec!["16R", "17L", "17R"]);
    }

    #[test]
    fn kden_weather_fallback_north_all_vmc() {
        let config = load_config();
        let procedure = config.0.get("KDEN").unwrap();
        let weather = AirportWeather {
            ceiling: 0,
            conditions: WeatherConditions::VFR,
            name: "DEN".into(),
            raw: "".into(),
            visibility: 10,
            wind: (350, 10, 30),
        };

        let flow = procedure.determine_flow(&weather, &[]).unwrap();
        assert_eq!(flow.dep_name.as_deref(), Some("NORTH ALL"));
        assert_eq!(flow.arr_name.as_deref(), Some("NORTH ALL (VMC)"));
        assert_eq!(flow.dep_rwys, vec!["34L", "34R"]);
        assert_eq!(flow.arr_rwys, vec!["34R", "35L", "35R"]);
    }
}
