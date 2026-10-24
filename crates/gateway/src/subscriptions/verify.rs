//! Authenticate a request to a subscription endpoint.
//!
//! Two ways in, checked in this order:
//!
//! 1. **The provider's own signature**, when its header is present. It is
//!    verified with the signing secret the provider issued (pasted into Borg),
//!    or, for GitHub, which signs with whatever secret it is given, with
//!    Borg's key.
//! 2. **Borg's key** as `Authorization: Bearer <key>`, or as the password of
//!    HTTP Basic for services that can only put credentials in the URL.
//!
//! A request with neither is refused. A signature header that is present but
//! wrong is refused even if a bearer key is also sent: a forged signature is
//! never rescued by a second credential.

use axum::http::HeaderMap;
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use cortex::subscriptions::presets::AuthScheme;

use crate::crypto::constant_time_eq;
use crate::verify_common::hmac_sha256_verify;

/// Seconds a timestamped signature may be old (Stripe, Svix).
pub const SIGNATURE_TOLERANCE_SECS: i64 = 300;
/// Linear asks receivers to reject deliveries older than a minute.
const LINEAR_TOLERANCE_MS: i64 = 60_000;

/// Why a request was refused. The messages are written for the person
/// reading the provider's delivery log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthError {
    /// No signature header and no key.
    Missing,
    /// The key did not match.
    BadKey,
    /// The signature did not match.
    BadSignature,
    /// The signature is older than the tolerance.
    Stale,
    /// The provider signed the request, but no signing secret was pasted.
    NeedsSigningSecret,
    /// A signing secret is pasted, so the provider's signature is required,
    /// and this request does not carry the named header.
    SignatureRequired(&'static str),
    /// A header could not be read.
    Malformed(&'static str),
}

impl std::fmt::Display for AuthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AuthError::Missing => f.write_str(
                "No credentials. Send Authorization: Bearer <key>, or the provider's signature header.",
            ),
            AuthError::BadKey => f.write_str("The key does not match this subscription."),
            AuthError::BadSignature => f.write_str(
                "The signature does not match. Check that the signing secret in Borg is the one this provider shows.",
            ),
            AuthError::Stale => f.write_str("The signature is too old. Check the sender's clock."),
            AuthError::NeedsSigningSecret => f.write_str(
                "This provider signs its requests. Paste its signing secret into the subscription in Borg.",
            ),
            AuthError::SignatureRequired(h) => write!(
                f,
                "This subscription checks the provider's signature, and the request has no {h} header. Send events from the provider itself."
            ),
            AuthError::Malformed(h) => write!(f, "The {h} header could not be read."),
        }
    }
}

impl std::error::Error for AuthError {}

/// The secrets one subscription verifies against.
#[derive(Debug, Clone, Copy)]
pub struct Secrets<'a> {
    /// Borg's endpoint key.
    pub key: &'a str,
    /// The signing secret the provider issued, if pasted.
    pub signing_secret: Option<&'a str>,
}

fn header<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|v| v.to_str().ok())
}

/// Authenticate `body` under `scheme`.
pub fn verify(
    scheme: AuthScheme,
    headers: &HeaderMap,
    body: &[u8],
    secrets: Secrets<'_>,
    now_unix: i64,
) -> Result<(), AuthError> {
    if let Some(result) = verify_native(scheme, headers, body, secrets, now_unix) {
        return result;
    }
    // Once the user pasted the provider's signing secret, its signature is
    // required: accepting the bare key instead would skip the timestamp
    // check and let a replayed request through.
    if scheme.provider_issues_secret() && secrets.signing_secret.is_some() {
        return Err(AuthError::SignatureRequired(signature_header(scheme)));
    }
    verify_key(headers, secrets.key)
}

/// The header a provider that issues its own secret signs with.
fn signature_header(scheme: AuthScheme) -> &'static str {
    match scheme {
        AuthScheme::Sentry => "Sentry-Hook-Signature",
        AuthScheme::Linear => "Linear-Signature",
        AuthScheme::Pagerduty => "X-PagerDuty-Signature",
        AuthScheme::Stripe => "Stripe-Signature",
        AuthScheme::Svix => "svix-signature",
        AuthScheme::Github => "X-Hub-Signature-256",
        AuthScheme::Bearer | AuthScheme::None => "signature",
    }
}

/// Whether `ts` is outside the tolerance around `now`. `abs_diff` because
/// `ts` comes from the sender before the signature is checked: a
/// subtraction could overflow on a crafted value.
fn stale(now: i64, ts: i64, tolerance: i64) -> bool {
    now.abs_diff(ts) > tolerance.unsigned_abs()
}

/// `None` when the scheme's signature header is absent (fall back to the key).
fn verify_native(
    scheme: AuthScheme,
    headers: &HeaderMap,
    body: &[u8],
    secrets: Secrets<'_>,
    now: i64,
) -> Option<Result<(), AuthError>> {
    let provider_secret = || secrets.signing_secret.ok_or(AuthError::NeedsSigningSecret);
    Some(match scheme {
        AuthScheme::Bearer | AuthScheme::None => return None,
        AuthScheme::Github => {
            let sig = header(headers, "x-hub-signature-256")?;
            let secret = secrets.signing_secret.unwrap_or(secrets.key);
            sig.strip_prefix("sha256=")
                .ok_or(AuthError::Malformed("X-Hub-Signature-256"))
                .and_then(|hex| hmac_hex(secret, body, hex))
        }
        AuthScheme::Sentry => {
            let sig = header(headers, "sentry-hook-signature")?;
            provider_secret().and_then(|s| hmac_hex(s, body, sig))
        }
        AuthScheme::Linear => {
            let sig = header(headers, "linear-signature")?;
            provider_secret()
                .and_then(|s| hmac_hex(s, body, sig))
                .and_then(|()| linear_fresh(body, now))
        }
        AuthScheme::Pagerduty => {
            let sig = header(headers, "x-pagerduty-signature")?;
            provider_secret().and_then(|s| {
                let any = sig
                    .split(',')
                    .filter_map(|part| part.trim().strip_prefix("v1="))
                    .any(|hex| hmac_hex(s, body, hex).is_ok());
                if any {
                    Ok(())
                } else {
                    Err(AuthError::BadSignature)
                }
            })
        }
        AuthScheme::Stripe => {
            let sig = header(headers, "stripe-signature")?;
            provider_secret().and_then(|s| stripe(s, sig, body, now))
        }
        AuthScheme::Svix => {
            let sig = header(headers, "svix-signature")?;
            provider_secret().and_then(|s| svix(s, headers, sig, body, now))
        }
    })
}

fn hmac_hex(secret: &str, data: &[u8], expected_hex: &str) -> Result<(), AuthError> {
    let expected = hex::decode(expected_hex.trim()).map_err(|_| AuthError::BadSignature)?;
    hmac_sha256_verify(secret.as_bytes(), data, &expected, "subscription")
        .map_err(|_| AuthError::BadSignature)
}

fn stripe(secret: &str, sig: &str, body: &[u8], now: i64) -> Result<(), AuthError> {
    let mut ts = None;
    let mut candidates = Vec::new();
    for part in sig.split(',') {
        match part.trim().split_once('=') {
            Some(("t", v)) => ts = v.parse::<i64>().ok(),
            Some(("v1", v)) => candidates.push(v),
            _ => {}
        }
    }
    let ts = ts.ok_or(AuthError::Malformed("Stripe-Signature"))?;
    if stale(now, ts, SIGNATURE_TOLERANCE_SECS) {
        return Err(AuthError::Stale);
    }
    let mut signed = format!("{ts}.").into_bytes();
    signed.extend_from_slice(body);
    if candidates
        .iter()
        .any(|hex| hmac_hex(secret, &signed, hex).is_ok())
    {
        Ok(())
    } else {
        Err(AuthError::BadSignature)
    }
}

fn svix(
    secret: &str,
    headers: &HeaderMap,
    sig: &str,
    body: &[u8],
    now: i64,
) -> Result<(), AuthError> {
    let id = header(headers, "svix-id").ok_or(AuthError::Malformed("svix-id"))?;
    let ts_text =
        header(headers, "svix-timestamp").ok_or(AuthError::Malformed("svix-timestamp"))?;
    let ts: i64 = ts_text
        .parse()
        .map_err(|_| AuthError::Malformed("svix-timestamp"))?;
    if stale(now, ts, SIGNATURE_TOLERANCE_SECS) {
        return Err(AuthError::Stale);
    }
    let key = STANDARD
        .decode(
            secret
                .trim()
                .strip_prefix("whsec_")
                .unwrap_or(secret.trim()),
        )
        .map_err(|_| AuthError::BadSignature)?;
    let mut signed = format!("{id}.{ts}.").into_bytes();
    signed.extend_from_slice(body);
    let ok = sig
        .split_whitespace()
        .filter_map(|part| part.strip_prefix("v1,"))
        .filter_map(|b64| STANDARD.decode(b64).ok())
        .any(|expected| hmac_sha256_verify(&key, &signed, &expected, "subscription").is_ok());
    if ok {
        Ok(())
    } else {
        Err(AuthError::BadSignature)
    }
}

fn linear_fresh(body: &[u8], now: i64) -> Result<(), AuthError> {
    let ts = serde_json::from_slice::<serde_json::Value>(body)
        .ok()
        .and_then(|v| v.get("webhookTimestamp")?.as_i64());
    match ts {
        Some(ms) if stale(now.saturating_mul(1000), ms, LINEAR_TOLERANCE_MS) => {
            Err(AuthError::Stale)
        }
        _ => Ok(()),
    }
}

fn verify_key(headers: &HeaderMap, key: &str) -> Result<(), AuthError> {
    if key.trim().is_empty() {
        return Err(AuthError::BadKey);
    }
    let auth = header(headers, "authorization").ok_or(AuthError::Missing)?;
    let presented = if let Some(token) = auth.strip_prefix("Bearer ") {
        token.trim().to_string()
    } else if let Some(b64) = auth.strip_prefix("Basic ") {
        let decoded = STANDARD
            .decode(b64.trim())
            .map_err(|_| AuthError::Malformed("Authorization"))?;
        let text = String::from_utf8(decoded).map_err(|_| AuthError::Malformed("Authorization"))?;
        // The key is the password; the user name is ignored.
        text.split_once(':')
            .map(|(_, p)| p.to_string())
            .unwrap_or(text)
    } else {
        return Err(AuthError::Malformed("Authorization"));
    };
    if constant_time_eq(presented.as_bytes(), key.as_bytes()) {
        Ok(())
    } else {
        Err(AuthError::BadKey)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;
    use hmac::{Hmac, KeyInit, Mac};
    use sha2::Sha256;

    const KEY: &str = "bsk_0123";
    const NOW: i64 = 1_792_800_000;

    fn sign_hex(secret: &[u8], data: &[u8]) -> String {
        let mut mac = Hmac::<Sha256>::new_from_slice(secret).unwrap();
        mac.update(data);
        hex::encode(mac.finalize().into_bytes())
    }

    fn headers(pairs: &[(&'static str, String)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(*k, HeaderValue::from_str(v).unwrap());
        }
        h
    }

    fn secrets(signing: Option<&'static str>) -> Secrets<'static> {
        Secrets {
            key: KEY,
            signing_secret: signing,
        }
    }

    /// GitHub's published example: secret "It's a Secret to Everybody",
    /// payload "Hello, World!".
    #[test]
    fn github_published_vector_verifies() {
        let body = b"Hello, World!";
        let h = headers(&[(
            "x-hub-signature-256",
            "sha256=757107ea0eb2509fc211221cce984b8a37570b6d7586c22c46f4379c8b043e17".into(),
        )]);
        let s = Secrets {
            key: "It's a Secret to Everybody",
            signing_secret: None,
        };
        assert_eq!(verify(AuthScheme::Github, &h, body, s, NOW), Ok(()));
        let wrong = Secrets {
            key: "other",
            signing_secret: None,
        };
        assert_eq!(
            verify(AuthScheme::Github, &h, body, wrong, NOW),
            Err(AuthError::BadSignature)
        );
    }

    #[test]
    fn a_bad_signature_is_not_rescued_by_a_good_bearer_key() {
        let h = headers(&[
            ("x-hub-signature-256", "sha256=00".into()),
            ("authorization", format!("Bearer {KEY}")),
        ]);
        assert_eq!(
            verify(AuthScheme::Github, &h, b"{}", secrets(None), NOW),
            Err(AuthError::BadSignature)
        );
    }

    #[test]
    fn a_pasted_signing_secret_makes_the_signature_required() {
        let bearer = headers(&[("authorization", format!("Bearer {KEY}"))]);
        for scheme in [
            AuthScheme::Stripe,
            AuthScheme::Sentry,
            AuthScheme::Pagerduty,
            AuthScheme::Linear,
            AuthScheme::Svix,
        ] {
            let err = verify(scheme, &bearer, b"{}", secrets(Some("whsec_x")), NOW).unwrap_err();
            assert_eq!(
                err,
                AuthError::SignatureRequired(signature_header(scheme)),
                "{scheme:?}"
            );
            // The sender's log names what is missing, not "no credentials".
            assert!(err.to_string().contains(signature_header(scheme)), "{err}");
        }
        // GitHub signs with Borg's own key, so the key stays accepted.
        assert_eq!(
            verify(AuthScheme::Github, &bearer, b"{}", secrets(Some("gh")), NOW),
            Ok(())
        );
    }

    #[test]
    fn crafted_timestamps_are_stale_not_a_crash() {
        for t in [i64::MIN, i64::MAX] {
            let h = headers(&[("stripe-signature", format!("t={t},v1=00"))]);
            assert_eq!(
                verify(AuthScheme::Stripe, &h, b"{}", secrets(Some("whsec_x")), NOW),
                Err(AuthError::Stale)
            );
            let svix_h = headers(&[
                ("svix-id", "msg_1".to_string()),
                ("svix-timestamp", t.to_string()),
                ("svix-signature", "v1,AAAA".to_string()),
            ]);
            assert_eq!(
                verify(
                    AuthScheme::Svix,
                    &svix_h,
                    b"{}",
                    secrets(Some("whsec_eA==")),
                    NOW
                ),
                Err(AuthError::Stale)
            );
        }
    }

    #[test]
    fn bearer_and_basic_keys_work_for_every_scheme_without_a_signature() {
        let basic = STANDARD.encode(format!("borg:{KEY}"));
        for scheme in [
            AuthScheme::Bearer,
            AuthScheme::Github,
            AuthScheme::Sentry,
            AuthScheme::Stripe,
        ] {
            let bearer = headers(&[("authorization", format!("Bearer {KEY}"))]);
            assert_eq!(
                verify(scheme, &bearer, b"{}", secrets(None), NOW),
                Ok(()),
                "{scheme:?}"
            );
            let basic_h = headers(&[("authorization", format!("Basic {basic}"))]);
            assert_eq!(
                verify(scheme, &basic_h, b"{}", secrets(None), NOW),
                Ok(()),
                "{scheme:?}"
            );
        }
        let wrong = headers(&[("authorization", "Bearer nope".into())]);
        assert_eq!(
            verify(AuthScheme::Bearer, &wrong, b"{}", secrets(None), NOW),
            Err(AuthError::BadKey)
        );
        assert_eq!(
            verify(
                AuthScheme::Bearer,
                &HeaderMap::new(),
                b"{}",
                secrets(None),
                NOW
            ),
            Err(AuthError::Missing)
        );
    }

    #[test]
    fn provider_signed_schemes_need_the_pasted_secret() {
        let body = br#"{"a":1}"#;
        let sig = sign_hex(b"sentry-secret", body);
        let h = headers(&[("sentry-hook-signature", sig)]);
        assert_eq!(
            verify(AuthScheme::Sentry, &h, body, secrets(None), NOW),
            Err(AuthError::NeedsSigningSecret)
        );
        assert_eq!(
            verify(
                AuthScheme::Sentry,
                &h,
                body,
                secrets(Some("sentry-secret")),
                NOW
            ),
            Ok(())
        );
        assert_eq!(
            verify(
                AuthScheme::Sentry,
                &h,
                br#"{"a":2}"#,
                secrets(Some("sentry-secret")),
                NOW
            ),
            Err(AuthError::BadSignature)
        );
    }

    #[test]
    fn pagerduty_accepts_any_of_several_v1_signatures() {
        let body = b"{}";
        let good = sign_hex(b"pd", body);
        let h = headers(&[("x-pagerduty-signature", format!("v1=deadbeef,v1={good}"))]);
        assert_eq!(
            verify(AuthScheme::Pagerduty, &h, body, secrets(Some("pd")), NOW),
            Ok(())
        );
        let h = headers(&[("x-pagerduty-signature", "v1=deadbeef".into())]);
        assert_eq!(
            verify(AuthScheme::Pagerduty, &h, body, secrets(Some("pd")), NOW),
            Err(AuthError::BadSignature)
        );
    }

    #[test]
    fn stripe_signs_timestamp_dot_body_and_rejects_stale_ones() {
        let body = br#"{"id":"evt_1"}"#;
        let ts = NOW - 10;
        let mut signed = format!("{ts}.").into_bytes();
        signed.extend_from_slice(body);
        let sig = sign_hex(b"whsec_x", &signed);
        let h = headers(&[("stripe-signature", format!("t={ts},v1={sig},v0=ignored"))]);
        assert_eq!(
            verify(AuthScheme::Stripe, &h, body, secrets(Some("whsec_x")), NOW),
            Ok(())
        );
        assert_eq!(
            verify(
                AuthScheme::Stripe,
                &h,
                body,
                secrets(Some("whsec_x")),
                NOW + 1000
            ),
            Err(AuthError::Stale)
        );
        let no_ts = headers(&[("stripe-signature", format!("v1={sig}"))]);
        assert_eq!(
            verify(
                AuthScheme::Stripe,
                &no_ts,
                body,
                secrets(Some("whsec_x")),
                NOW
            ),
            Err(AuthError::Malformed("Stripe-Signature"))
        );
    }

    #[test]
    fn linear_checks_signature_and_delivery_age() {
        let fresh = format!(r#"{{"webhookTimestamp":{}}}"#, NOW * 1000 - 5_000);
        let sig = sign_hex(b"lin", fresh.as_bytes());
        let h = headers(&[("linear-signature", sig)]);
        assert_eq!(
            verify(
                AuthScheme::Linear,
                &h,
                fresh.as_bytes(),
                secrets(Some("lin")),
                NOW
            ),
            Ok(())
        );

        let old = format!(r#"{{"webhookTimestamp":{}}}"#, NOW * 1000 - 120_000);
        let sig = sign_hex(b"lin", old.as_bytes());
        let h = headers(&[("linear-signature", sig)]);
        assert_eq!(
            verify(
                AuthScheme::Linear,
                &h,
                old.as_bytes(),
                secrets(Some("lin")),
                NOW
            ),
            Err(AuthError::Stale)
        );
    }

    #[test]
    fn svix_signs_id_timestamp_body_with_the_decoded_secret() {
        let raw_key = b"svix-key-bytes-0123456789abcdef";
        let secret: &'static str =
            Box::leak(format!("whsec_{}", STANDARD.encode(raw_key)).into_boxed_str());
        let body = br#"{"event_type":"message.received"}"#;
        let ts = NOW - 5;
        let mut signed = format!("msg_1.{ts}.").into_bytes();
        signed.extend_from_slice(body);
        let mut mac = Hmac::<Sha256>::new_from_slice(raw_key).unwrap();
        mac.update(&signed);
        let sig = STANDARD.encode(mac.finalize().into_bytes());
        let h = headers(&[
            ("svix-id", "msg_1".into()),
            ("svix-timestamp", ts.to_string()),
            ("svix-signature", format!("v1,bm9wZQ== v1,{sig}")),
        ]);
        assert_eq!(
            verify(AuthScheme::Svix, &h, body, secrets(Some(secret)), NOW),
            Ok(())
        );
        let h2 = headers(&[
            ("svix-id", "msg_2".into()),
            ("svix-timestamp", ts.to_string()),
            ("svix-signature", format!("v1,{sig}")),
        ]);
        assert_eq!(
            verify(AuthScheme::Svix, &h2, body, secrets(Some(secret)), NOW),
            Err(AuthError::BadSignature),
            "the id is part of what is signed"
        );
    }
}
