use anyhow::{Result, anyhow};
use log::warn;
use serde::{Deserialize, Serialize};

/// Derived weather conditions.
#[allow(clippy::upper_case_acronyms)]
#[derive(Serialize, Deserialize, Debug, PartialEq, Clone)]
pub enum WeatherConditions {
    VFR,
    MVFR,
    IFR,
    LIFR,
}

/// Parsed weather information for an airport.
#[derive(Serialize, Deserialize)]
pub struct AirportWeather {
    pub name: String,
    pub conditions: WeatherConditions,
    pub visibility: u16,
    pub ceiling: u16,
    pub wind: (u16, u8, u8),
    pub raw: String,
}

/// Parse a METAR into a struct of data.
pub fn parse_metar(line: &str) -> Result<AirportWeather> {
    let parts: Vec<_> = line.split(' ').collect();
    let airport = {
        let s = parts.first().ok_or_else(|| anyhow!("Blank metar?"))?;
        if s.len() == 4 { &s[1..] } else { s }
    };
    let mut ceiling = 3_456;
    for part in &parts {
        if part.starts_with("BKN") || part.starts_with("OVC") {
            ceiling = part
                .chars()
                .skip_while(|c| c.is_alphabetic())
                .take_while(|c| c.is_numeric())
                .collect::<String>()
                .parse::<u16>()?
                * 100;
            break;
        }
    }

    let visibility = parts
        .iter()
        .find(|part| part.chars().next().unwrap().is_ascii_digit() && part.ends_with("SM"))
        .map(|part| {
            let vis = part.replace("SM", "");
            if vis.contains('/') {
                Ok(0)
            } else {
                vis.parse()
            }
        })
        .unwrap_or_else(|| {
            warn!("Could not determine visibility for {airport}");
            Ok(0)
        })?;

    let wind_info = parts
        .iter()
        .find(|part| part.ends_with("KT"))
        .unwrap_or_else(|| {
            warn!("Could not determine wind for {airport}");
            &"00000KT"
        });
    let wind = {
        let dir = {
            let s = wind_info.chars().take(3).collect::<String>();
            if s == "VRB" { 0 } else { s.parse()? }
        };
        let mag = wind_info
            .chars()
            .skip(3)
            .take(2)
            .collect::<String>()
            .parse()?;
        let gust = wind_info
            .chars()
            .skip(6)
            .take_while(|c| c.is_numeric())
            .collect::<String>();
        let gust = if !gust.is_empty() { gust.parse()? } else { 0 };
        (dir, mag, gust)
    };

    let conditions = if visibility > 5 && ceiling > 3_000 {
        WeatherConditions::VFR
    } else if visibility >= 3 && ceiling > 1_000 {
        WeatherConditions::MVFR
    } else if visibility >= 1 && ceiling > 500 {
        WeatherConditions::IFR
    } else {
        WeatherConditions::LIFR
    };

    Ok(AirportWeather {
        name: airport.to_string(),
        conditions,
        visibility,
        ceiling,
        wind,
        raw: line.to_string(),
    })
}

/// Check if the value is AOA `min`, clockwise to AOB `max`.
pub fn wind_between(val: u16, min: u16, max: u16) -> bool {
    let val = val % 360;
    let min = min % 360;
    let max = max % 360;

    if min <= max {
        min <= val && val <= max
    } else {
        min <= val || val <= max
    }
}

#[cfg(test)]
pub mod tests {
    use super::{WeatherConditions, parse_metar, wind_between};

    #[test]
    fn test_parse_metar_standard() {
        let ret = parse_metar("KDEN 030253Z 22013KT 10SM SCT100 BKN160 13/M12 A2943 RMK AO2 PK WND 21036/0211 SLP924 T01331117 58005").unwrap();
        assert_eq!(ret.name, "DEN");
        assert_eq!(ret.conditions, WeatherConditions::VFR);

        let ret = parse_metar("KDEN 2SM BNK005").unwrap();
        assert_eq!(ret.conditions, WeatherConditions::IFR);

        let ret = parse_metar("KDEN 4SM OVC020").unwrap();
        assert_eq!(ret.conditions, WeatherConditions::MVFR);

        let ret = parse_metar("KDEN 1/2SM OVC001").unwrap();
        assert_eq!(ret.conditions, WeatherConditions::LIFR);
    }

    #[test]
    fn test_parse_metar_wind() {
        let missing = "K5SM 070435Z AUTO 10SM CLR M12/M13 A3015 RMK AO2";
        let ret = parse_metar(missing).unwrap();
        assert_eq!(ret.wind, (0, 0, 0));

        let standard = "KCPW 070435Z AUTO 03007KT OVC003 M13/M15 A3013 RMK AO2 PWINO";
        let ret = parse_metar(standard).unwrap();
        assert_eq!(ret.wind, (30, 7, 0));

        let gust = "KFLY 070435Z AUTO 36014G21KT OVC036 M05/M07 A3028 RMK AO2 T10531075 PWINO";
        let ret = parse_metar(gust).unwrap();
        assert_eq!(ret.wind, (360, 14, 21));
    }

    #[test]
    fn test_parse_metar_odd_ones() {
        let entries = &[
            "K4BM 070435Z AUTO 36006KT BKN009 OVC014 A3021 RMK AO2 PWINO",
            "K5SM 070435Z AUTO 10SM CLR M12/M13 A3015 RMK AO2",
            "KAEJ 242115Z AUTO 18/M10 A3011 RMK AO2 T01801100 PWINO",
            "KCPW 070435Z AUTO 03007KT OVC003 M13/M15 A3013 RMK AO2 PWINO",
            "KDWX 070435Z AUTO 36009KT CLR M07/M10 A3026 RMK AO2 T10681100 PWINO",
            "KFLY 070435Z AUTO 36014G21KT OVC036 M05/M07 A3028 RMK AO2 T10531075 PWINO",
            "KHEQ 070435Z AUTO 00/00 RMK AO2 TSNO PWINO",
            "KMYP 070435Z AUTO OVC002 M14/M16 A3018 RMK AO2 PWINO",
            "KTBX 070415Z AUTO 15017KT CLR M06/M12 A3027 RMK AO2 PWINO",
            "KAPA 081853Z VRB06KT 10SM SCT055 16/02 A3027 RMK AO2 SLP224 T01610017",
        ];

        for entry in entries {
            _ = parse_metar(entry).unwrap();
        }
    }

    #[test]
    fn test_wind_between() {
        assert!(wind_between(0, 0, 0));
        assert!(wind_between(90, 0, 180));
        assert!(wind_between(5, 350, 10));
        assert!(!wind_between(20, 0, 10));
    }
}
