use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use crate::{
    aviation::{AirportWeather, WeatherConditions},
    sql::Atis,
};

const NO_METAR: &str = "No METAR available";
const NO_RULE: &str = "No rule matches the current weather";

/// The kind of ATIS a vATIS station broadcasts.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Deserialize, Serialize, sqlx::Type,
)]
#[serde(rename_all = "lowercase")]
#[sqlx(rename_all = "lowercase")]
pub enum AtisType {
    Combined,
    Departure,
    Arrival,
}

impl AtisType {
    /// Callsign vATIS connects to VATSIM with for this ATIS.
    pub fn callsign(self, facility: &str) -> String {
        match self {
            Self::Combined => format!("{facility}_ATIS"),
            Self::Departure => format!("{facility}_D_ATIS"),
            Self::Arrival => format!("{facility}_A_ATIS"),
        }
    }
}

/// Where a resolved flow came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum FlowSource {
    /// The preset of an online vATIS.
    Atis,
    /// Matched to another airport's current flow (`tryMatch`).
    Matched,
    /// Estimated from the weather using the configured rules.
    Weather,
}

/// One side (departures or arrivals) of an airport's flow.
#[derive(Debug, Clone, PartialEq)]
pub struct FlowSide {
    pub name: String,
    /// Runway → departure corridors or arrival gates (empty lists for combined airports).
    pub rwys: HashMap<String, Vec<String>>,
    pub source: FlowSource,
}

/// An airport's current flow. Either side may be missing if it couldn't be determined.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ResolvedFlow {
    pub dep: Option<FlowSide>,
    pub arr: Option<FlowSide>,
    /// Problems hit while resolving, e.g. an unknown ATIS preset or missing weather.
    pub issues: Vec<String>,
}

impl ResolvedFlow {
    pub fn dep_name(&self) -> Option<&str> {
        self.dep.as_ref().map(|side| side.name.as_str())
    }

    pub fn arr_name(&self) -> Option<&str> {
        self.arr.as_ref().map(|side| side.name.as_str())
    }

    fn issue(&mut self, issue: impl Into<String>) {
        let issue = issue.into();
        if !self.issues.contains(&issue) {
            self.issues.push(issue);
        }
    }

    fn with_combined(mut self, flow: &Flow, source: FlowSource) -> Self {
        let side = |rwys: &[String]| FlowSide {
            name: flow.name.clone(),
            rwys: rwys_to_map(rwys),
            source,
        };
        self.dep = Some(side(&flow.dep_rwys));
        self.arr = Some(side(&flow.arr_rwys));
        self
    }
}

#[derive(Deserialize, Debug, Clone)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum AirportProcedure {
    Combined(CombinedProcedure),
    Split(SplitProcedure),
}

impl AirportProcedure {
    /// Resolve the airport's current flow.
    ///
    /// An online ATIS's preset wins. Otherwise the flow is matched to the
    /// reference airport's flow in `current` (`tryMatch`), or estimated from
    /// the weather. Passing no ATIS gives the weather-based suggestion.
    pub fn resolve(
        &self,
        icao: &str,
        weather: Option<&AirportWeather>,
        atis: &[Atis],
        current: &HashMap<String, ResolvedFlow>,
    ) -> ResolvedFlow {
        match self {
            Self::Combined(proc) => proc.resolve(icao, weather, atis, current),
            Self::Split(proc) => proc.resolve(icao, weather, atis),
        }
    }

    /// The airport whose flow this one tries to match, if any.
    pub fn try_match_icao(&self) -> Option<&str> {
        match self {
            Self::Combined(proc) => proc.try_match.as_ref().map(|t| t.icao.as_str()),
            Self::Split(_) => None,
        }
    }

    /// Wind bounds of every rule, for config validation.
    pub(crate) fn rule_bounds(
        &self,
    ) -> Vec<(Option<&WindDirectionBounds>, Option<&WindSpeedBounds>)> {
        match self {
            Self::Combined(proc) => proc
                .rules
                .iter()
                .map(|r| (r.direction_bounds.as_ref(), r.speed_bounds.as_ref()))
                .collect(),
            Self::Split(proc) => proc
                .rules
                .iter()
                .map(|r| (r.direction_bounds.as_ref(), r.speed_bounds.as_ref()))
                .collect(),
        }
    }
}

#[derive(Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CombinedProcedure {
    pub flows: HashMap<String, Flow>,
    pub rules: Vec<FlowRule>,
    pub try_match: Option<TryMatchProcedure>,
}

impl CombinedProcedure {
    fn resolve(
        &self,
        icao: &str,
        weather: Option<&AirportWeather>,
        atis: &[Atis],
        current: &HashMap<String, ResolvedFlow>,
    ) -> ResolvedFlow {
        let mut resolved = ResolvedFlow::default();
        if let Some(atis) = find_atis(atis, icao, AtisType::Combined) {
            match self.flows.get(&atis.preset) {
                Some(flow) => return resolved.with_combined(flow, FlowSource::Atis),
                None => resolved.issue(format!("ATIS preset '{}' is not configured", atis.preset)),
            }
        }
        let Some(weather) = weather else {
            resolved.issue(NO_METAR);
            return resolved;
        };
        if let Some(flow) = self.matched_flow(weather, current) {
            return resolved.with_combined(flow, FlowSource::Matched);
        }
        match find_matching_rule(&self.rules, weather)
            .and_then(|rule| self.flows.get(&rule.use_flow))
        {
            Some(flow) => resolved.with_combined(flow, FlowSource::Weather),
            None => {
                resolved.issue(NO_RULE);
                resolved
            }
        }
    }

    /// The local flow mapped to the reference airport's current departure
    /// flow, if the wind is light enough to match.
    fn matched_flow(
        &self,
        weather: &AirportWeather,
        current: &HashMap<String, ResolvedFlow>,
    ) -> Option<&Flow> {
        let try_match = self.try_match.as_ref()?;
        let wind_kts = u32::from(weather.wind.1.max(weather.wind.2));
        if try_match
            .match_when_wind_lt
            .is_some_and(|limit| wind_kts >= limit)
        {
            return None;
        }
        let reference = current.get(&try_match.icao)?.dep_name()?;
        let local = try_match
            .match_flows
            .get(reference)?
            .for_conditions(&weather.conditions);
        self.flows.get(local)
    }
}

#[derive(Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DepCorridor {
    pub direction: String,
    pub gates: Vec<String>,
}

#[derive(Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SplitProcedure {
    pub dep_corridors: HashMap<String, DepCorridor>,
    pub dep_flows: HashMap<String, SplitFlow>,
    pub arr_flows: HashMap<String, SplitFlow>,
    pub rules: Vec<SplitFlowRule>,
}

impl SplitProcedure {
    fn resolve(&self, icao: &str, weather: Option<&AirportWeather>, atis: &[Atis]) -> ResolvedFlow {
        let rule = match weather {
            Some(weather) => find_matching_split_rule(&self.rules, weather).ok_or(NO_RULE),
            None => Err(NO_METAR),
        };
        let mut resolved = ResolvedFlow::default();
        let dep = split_side(
            &mut resolved,
            "Departure",
            &self.dep_flows,
            find_atis(atis, icao, AtisType::Departure),
            rule.map(|r| r.use_dep_flow.as_str()),
        );
        let arr = split_side(
            &mut resolved,
            "Arrival",
            &self.arr_flows,
            find_atis(atis, icao, AtisType::Arrival),
            rule.map(|r| r.use_arr_flow.as_str()),
        );
        resolved.dep = dep;
        resolved.arr = arr;
        resolved
    }
}

/// Resolve one side of a split airport: its ATIS preset if known, else the
/// weather rule's flow.
fn split_side(
    resolved: &mut ResolvedFlow,
    label: &str,
    flows: &HashMap<String, SplitFlow>,
    atis: Option<&Atis>,
    rule_flow: Result<&str, &str>,
) -> Option<FlowSide> {
    if let Some(atis) = atis {
        match flows.get(&atis.preset) {
            Some(flow) => {
                return Some(FlowSide {
                    name: atis.preset.clone(),
                    rwys: flow.rwys.clone(),
                    source: FlowSource::Atis,
                });
            }
            None => resolved.issue(format!(
                "{label} ATIS preset '{}' is not configured",
                atis.preset
            )),
        }
    }
    match rule_flow {
        Ok(name) => flows.get(name).map(|flow| FlowSide {
            name: name.to_string(),
            rwys: flow.rwys.clone(),
            source: FlowSource::Weather,
        }),
        Err(issue) => {
            resolved.issue(issue);
            None
        }
    }
}

fn find_atis<'a>(atis: &'a [Atis], icao: &str, atis_type: AtisType) -> Option<&'a Atis> {
    atis.iter()
        .find(|a| a.facility == icao && a.atis_type == atis_type)
}

#[derive(Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SplitFlow {
    /// Runway → departure corridors (or arrival gates) assigned to it.
    pub rwys: HashMap<String, Vec<String>>,
}

#[derive(Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
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
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TryMatchProcedure {
    pub icao: String,
    pub match_flows: HashMap<String, TryMatchFlowByConditions>,
    pub match_when_wind_lt: Option<u32>,
}

#[derive(Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TryMatchFlowByConditions {
    pub(crate) vmc: String,
    pub(crate) imc: String,
}

impl TryMatchFlowByConditions {
    fn for_conditions(&self, conditions: &WeatherConditions) -> &str {
        match conditions {
            WeatherConditions::VFR | WeatherConditions::MVFR => &self.vmc,
            WeatherConditions::IFR | WeatherConditions::LIFR => &self.imc,
        }
    }
}

#[derive(Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Flow {
    pub name: String,
    pub dep_rwys: Vec<String>,
    pub arr_rwys: Vec<String>,
}

#[derive(Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FlowRule {
    #[serde(default)]
    pub calm: bool,
    pub conds: Vec<WeatherConditions>,
    pub use_flow: String,
    pub direction_bounds: Option<WindDirectionBounds>,
    pub speed_bounds: Option<WindSpeedBounds>,
}

/// Convert a simple runway list (from combined flows) into the standard
/// runway→gate list map (combined airports have no gate data).
fn rwys_to_map(rwys: &[String]) -> HashMap<String, Vec<String>> {
    rwys.iter().map(|r| (r.clone(), Vec::new())).collect()
}

/// Sorted runway names from a runway→gate list map, for display.
pub fn sorted_rwy_names(map: &HashMap<String, Vec<String>>) -> Vec<String> {
    let mut names: Vec<String> = map.keys().cloned().collect();
    names.sort();
    names
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

fn find_matching_rule<'a>(rules: &'a [FlowRule], weather: &AirportWeather) -> Option<&'a FlowRule> {
    rules.iter().find(|rule| {
        matches_wind_rule(
            weather,
            rule.calm,
            &rule.conds,
            &rule.direction_bounds,
            &rule.speed_bounds,
        )
    })
}

fn find_matching_split_rule<'a>(
    rules: &'a [SplitFlowRule],
    weather: &AirportWeather,
) -> Option<&'a SplitFlowRule> {
    rules.iter().find(|rule| {
        matches_wind_rule(
            weather,
            rule.calm,
            &rule.conds,
            &rule.direction_bounds,
            &rule.speed_bounds,
        )
    })
}

#[derive(Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WindSpeedBounds {
    pub(crate) min_kts: u8,
    pub(crate) max_kts: u8,
}

impl WindSpeedBounds {
    #[inline]
    pub fn is_within_bounds(&self, wind_kts: u8) -> bool {
        wind_kts >= self.min_kts && wind_kts <= self.max_kts
    }
}

#[derive(Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
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

    fn weather(name: &str, conditions: WeatherConditions, wind: (u16, u8, u8)) -> AirportWeather {
        AirportWeather {
            ceiling: 0,
            conditions,
            name: name.into(),
            raw: String::new(),
            visibility: 10,
            wind,
            altimeter: None,
        }
    }

    fn atis(facility: &str, atis_type: AtisType, preset: &str) -> Atis {
        Atis {
            airport_conditions: String::new(),
            atis_letter: "A".into(),
            atis_type,
            facility: facility.into(),
            id: 0,
            notams: String::new(),
            preset: preset.into(),
            timestamp: Utc::now(),
            version: String::new(),
            text_atis: String::new(),
        }
    }

    /// Resolve an airport from the real config with no other airports' flows.
    fn resolve(
        config: &ConfigIDS,
        icao: &str,
        weather: Option<&AirportWeather>,
        atis: &[Atis],
    ) -> ResolvedFlow {
        config.0[icao].resolve(icao, weather, atis, &HashMap::new())
    }

    fn try_match_procedure() -> AirportProcedure {
        serde_json::from_value(serde_json::json!({
            "type": "combined",
            "flows": {
                "LOCAL EAST VMC": { "name": "LOCAL EAST VMC", "depRwys": ["17"], "arrRwys": ["17"] },
                "LOCAL EAST IMC": { "name": "LOCAL EAST IMC", "depRwys": ["18"], "arrRwys": ["18"] },
                "LOCAL WEST": { "name": "LOCAL WEST", "depRwys": ["35"], "arrRwys": ["35"] }
            },
            "rules": [{
                "conds": ["VFR"],
                "useFlow": "LOCAL WEST",
                "directionBounds": null,
                "speedBounds": null
            }],
            "tryMatch": {
                "icao": "KDEN",
                "matchFlows": {
                    "DEN EAST": { "vmc": "LOCAL EAST VMC", "imc": "LOCAL EAST IMC" }
                },
                "matchWhenWindLt": 15
            }
        }))
        .unwrap()
    }

    fn den_east() -> HashMap<String, ResolvedFlow> {
        let side = FlowSide {
            name: "DEN EAST".into(),
            rwys: HashMap::new(),
            source: FlowSource::Atis,
        };
        HashMap::from([(
            "KDEN".to_string(),
            ResolvedFlow {
                dep: Some(side.clone()),
                arr: Some(side),
                issues: Vec::new(),
            },
        )])
    }

    #[test]
    fn try_match_selects_mapped_flow_when_wind_allows() {
        let weather = weather("APA", WeatherConditions::VFR, (180, 8, 0));
        let flow = try_match_procedure().resolve("KAPA", Some(&weather), &[], &den_east());
        assert_eq!(flow.dep_name(), Some("LOCAL EAST VMC"));
        assert_eq!(flow.dep.unwrap().source, FlowSource::Matched);
    }

    #[test]
    fn try_match_selects_imc_flow_for_ifr_conditions() {
        let weather = weather("APA", WeatherConditions::IFR, (180, 8, 0));
        let flow = try_match_procedure().resolve("KAPA", Some(&weather), &[], &den_east());
        assert_eq!(flow.dep_name(), Some("LOCAL EAST IMC"));
    }

    #[test]
    fn try_match_falls_back_to_weather_at_wind_limit() {
        let weather = weather("APA", WeatherConditions::VFR, (180, 10, 15));
        let flow = try_match_procedure().resolve("KAPA", Some(&weather), &[], &den_east());
        assert_eq!(flow.dep_name(), Some("LOCAL WEST"));
        assert_eq!(flow.dep.unwrap().source, FlowSource::Weather);
    }

    /// Even if winds favor another flow, choose whichever vATIS has sent
    #[test]
    fn atis_override() {
        let config = load_config();
        let weather = weather("APA", WeatherConditions::VFR, (180, 5, 10));
        let atis = atis("KAPA", AtisType::Combined, "NORTH VMC");
        let flow = resolve(&config, "KAPA", Some(&weather), &[atis]);
        assert_eq!(flow.dep_name(), Some("NORTH VMC"));
        assert_eq!(flow.dep.unwrap().source, FlowSource::Atis);
        assert!(flow.issues.is_empty());
    }

    #[test]
    fn atis_resolves_without_weather() {
        let config = load_config();
        let atis = atis("KAPA", AtisType::Combined, "NORTH VMC");
        let flow = resolve(&config, "KAPA", None, &[atis]);
        assert_eq!(flow.dep_name(), Some("NORTH VMC"));
        assert!(flow.issues.is_empty());
    }

    #[test]
    fn unknown_atis_preset_falls_back_to_weather() {
        let config = load_config();
        let weather = weather("APA", WeatherConditions::VFR, (350, 5, 10));
        let atis = atis("KAPA", AtisType::Combined, "BOGUS");
        let flow = resolve(&config, "KAPA", Some(&weather), &[atis]);
        assert_eq!(flow.dep_name(), Some("NORTH VMC"));
        assert_eq!(flow.dep.unwrap().source, FlowSource::Weather);
        assert_eq!(flow.issues, ["ATIS preset 'BOGUS' is not configured"]);
    }

    #[test]
    fn no_weather_or_atis_reports_why() {
        let config = load_config();
        for icao in ["KAPA", "KDEN"] {
            let flow = resolve(&config, icao, None, &[]);
            assert_eq!(flow.dep, None);
            assert_eq!(flow.arr, None);
            assert_eq!(flow.issues, [NO_METAR]);
        }
    }

    #[test]
    fn flow_from_calm_winds() {
        let config = load_config();
        let weather = weather("APA", WeatherConditions::VFR, (350, 1, 0));
        let flow = resolve(&config, "KAPA", Some(&weather), &[]);
        assert_eq!(flow.dep_name(), Some("SOUTH VMC"))
    }

    #[test]
    fn flow_from_non_calm_winds() {
        let config = load_config();
        let weather = weather("APA", WeatherConditions::VFR, (350, 5, 10));
        let flow = resolve(&config, "KAPA", Some(&weather), &[]);
        assert_eq!(flow.dep_name(), Some("NORTH VMC"))
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
    fn ase_flows() {
        let config = load_config();
        for (conditions, wind, expected) in [
            (WeatherConditions::VFR, (350, 5, 9), "VMC"),
            (WeatherConditions::VFR, (350, 5, 15), "VMC 15 TAILWIND"),
            (WeatherConditions::VFR, (150, 5, 15), "VMC 33 TAILWIND"),
            (WeatherConditions::IFR, (350, 5, 9), "IMC"),
            (WeatherConditions::IFR, (350, 5, 15), "IMC 15 TAILWIND"),
            (WeatherConditions::IFR, (150, 5, 15), "IMC 33 TAILWIND"),
        ] {
            let weather = weather("ASE", conditions.clone(), wind);
            let flow = resolve(&config, "KASE", Some(&weather), &[]);
            assert_eq!(flow.dep_name(), Some(expected), "{conditions:?} {wind:?}");
        }
    }

    // KDEN Split ATIS Tests

    #[test]
    fn kden_split_atis_both_present() {
        let config = load_config();
        let weather = weather("DEN", WeatherConditions::VFR, (180, 5, 10));
        let dep_atis = atis("KDEN", AtisType::Departure, "SOUTH ALL");
        let arr_atis = atis("KDEN", AtisType::Arrival, "SOUTH ALL (VMC)");

        let flow = resolve(&config, "KDEN", Some(&weather), &[dep_atis, arr_atis]);
        assert_eq!(flow.dep_name(), Some("SOUTH ALL"));
        assert_eq!(flow.arr_name(), Some("SOUTH ALL (VMC)"));
        let (dep, arr) = (flow.dep.unwrap(), flow.arr.unwrap());
        assert_eq!(sorted_rwy_names(&dep.rwys), vec!["16L", "17L"]);
        assert_eq!(sorted_rwy_names(&arr.rwys), vec!["16L", "16R", "17R"]);
        assert_eq!(
            (dep.source, arr.source),
            (FlowSource::Atis, FlowSource::Atis)
        );
    }

    #[test]
    fn kden_split_atis_only_departure() {
        let config = load_config();
        // VMC, south wind 11-25 kts -> weather should pick SOUTH EAST arr
        let weather = weather("DEN", WeatherConditions::VFR, (130, 5, 15));
        let dep_atis = atis("KDEN", AtisType::Departure, "SOUTH EAST");

        let flow = resolve(&config, "KDEN", Some(&weather), &[dep_atis]);
        let (dep, arr) = (flow.dep.unwrap(), flow.arr.unwrap());
        assert_eq!(dep.name, "SOUTH EAST");
        assert_eq!(sorted_rwy_names(&dep.rwys), vec!["17L", "8"]);
        // Arrival should be weather-determined: SOUTH EAST
        assert_eq!(arr.name, "SOUTH EAST");
        assert_eq!(arr.source, FlowSource::Weather);
        assert_eq!(sorted_rwy_names(&arr.rwys), vec!["16L", "16R", "17R", "7"]);
    }

    #[test]
    fn kden_split_atis_only_departure_without_weather() {
        let config = load_config();
        let dep_atis = atis("KDEN", AtisType::Departure, "SOUTH EAST");
        let flow = resolve(&config, "KDEN", None, &[dep_atis]);
        assert_eq!(flow.dep_name(), Some("SOUTH EAST"));
        assert_eq!(flow.arr, None);
        assert_eq!(flow.issues, [NO_METAR]);
    }

    #[test]
    fn kden_weather_fallback() {
        let config = load_config();
        for (conditions, wind, dep_name, arr_name, dep_rwys, arr_rwys) in [
            (
                WeatherConditions::VFR,
                (180, 1, 0),
                "SOUTH CALM",
                "SOUTH CALM",
                vec!["17L", "25", "8"],
                vec!["16L", "16R", "17R"],
            ),
            (
                WeatherConditions::VFR,
                (90, 7, 0),
                "SOUTH CALM",
                "SOUTH CALM",
                vec!["17L", "25", "8"],
                vec!["16L", "16R", "17R"],
            ),
            (
                WeatherConditions::IFR,
                (180, 1, 0),
                "SOUTH CALM",
                "SOUTH IMC",
                vec!["17L", "25", "8"],
                vec!["16R", "17L", "17R"],
            ),
            (
                WeatherConditions::VFR,
                (350, 10, 30),
                "NORTH ALL",
                "NORTH ALL (VMC)",
                vec!["34L", "34R"],
                vec!["34R", "35L", "35R"],
            ),
        ] {
            let weather = weather("DEN", conditions, wind);
            let flow = resolve(&config, "KDEN", Some(&weather), &[]);
            let (dep, arr) = (flow.dep.unwrap(), flow.arr.unwrap());
            assert_eq!((dep.name.as_str(), arr.name.as_str()), (dep_name, arr_name));
            assert_eq!(sorted_rwy_names(&dep.rwys), dep_rwys);
            assert_eq!(sorted_rwy_names(&arr.rwys), arr_rwys);
            assert_eq!(dep.source, FlowSource::Weather);
        }
    }

    /// Every airport has a rule for any wind and flight category, so the IDS
    /// can always estimate a flow from the weather.
    #[test]
    fn rules_cover_all_weather() {
        let config = load_config();
        for (icao, procedure) in &config.0 {
            let mut weather = weather(icao, WeatherConditions::VFR, (0, 0, 0));
            for conditions in [
                WeatherConditions::VFR,
                WeatherConditions::MVFR,
                WeatherConditions::IFR,
                WeatherConditions::LIFR,
            ] {
                weather.conditions = conditions;
                for kts in 0..100 {
                    for dir in 0..360 {
                        weather.wind = (dir, kts, 0);
                        let matched = match procedure {
                            AirportProcedure::Combined(proc) => {
                                find_matching_rule(&proc.rules, &weather).is_some()
                            }
                            AirportProcedure::Split(proc) => {
                                find_matching_split_rule(&proc.rules, &weather).is_some()
                            }
                        };
                        assert!(matched, "{icao} has no rule for {weather:?}");
                    }
                }
            }
        }
    }

    #[test]
    fn unknown_config_fields_are_rejected() {
        let result = serde_json::from_value::<AirportProcedure>(serde_json::json!({
            "type": "combined",
            "flows": {},
            "rules": [{ "conds": ["VFR"], "useFlow": "X", "directionBound": null }]
        }));
        assert!(result.unwrap_err().to_string().contains("directionBound"));
    }

    #[test]
    fn atis_callsigns_match_vatis() {
        assert_eq!(AtisType::Combined.callsign("KAPA"), "KAPA_ATIS");
        assert_eq!(AtisType::Departure.callsign("KDEN"), "KDEN_D_ATIS");
        assert_eq!(AtisType::Arrival.callsign("KDEN"), "KDEN_A_ATIS");
    }
}
