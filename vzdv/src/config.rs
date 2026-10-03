use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, fs, path::Path};

use crate::ids::AirportProcedure;

/// Default place to look for the config file.
pub const DEFAULT_CONFIG_FILE_NAME: &str = "vzdv.toml";
pub const DEFAULT_IDS_CONFIG_FILE_NAME: &str = "ids.json";

/// App configuration.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct Config {
    pub hosted_domain: String,
    pub database: ConfigDatabase,
    pub staff: ConfigStaff,
    pub vatsim: ConfigVatsim,
    pub training: ConfigTraining,
    pub airports: ConfigAirports,
    pub weather: ConfigWeather,
    pub stats: ConfigStats,
    pub discord: ConfigDiscord,
    pub email: ConfigEmail,
    pub airspace_maps: ConfigAirspaceMaps,
    #[serde(default)]
    pub ids: ConfigIdsOptions,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct ConfigDatabase {
    pub file: String,
    pub resource_category_ordering: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct ConfigStaff {
    pub email_domain: String,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct ConfigVatsim {
    pub oauth_url_base: String,
    pub oauth_client_id: String,
    pub oauth_client_secret: String,
    pub oauth_client_callback_url: String,
    pub vatusa_api_key: String,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct ConfigTraining {
    pub certifications: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct ConfigAirports {
    pub all: Vec<Airport>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct ConfigWeather {
    pub overview: Vec<String>,
    pub all: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct Airport {
    pub code: String,
    pub name: String,
    pub location: String,
    pub towered: bool,
    pub class: String,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct ConfigStats {
    pub position_prefixes: Vec<String>,
    pub position_suffixes: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct ConfigDiscord {
    pub join_link: String,
    pub bot_token: String,
    pub auth: ConfigDiscordAuth,
    pub guild_id: u64,
    pub online_channel: u64,
    pub online_message: Option<u64>,
    pub off_roster_channel: u64,
    pub webhooks: ConfigDiscordWebhooks,
    pub roles: ConfigDiscordRoles,
    pub owner_id: u64,
    pub solo_cert_expiration_channel: u64,
    pub streamers: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct ConfigDiscordAuth {
    pub client_id: String,
    pub client_secret: String,
    pub redirect_uri: String,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct ConfigDiscordWebhooks {
    pub staffing_request: String,
    pub new_feedback: String,
    pub feedback: String,
    pub new_visitor_app: String,
    pub errors: String,
    pub audit: String,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct ConfigDiscordRoles {
    // status
    pub guest: u64,
    pub controller_otm: u64,
    pub home_controller: u64,
    pub visiting_controller: u64,
    pub event_controller: u64,

    // special
    pub vatusa_vatgov: u64,

    // staff
    pub sr_staff: u64,
    pub jr_staff: u64,

    // staff teams
    pub training_staff: u64,
    pub event_team: u64,
    pub fe_team: u64,
    pub web_team: u64,

    // ratings
    pub administrator: u64,
    pub supervisor: u64,
    pub instructor_3: u64,
    pub instructor_1: u64,
    pub controller_3: u64,
    pub controller_1: u64,
    pub student_3: u64,
    pub student_2: u64,
    pub student_1: u64,
    pub observer: u64,

    // certs
    pub t2_ctr: u64,
    pub t1_app: u64,
    pub t1_twr: u64,
    pub t1_gnd: u64,

    // misc
    pub ignore: u64,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct ConfigEmail {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub password: String,
    pub from: String,
    pub reply_to: String,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct ConfigAirspaceMaps {
    pub carto_key: String,
}

/// Site-side IDS options from the main config file. The airport procedures
/// themselves live in the IDS config file; see [`ConfigIDS`].
#[derive(Debug, Clone, Deserialize, Default)]
pub struct ConfigIdsOptions {
    #[serde(default)]
    pub vatis_jwt: VatisJwtMode,
}

/// How strictly to check the JWT that vATIS attaches to IDS updates.
#[derive(Debug, Clone, Copy, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum VatisJwtMode {
    /// Don't check tokens.
    Off,
    /// Check tokens and log failures, but still accept the update.
    #[default]
    Log,
    /// Reject updates without a valid token.
    Enforce,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct ConfigIDS(pub HashMap<String, AirportProcedure>);

impl ConfigIDS {
    /// Read the JSON file at the given path and load into the app's configuration file.
    pub fn load_from_disk(path: &Path) -> Result<Self> {
        if !Path::new(path).exists() {
            bail!("Config file \"{}\" not found", path.display());
        }
        let text = fs::read_to_string(path)?;
        let config: ConfigIDS = serde_json::from_str(&text)?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<()> {
        for (icao, entry) in self.0.iter() {
            match entry {
                AirportProcedure::Combined(proc) => {
                    // Check that all flow keys match their name field
                    if let Some((bad_flow_name, bad_flow)) =
                        proc.flows.iter().find(|f| *f.0 != f.1.name)
                    {
                        bail!(
                            "{bad_flow_name} flow name ({}) does not match for {icao}",
                            bad_flow.name
                        )
                    }
                    // Check that every rule mentions a flow present in `flows`
                    if let Some(bad_rule) = proc
                        .rules
                        .iter()
                        .find(|r| !proc.flows.contains_key(&r.use_flow))
                    {
                        bail!(
                            "Rule in {icao} references a flow that is not present in flows field! Rule: {bad_rule:?}"
                        )
                    }
                    if let Some(try_match) = &proc.try_match {
                        let Some(reference) = self.0.get(&try_match.icao) else {
                            bail!(
                                "tryMatch in {icao} references airport {} that is not configured",
                                try_match.icao
                            )
                        };
                        if matches!(
                            reference,
                            AirportProcedure::Combined(reference) if reference.try_match.is_some()
                        ) {
                            bail!(
                                "tryMatch in {icao} references {}, which also has tryMatch configured",
                                try_match.icao
                            )
                        }
                        for (reference_flow, flows) in &try_match.match_flows {
                            let reference_has_flow = match reference {
                                AirportProcedure::Combined(reference) => {
                                    reference.flows.contains_key(reference_flow)
                                }
                                AirportProcedure::Split(reference) => {
                                    reference.dep_flows.contains_key(reference_flow)
                                }
                            };
                            if !reference_has_flow {
                                bail!(
                                    "tryMatch in {icao} maps {}'s flow '{reference_flow}', which {} does not have",
                                    try_match.icao,
                                    try_match.icao
                                )
                            }
                            for flow in [&flows.vmc, &flows.imc] {
                                if !proc.flows.contains_key(flow) {
                                    bail!(
                                        "tryMatch in {icao} maps {reference_flow} to local flow '{flow}' that is not configured"
                                    )
                                }
                            }
                        }
                    }
                }
                AirportProcedure::Split(proc) => {
                    // Check that every departure flow only uses defined corridors
                    for (flow_name, flow) in &proc.dep_flows {
                        if let Some(corridor) = flow
                            .rwys
                            .values()
                            .flatten()
                            .find(|corridor| !proc.dep_corridors.contains_key(*corridor))
                        {
                            bail!(
                                "Dep flow '{flow_name}' in {icao} uses corridor '{corridor}' not present in depCorridors"
                            )
                        }
                    }
                    // Check that every rule references valid dep and arr flows
                    for rule in &proc.rules {
                        if !proc.dep_flows.contains_key(&rule.use_dep_flow) {
                            bail!(
                                "Rule in {icao} references dep flow '{}' not present in depFlows! Rule: {rule:?}",
                                rule.use_dep_flow
                            )
                        }
                        if !proc.arr_flows.contains_key(&rule.use_arr_flow) {
                            bail!(
                                "Rule in {icao} references arr flow '{}' not present in arrFlows! Rule: {rule:?}",
                                rule.use_arr_flow
                            )
                        }
                    }
                }
            }
            for (direction_bounds, speed_bounds) in entry.rule_bounds() {
                if let Some(bounds) = direction_bounds
                    && (bounds.wind_from > 360 || bounds.wind_to > 360)
                {
                    bail!("Rule in {icao} has a wind direction over 360: {bounds:?}")
                }
                if let Some(bounds) = speed_bounds
                    && bounds.min_kts > bounds.max_kts
                {
                    bail!("Rule in {icao} has minKts above maxKts: {bounds:?}")
                }
            }
        }

        Ok(())
    }
}

impl Config {
    /// Read the TOML file at the given path and load into the app's configuration file.
    pub fn load_from_disk(path: &Path) -> Result<Self> {
        if !Path::new(path).exists() {
            bail!("Config file \"{}\" not found", path.display());
        }
        let text = fs::read_to_string(path)?;
        let config: Config = toml::from_str(&text)?;
        Ok(config)
    }
}

#[cfg(test)]
mod tests {
    use super::ConfigIDS;

    fn config(value: serde_json::Value) -> ConfigIDS {
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn shipped_ids_config_is_valid() {
        let text = std::fs::read_to_string("../ids.json").unwrap();
        let config: ConfigIDS = serde_json::from_str(&text).unwrap();
        config.validate().unwrap();
    }

    #[test]
    fn rejects_unknown_departure_corridor() {
        let config = config(serde_json::json!({
            "KDEN": {
                "type": "split",
                "depCorridors": { "N": { "direction": "NORTH", "gates": [] } },
                "depFlows": { "NORTH": { "rwys": { "34L": ["N", "E"] } } },
                "arrFlows": {},
                "rules": []
            }
        }));
        let err = config.validate().unwrap_err().to_string();
        assert!(err.contains("corridor 'E'"), "{err}");
    }

    #[test]
    fn rejects_try_match_of_missing_reference_flow() {
        let config = config(serde_json::json!({
            "KDEN": {
                "type": "split",
                "depCorridors": {},
                "depFlows": { "NORTH": { "rwys": {} } },
                "arrFlows": {},
                "rules": []
            },
            "KAPA": {
                "type": "combined",
                "flows": { "N": { "name": "N", "depRwys": [], "arrRwys": [] } },
                "rules": [],
                "tryMatch": {
                    "icao": "KDEN",
                    "matchFlows": { "SOUTH": { "vmc": "N", "imc": "N" } }
                }
            }
        }));
        let err = config.validate().unwrap_err().to_string();
        assert!(err.contains("'SOUTH'"), "{err}");
    }

    #[test]
    fn rejects_inverted_speed_bounds() {
        let config = config(serde_json::json!({
            "KAPA": {
                "type": "combined",
                "flows": { "N": { "name": "N", "depRwys": [], "arrRwys": [] } },
                "rules": [{ "conds": ["VFR"], "useFlow": "N", "speedBounds": { "minKts": 10, "maxKts": 5 } }]
            }
        }));
        assert!(config.validate().is_err());
    }
}
