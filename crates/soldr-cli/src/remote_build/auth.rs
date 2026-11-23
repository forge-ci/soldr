use std::io;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::http::HeaderMap;
use ring::hmac;

use super::snapshot::digest;

const LAUNCH_WINDOW_SECONDS: u64 = 900;
const SESSION_WINDOW_SECONDS: u64 = 4 * 60 * 60;

#[derive(Clone)]
pub(super) enum RunnerAuth {
    Tunnel(String),
    Forge(hmac::Key),
}

impl RunnerAuth {
    pub(super) fn tunnel(token: &str) -> Self {
        Self::Tunnel(digest(format!("Bearer {token}").as_bytes()))
    }

    pub(super) fn forge(path: &Path) -> io::Result<Self> {
        let contents = std::fs::read_to_string(path)?;
        let key = contents.trim_end_matches(['\r', '\n']).as_bytes();
        if key.len() < 32 {
            return Err(io::Error::other(
                "Forge CI worker key must be at least 32 bytes.",
            ));
        }
        Ok(Self::Forge(hmac::Key::new(hmac::HMAC_SHA256, key)))
    }

    pub(super) fn authorized(&self, headers: &HeaderMap, starts_session: bool) -> bool {
        let Some(header) = headers.get("authorization") else {
            return false;
        };
        match self {
            Self::Tunnel(expected) => digest(header.as_bytes()) == *expected,
            Self::Forge(key) => {
                let Ok(now) = SystemTime::now().duration_since(UNIX_EPOCH) else {
                    return false;
                };
                header
                    .to_str()
                    .ok()
                    .and_then(|value| value.strip_prefix("Bearer "))
                    .is_some_and(|token| valid_grant(key, token, now.as_secs(), starts_session))
            }
        }
    }
}

fn valid_grant(key: &hmac::Key, token: &str, now: u64, starts_session: bool) -> bool {
    let Some((claims, signature)) = token.rsplit_once('.') else {
        return false;
    };
    let mut parts = claims.split('.');
    let (Some("w1"), Some(account), Some(expiry), Some(nonce), None) = (
        parts.next(),
        parts.next(),
        parts.next(),
        parts.next(),
        parts.next(),
    ) else {
        return false;
    };
    let Ok(expiry) = expiry.parse::<u64>() else {
        return false;
    };
    if !account.parse::<u64>().is_ok_and(|id| id > 0)
        || nonce.len() != 43
        || expiry > now.saturating_add(LAUNCH_WINDOW_SECONDS)
        || now
            >= expiry.saturating_add(if starts_session {
                0
            } else {
                SESSION_WINDOW_SECONDS
            })
    {
        return false;
    }
    let mut tag = [0; 32];
    hex::decode_to_slice(signature, &mut tag).is_ok()
        && hmac::verify(key, claims.as_bytes(), &tag).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_worker_grant_has_a_short_launch_window_and_bounded_download_window() {
        let key = hmac::Key::new(hmac::HMAC_SHA256, &[7; 32]);
        let expiry = 1_000;
        let nonce = "a".repeat(43);
        let claims = format!("w1.42.{expiry}.{nonce}");
        let tag = hex::encode(hmac::sign(&key, claims.as_bytes()));
        let grant = format!("{claims}.{tag}");
        assert!(valid_grant(&key, &grant, expiry - 1, true));
        assert!(!valid_grant(&key, &grant, expiry, true));
        assert!(valid_grant(&key, &grant, expiry, false));
        assert!(!valid_grant(
            &key,
            &grant,
            expiry + SESSION_WINDOW_SECONDS,
            false
        ));
        assert!(!valid_grant(
            &key,
            &grant.replace(".42.", ".43."),
            expiry - 1,
            true
        ));
        assert!(!valid_grant(
            &key,
            &grant.replace("w1.", "v1."),
            expiry - 1,
            true
        ));
    }
}
