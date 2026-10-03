use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

pub type SectorId = u8;
pub type SectorConsolidation = HashMap<u8, Vec<SectorOrArea>>;

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(untagged)]
pub enum SectorOrArea {
    Sector(SectorId),
    Area(String),
}

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct SectorSplit {
    pub high: SectorConsolidation,
    pub low: SectorConsolidation,
    pub color_code_by_area: Option<bool>,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct FrequencyConfig {
    pub name: String,
    pub freq: f32,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct SectorsConfig {
    pub frequencies: HashMap<String, FrequencyConfig>,
    pub areas: HashMap<String, Vec<SectorId>>,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct ConfigSplits {
    pub frequencies: HashMap<String, FrequencyConfig>,
    pub areas: HashMap<String, Vec<SectorId>>,
    pub splits: IndexMap<String, SectorSplit>,
}
