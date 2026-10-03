//! Verification of the JWT vATIS attaches to IDS updates.
//!
//! vATIS signs each IDS POST with a short-lived RS256 token (key id
//! `ids-validation`, issuer and audience `vatis.app`) and publishes the public
//! key as a JWKS. A valid token only proves the update came from an official
//! vATIS build: it carries no CID or facility and isn't tied to the body.

use crate::shared::AppState;
use anyhow::{Result, bail};
use axum::http::{HeaderMap, header::AUTHORIZATION};
use base64::{Engine as _, engine::general_purpose::STANDARD_NO_PAD};
use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode, decode_header};
use log::{error, info, warn};
use serde::Deserialize;
use std::{
    collections::HashMap,
    sync::{Arc, Mutex, RwLock},
    time::{Duration, Instant},
};
use vzdv::{GENERAL_HTTP_CLIENT, config::VatisJwtMode};

const JWKS_URL: &str = "https://hub.vatis.app/.well-known/jwks.json";
const ISSUER: &str = "vatis.app";
/// Allowed clock skew, since `nbf` and `exp` come from the controller's PC clock.
const LEEWAY_SECONDS: u64 = 120;
const REFRESH_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);
/// Minimum time between JWKS fetches, both for retries and unknown key ids.
const MIN_FETCH_INTERVAL: Duration = Duration::from_secs(5 * 60);

/// Result of checking a request's token.
#[derive(Debug, PartialEq, Eq)]
pub enum Verdict {
    Valid,
    Missing,
    Invalid(String),
    /// The JWKS hasn't been fetched, so nothing can be verified.
    NoKeys,
}

/// vATIS's public signing keys, by key id.
#[derive(Default)]
pub struct VatisKeys {
    keys: RwLock<HashMap<String, DecodingKey>>,
    last_fetch: Mutex<Option<Instant>>,
}

#[derive(Deserialize)]
struct Jwks {
    keys: Vec<Jwk>,
}

#[derive(Deserialize)]
struct Jwk {
    kid: String,
    kty: String,
    n: Option<String>,
    e: Option<String>,
}

/// Decode base64 whether it's base64url or standard, padded or not.
///
/// vATIS's JWKS uses standard base64 even though JWK requires base64url,
/// which `DecodingKey::from_jwk` rejects.
fn decode_b64(value: &str) -> Result<Vec<u8>> {
    let normalized: String = value
        .trim_end_matches('=')
        .chars()
        .map(|c| match c {
            '-' => '+',
            '_' => '/',
            c => c,
        })
        .collect();
    Ok(STANDARD_NO_PAD.decode(normalized)?)
}

fn parse_jwks(json: &str) -> Result<HashMap<String, DecodingKey>> {
    let jwks: Jwks = serde_json::from_str(json)?;
    let mut keys = HashMap::new();
    for key in jwks.keys {
        let (Some(n), Some(e)) = (&key.n, &key.e) else {
            continue;
        };
        if key.kty == "RSA" {
            let key_value = DecodingKey::from_rsa_raw_components(&decode_b64(n)?, &decode_b64(e)?);
            keys.insert(key.kid, key_value);
        }
    }
    if keys.is_empty() {
        bail!("JWKS has no RSA keys");
    }
    Ok(keys)
}

fn validation() -> Validation {
    let mut validation = Validation::new(Algorithm::RS256);
    validation.set_issuer(&[ISSUER]);
    validation.set_audience(&[ISSUER]);
    validation.set_required_spec_claims(&["exp", "nbf", "iss", "aud"]);
    validation.validate_nbf = true;
    validation.leeway = LEEWAY_SECONDS;
    validation
}

fn bearer_token(headers: &HeaderMap) -> Option<&str> {
    let token = headers
        .get(AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")?
        .trim();
    (!token.is_empty()).then_some(token)
}

impl VatisKeys {
    #[cfg(test)]
    pub fn from_jwks(json: &str) -> Self {
        let keys = Self::default();
        *keys.keys.write().unwrap() = parse_jwks(json).unwrap();
        *keys.last_fetch.lock().unwrap() = Some(Instant::now());
        keys
    }

    /// Fetch the JWKS and replace the stored keys.
    async fn fetch(&self) -> Result<()> {
        *self.last_fetch.lock().unwrap() = Some(Instant::now());
        let resp = GENERAL_HTTP_CLIENT.get(JWKS_URL).send().await?;
        if !resp.status().is_success() {
            bail!("vATIS JWKS returned status {}", resp.status());
        }
        let keys = parse_jwks(&resp.text().await?)?;
        *self.keys.write().unwrap() = keys;
        Ok(())
    }

    /// Whether enough time has passed to fetch again; claims the slot if so.
    fn claim_fetch(&self) -> bool {
        let mut last = self.last_fetch.lock().unwrap();
        if last.is_some_and(|at| at.elapsed() < MIN_FETCH_INTERVAL) {
            return false;
        }
        *last = Some(Instant::now());
        true
    }

    fn key(&self, kid: &str) -> Option<DecodingKey> {
        self.keys.read().unwrap().get(kid).cloned()
    }

    /// Check the request's bearer token.
    pub async fn verify(&self, headers: &HeaderMap) -> Verdict {
        let Some(token) = bearer_token(headers) else {
            return Verdict::Missing;
        };
        let kid = match decode_header(token) {
            Ok(header) => header.kid.unwrap_or_default(),
            Err(e) => return Verdict::Invalid(format!("unreadable header: {e}")),
        };
        let mut key = self.key(&kid);
        // vATIS may have rotated keys
        if key.is_none() && self.claim_fetch() {
            if let Err(e) = self.fetch().await {
                warn!("Could not refetch vATIS JWKS: {e}");
            }
            key = self.key(&kid);
        }
        let Some(key) = key else {
            if self.keys.read().unwrap().is_empty() {
                return Verdict::NoKeys;
            }
            return Verdict::Invalid(format!("unknown key id '{kid}'"));
        };
        match decode::<serde_json::Value>(token, &key, &validation()) {
            Ok(_) => Verdict::Valid,
            Err(e) => Verdict::Invalid(e.to_string()),
        }
    }
}

/// Keep vATIS's signing keys loaded, retrying sooner after failures.
pub async fn refresh_keys(state: Arc<AppState>) {
    if state.config.ids.vatis_jwt == VatisJwtMode::Off {
        return;
    }
    loop {
        let wait = match state.vatis_keys.fetch().await {
            Ok(()) => {
                info!("Loaded vATIS JWKS");
                REFRESH_INTERVAL
            }
            Err(e) => {
                error!("Could not fetch vATIS JWKS: {e}");
                MIN_FETCH_INTERVAL
            }
        };
        tokio::time::sleep(wait).await;
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use base64::engine::general_purpose::STANDARD;
    use jsonwebtoken::{EncodingKey, Header, encode};
    use serde_json::json;

    /// Throwaway RSA key (PKCS#1 DER) for signing test tokens.
    const TEST_KEY: &str = include_str!("../testdata/vatis_test_key.b64");
    /// Copy of the live vATIS JWKS, which uses standard base64.
    const LIVE_JWKS: &str = include_str!("../testdata/vatis_jwks.json");

    fn test_key_der() -> Vec<u8> {
        STANDARD.decode(TEST_KEY.trim()).unwrap()
    }

    /// JWKS for the test key, encoded the way vATIS does (standard base64).
    pub const TEST_JWKS: &str = include_str!("../testdata/vatis_test_jwks.json");

    /// Sign claims the way vATIS does, optionally overriding fields.
    pub fn token(changes: serde_json::Value) -> String {
        let now = chrono::Utc::now().timestamp();
        let mut claims = json!({
            "iss": "vatis.app", "aud": "vatis.app", "nbf": now, "iat": now, "exp": now + 300
        });
        for (key, value) in changes.as_object().unwrap() {
            claims[key] = value.clone();
        }
        let mut header = Header::new(Algorithm::RS256);
        header.kid = Some("ids-validation".into());
        encode(
            &header,
            &claims,
            &EncodingKey::from_rsa_der(&test_key_der()),
        )
        .unwrap()
    }

    fn headers(token: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(AUTHORIZATION, format!("Bearer {token}").parse().unwrap());
        headers
    }

    #[test]
    fn live_jwks_parses_despite_standard_base64() {
        let keys = parse_jwks(LIVE_JWKS).unwrap();
        assert!(keys.contains_key("ids-validation"));
        assert_eq!(decode_b64("AQAB").unwrap(), [1, 0, 1]);
        assert_eq!(decode_b64("-_8").unwrap(), decode_b64("+/8=").unwrap());
    }

    #[tokio::test]
    async fn accepts_valid_tokens_within_clock_skew() {
        let keys = VatisKeys::from_jwks(TEST_JWKS);
        let now = chrono::Utc::now().timestamp();
        assert_eq!(
            keys.verify(&headers(&token(json!({})))).await,
            Verdict::Valid
        );
        let skewed = token(json!({ "nbf": now + 60, "exp": now + 360 }));
        assert_eq!(keys.verify(&headers(&skewed)).await, Verdict::Valid);
    }

    #[tokio::test]
    async fn rejects_bad_tokens() {
        let keys = VatisKeys::from_jwks(TEST_JWKS);
        let now = chrono::Utc::now().timestamp();
        assert_eq!(keys.verify(&HeaderMap::new()).await, Verdict::Missing);
        for bad in [
            token(json!({ "exp": now - 600 })),
            token(json!({ "iss": "evil.example" })),
            token(json!({ "aud": "someone-else" })),
            "not.a.token".to_string(),
        ] {
            assert!(
                matches!(keys.verify(&headers(&bad)).await, Verdict::Invalid(_)),
                "{bad}"
            );
        }
        // Signed by someone else's key
        let other = VatisKeys::from_jwks(LIVE_JWKS);
        assert!(matches!(
            other.verify(&headers(&token(json!({})))).await,
            Verdict::Invalid(_)
        ));
        // Can't pick a symmetric algorithm to forge with the public key
        let mut header = Header::new(Algorithm::HS256);
        header.kid = Some("ids-validation".into());
        let forged = encode(
            &header,
            &json!({ "iss": "vatis.app", "aud": "vatis.app", "nbf": now, "exp": now + 300 }),
            &EncodingKey::from_secret(TEST_JWKS.as_bytes()),
        )
        .unwrap();
        assert!(matches!(
            keys.verify(&headers(&forged)).await,
            Verdict::Invalid(_)
        ));
    }

    #[tokio::test]
    async fn reports_missing_keys() {
        let keys = VatisKeys::default();
        *keys.last_fetch.lock().unwrap() = Some(Instant::now());
        assert_eq!(
            keys.verify(&headers(&token(json!({})))).await,
            Verdict::NoKeys
        );
    }
}
