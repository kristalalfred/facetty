//! Join tokens. The signaling server signs them, the SFU verifies them; both
//! hold the same secret.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use hmac::{Hmac, KeyInit, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;

use crate::ParticipantId;

/// Used when no secret is configured, so a local setup runs without config.
pub const DEV_SECRET: &str = "bits-insecure-dev-secret";

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Claims {
    pub room: String,
    pub participant: ParticipantId,
    pub expires_unix: u64,
}

#[derive(Debug, PartialEq, thiserror::Error)]
pub enum TokenError {
    #[error("malformed token")]
    Malformed,
    #[error("bad signature")]
    BadSignature,
    #[error("token expired")]
    Expired,
}

pub fn sign(secret: &[u8], claims: &Claims) -> String {
    let body = postcard::to_stdvec(claims).expect("postcard serialization into Vec cannot fail");
    let tag = mac(secret, &body).finalize().into_bytes();
    format!(
        "{}.{}",
        URL_SAFE_NO_PAD.encode(&body),
        URL_SAFE_NO_PAD.encode(tag)
    )
}

pub fn verify(secret: &[u8], token: &str, now_unix: u64) -> Result<Claims, TokenError> {
    let (body, tag) = token.split_once('.').ok_or(TokenError::Malformed)?;
    let body = URL_SAFE_NO_PAD
        .decode(body)
        .map_err(|_| TokenError::Malformed)?;
    let tag = URL_SAFE_NO_PAD
        .decode(tag)
        .map_err(|_| TokenError::Malformed)?;
    mac(secret, &body)
        .verify_slice(&tag)
        .map_err(|_| TokenError::BadSignature)?;
    let claims: Claims = postcard::from_bytes(&body).map_err(|_| TokenError::Malformed)?;
    if claims.expires_unix < now_unix {
        return Err(TokenError::Expired);
    }
    Ok(claims)
}

fn mac(secret: &[u8], body: &[u8]) -> Hmac<Sha256> {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret).expect("HMAC accepts any key length");
    mac.update(body);
    mac
}

#[cfg(test)]
mod tests {
    use super::*;

    fn claims() -> Claims {
        Claims {
            room: "standup".into(),
            participant: 3,
            expires_unix: 1_000,
        }
    }

    #[test]
    fn roundtrip() {
        let t = sign(b"s3cret", &claims());
        assert_eq!(verify(b"s3cret", &t, 999), Ok(claims()));
    }

    #[test]
    fn rejects_wrong_secret_and_expiry() {
        let t = sign(b"s3cret", &claims());
        assert_eq!(verify(b"other", &t, 0), Err(TokenError::BadSignature));
        assert_eq!(verify(b"s3cret", &t, 1_001), Err(TokenError::Expired));
        assert_eq!(verify(b"s3cret", "nope", 0), Err(TokenError::Malformed));
    }
}
