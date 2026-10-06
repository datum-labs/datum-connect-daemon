//! Datum service-account auth, done in-process.
//!
//! Datum's IdP (Zitadel) accepts the OAuth 2.0 JWT-bearer grant: sign a
//! short-lived assertion with the service account's private key and exchange
//! it for an access token. No browser and no refresh token, which is why an
//! appliance uses this rather than a human session — a personal login was
//! observed dying after two days, taking the tunnel with it.
//!
//! This replaces the Home Assistant add-on's `sa-credentials-helper.sh`
//! (openssl + curl + jq), byte for byte in what it sends. Like that helper,
//! every [`ServiceAccount::mint`] performs a fresh exchange: the refresh loop
//! calls it on every 401, and a source that hands back a cached token
//! instead produces an unrecoverable retry loop (issue #12).
//!
//! Signing uses `ring`, already in the dependency tree through iroh's QUIC
//! stack and the daemon's rustls provider, so no new crate is compiled.

use std::path::Path;
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use ring::rand::SystemRandom;
use ring::signature::{RSA_PKCS1_SHA256, RsaKeyPair};
use serde::Deserialize;
use serde_json::json;

use super::external_token_source::ExternalTokenError;

/// The key file's `type`, as Datum writes it.
pub const KEY_TYPE: &str = "datum_service_account";
pub const DEFAULT_ISSUER: &str = "https://auth.datum.net";

/// Deliberately short-lived: the assertion only has to survive one round
/// trip, and the access token it buys carries the real lifetime.
const ASSERTION_LIFETIME_SECS: u64 = 300;
const EXCHANGE_TIMEOUT: Duration = Duration::from_secs(30);
const JWT_BEARER_GRANT: &str = "urn:ietf:params:oauth:grant-type:jwt-bearer";

#[derive(Deserialize)]
struct KeyFile {
    #[serde(rename = "type")]
    key_type: String,
    client_id: String,
    private_key_id: String,
    private_key: String,
    scope: String,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
}

/// A loaded service account key, ready to mint tokens.
pub struct ServiceAccount {
    client_id: String,
    kid: String,
    scope: String,
    signer: RsaKeyPair,
    issuer: String,
    http: reqwest::Client,
}

impl std::fmt::Debug for ServiceAccount {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServiceAccount")
            .field("client_id", &self.client_id)
            .field("issuer", &self.issuer)
            .finish_non_exhaustive()
    }
}

fn sa_err(msg: impl Into<String>) -> ExternalTokenError {
    ExternalTokenError::ServiceAccount(msg.into())
}

impl ServiceAccount {
    pub fn from_file(path: &Path, issuer: &str) -> Result<Self, ExternalTokenError> {
        let raw = std::fs::read_to_string(path)
            .map_err(|e| sa_err(format!("cannot read key file {}: {e}", path.display())))?;
        Self::from_json(&raw, issuer)
            .map_err(|e| sa_err(format!("{}: {e}", path.display())))
    }

    /// Errors never quote the key, only which part of it is wrong.
    pub fn from_json(raw: &str, issuer: &str) -> Result<Self, ExternalTokenError> {
        let key: KeyFile = serde_json::from_str(raw).map_err(|e| {
            sa_err(format!(
                "not a Datum service account key (expected JSON with type, client_id, private_key_id, private_key and scope): {e}"
            ))
        })?;
        if key.key_type != KEY_TYPE {
            return Err(sa_err(format!(
                "key type is {:?}, expected {KEY_TYPE:?}",
                key.key_type
            )));
        }
        let signer = rsa_key_from_pem(&key.private_key)
            .map_err(|e| sa_err(format!("private_key: {e}")))?;
        let http = reqwest::Client::builder()
            .user_agent(crate::datum_http_user_agent())
            .timeout(EXCHANGE_TIMEOUT)
            .build()
            .map_err(|e| sa_err(format!("cannot build HTTP client: {e}")))?;
        Ok(Self {
            client_id: key.client_id,
            kid: key.private_key_id,
            scope: key.scope,
            signer,
            issuer: issuer.trim_end_matches('/').to_string(),
            http,
        })
    }

    pub fn client_id(&self) -> &str {
        &self.client_id
    }

    fn token_endpoint(&self) -> String {
        format!("{}/oauth/v2/token", self.issuer)
    }

    /// The signed JWT-bearer assertion, issued at `now` (Unix seconds).
    pub fn assertion(&self, now: u64) -> Result<String, ExternalTokenError> {
        let header = json!({"alg": "RS256", "kid": self.kid, "typ": "JWT"});
        let claims = json!({
            "iss": self.client_id,
            "sub": self.client_id,
            "aud": self.issuer,
            "iat": now,
            "exp": now + ASSERTION_LIFETIME_SECS,
        });
        let signing_input = format!(
            "{}.{}",
            URL_SAFE_NO_PAD.encode(header.to_string()),
            URL_SAFE_NO_PAD.encode(claims.to_string())
        );
        let mut signature = vec![0u8; self.signer.public().modulus_len()];
        self.signer
            .sign(
                &RSA_PKCS1_SHA256,
                &SystemRandom::new(),
                signing_input.as_bytes(),
                &mut signature,
            )
            .map_err(|_| sa_err("signing the assertion failed"))?;
        Ok(format!("{signing_input}.{}", URL_SAFE_NO_PAD.encode(signature)))
    }

    /// Exchanges a freshly signed assertion for a new access token.
    pub async fn mint(&self) -> Result<String, ExternalTokenError> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or_default();
        let assertion = self.assertion(now)?;
        let response = self
            .http
            .post(self.token_endpoint())
            .form(&[
                ("grant_type", JWT_BEARER_GRANT),
                ("assertion", assertion.as_str()),
                ("scope", self.scope.as_str()),
            ])
            .send()
            .await
            .map_err(|e| sa_err(format!("token exchange failed: {e}")))?;
        let status = response.status();
        let body = response
            .text()
            .await
            .map_err(|e| sa_err(format!("token exchange failed reading the response: {e}")))?;
        if !status.is_success() {
            let body: String = body.chars().take(300).collect();
            return Err(sa_err(format!("token exchange failed (HTTP {status}): {body}")));
        }
        let token: TokenResponse = serde_json::from_str(&body)
            .map_err(|_| sa_err("no access_token in the token response"))?;
        if token.access_token.is_empty() {
            return Err(sa_err("empty access_token in the token response"));
        }
        Ok(token.access_token)
    }
}

/// Accepts PKCS#1 (`RSA PRIVATE KEY`, what Datum issues) and PKCS#8
/// (`PRIVATE KEY`) PEM.
fn rsa_key_from_pem(pem: &str) -> Result<RsaKeyPair, String> {
    let (label, der) = decode_pem(pem)?;
    match label.as_str() {
        "RSA PRIVATE KEY" => RsaKeyPair::from_der(&der).map_err(|e| format!("bad RSA key: {e}")),
        "PRIVATE KEY" => RsaKeyPair::from_pkcs8(&der).map_err(|e| format!("bad RSA key: {e}")),
        other => Err(format!("expected an RSA private key, found PEM {other:?}")),
    }
}

fn decode_pem(pem: &str) -> Result<(String, Vec<u8>), String> {
    const BEGIN: &str = "-----BEGIN ";
    let start = pem.find(BEGIN).ok_or("no PEM BEGIN line")? + BEGIN.len();
    let rest = &pem[start..];
    let label_end = rest.find("-----").ok_or("malformed PEM BEGIN line")?;
    let label = &rest[..label_end];
    let body = &rest[label_end + 5..];
    let end = body
        .find(&format!("-----END {label}-----"))
        .ok_or("no matching PEM END line")?;
    let b64: String = body[..end].chars().filter(|c| !c.is_whitespace()).collect();
    let der = STANDARD
        .decode(b64)
        .map_err(|e| format!("PEM body is not base64: {e}"))?;
    Ok((label.to_string(), der))
}

#[cfg(test)]
pub(crate) mod tests {
    use aws_lc_rs::encoding::AsDer;
    use ring::signature::{RSA_PKCS1_2048_8192_SHA256, UnparsedPublicKey};

    use super::*;

    /// A throwaway key, generated per test run. Never a real one.
    pub(crate) struct TestKey {
        pub pkcs8_der: Vec<u8>,
        pub public_der: Vec<u8>,
    }

    pub(crate) fn test_key() -> TestKey {
        let kp = aws_lc_rs::rsa::KeyPair::generate(aws_lc_rs::rsa::KeySize::Rsa2048).unwrap();
        let pkcs8: aws_lc_rs::encoding::Pkcs8V1Der = kp.as_der().unwrap();
        TestKey {
            pkcs8_der: pkcs8.as_ref().to_vec(),
            public_der: aws_lc_rs::signature::KeyPair::public_key(&kp).as_ref().to_vec(),
        }
    }

    fn pem(label: &str, der: &[u8]) -> String {
        let b64 = STANDARD.encode(der);
        let lines: Vec<&str> = b64
            .as_bytes()
            .chunks(64)
            .map(|c| std::str::from_utf8(c).unwrap())
            .collect();
        format!("-----BEGIN {label}-----\n{}\n-----END {label}-----\n", lines.join("\n"))
    }

    /// The RSAPrivateKey inside a PKCS#8 PrivateKeyInfo, i.e. the PKCS#1
    /// form Datum's key files carry.
    fn pkcs1_from_pkcs8(der: &[u8]) -> Vec<u8> {
        // Returns (content start, content len) of the TLV at `at`.
        fn tlv(der: &[u8], at: usize) -> (usize, usize) {
            let first = der[at + 1] as usize;
            if first < 0x80 {
                (at + 2, first)
            } else {
                let n = first & 0x7f;
                let len = der[at + 2..at + 2 + n]
                    .iter()
                    .fold(0usize, |acc, b| (acc << 8) | *b as usize);
                (at + 2 + n, len)
            }
        }
        let (seq, _) = tlv(der, 0); // PrivateKeyInfo SEQUENCE
        let (v, vlen) = tlv(der, seq); // version INTEGER
        let (alg, alglen) = tlv(der, v + vlen); // AlgorithmIdentifier
        let (key, keylen) = tlv(der, alg + alglen); // privateKey OCTET STRING
        der[key..key + keylen].to_vec()
    }

    pub(crate) fn key_json(private_key_pem: &str) -> String {
        json!({
            "type": KEY_TYPE,
            "client_id": "123456789012345678",
            "private_key_id": "kid-abc",
            "private_key": private_key_pem,
            "scope": "openid profile urn:zitadel:iam:org:project:id:zitadel:aud",
            "client_email": "ha@my-project.identity.miloapis.com",
        })
        .to_string()
    }

    pub(crate) fn pkcs1_key_json(key: &TestKey) -> String {
        key_json(&pem("RSA PRIVATE KEY", &pkcs1_from_pkcs8(&key.pkcs8_der)))
    }

    fn decode_segment(seg: &str) -> serde_json::Value {
        serde_json::from_slice(&URL_SAFE_NO_PAD.decode(seg).unwrap()).unwrap()
    }

    /// Splits an assertion, checks its signature against `public_der`, and
    /// returns the decoded header and claims.
    pub(crate) fn verify_assertion(
        assertion: &str,
        public_der: &[u8],
    ) -> (serde_json::Value, serde_json::Value) {
        let parts: Vec<&str> = assertion.split('.').collect();
        assert_eq!(parts.len(), 3, "header.claims.signature");
        let signing_input = format!("{}.{}", parts[0], parts[1]);
        let signature = URL_SAFE_NO_PAD.decode(parts[2]).unwrap();
        UnparsedPublicKey::new(&RSA_PKCS1_2048_8192_SHA256, public_der)
            .verify(signing_input.as_bytes(), &signature)
            .expect("signature verifies with the key's public half");
        (decode_segment(parts[0]), decode_segment(parts[1]))
    }

    #[test]
    fn assertion_has_the_helpers_header_and_claims_and_verifies() {
        let key = test_key();
        let sa = ServiceAccount::from_json(&pkcs1_key_json(&key), "https://auth.example.test")
            .unwrap();
        let (header, claims) = verify_assertion(&sa.assertion(1_700_000_000).unwrap(), &key.public_der);
        assert_eq!(header, json!({"alg": "RS256", "kid": "kid-abc", "typ": "JWT"}));
        assert_eq!(
            claims,
            json!({
                "iss": "123456789012345678",
                "sub": "123456789012345678",
                "aud": "https://auth.example.test",
                "iat": 1_700_000_000u64,
                "exp": 1_700_000_300u64,
            })
        );
    }

    #[test]
    fn pkcs8_keys_work_too_and_trailing_slash_on_issuer_is_dropped() {
        let key = test_key();
        let sa = ServiceAccount::from_json(&key_json(&pem("PRIVATE KEY", &key.pkcs8_der)), "https://auth.example.test/")
            .unwrap();
        let (_, claims) = verify_assertion(&sa.assertion(42).unwrap(), &key.public_der);
        assert_eq!(claims["aud"], "https://auth.example.test");
        assert_eq!(sa.token_endpoint(), "https://auth.example.test/oauth/v2/token");
    }

    #[test]
    fn rejects_what_is_not_a_service_account_key() {
        let key = test_key();
        let good: serde_json::Value = serde_json::from_str(&pkcs1_key_json(&key)).unwrap();
        let with = |field: &str, value: serde_json::Value| {
            let mut v = good.clone();
            v[field] = value;
            v.to_string()
        };
        let without = |field: &str| {
            let mut v = good.clone();
            v.as_object_mut().unwrap().remove(field);
            v.to_string()
        };
        for (what, raw) in [
            ("not JSON", "{ nope".to_string()),
            ("personal token", "\"eyJhbGciOi...\"".to_string()),
            ("wrong type", with("type", json!("authorized_user"))),
            ("no private_key_id", without("private_key_id")),
            ("no scope", without("scope")),
            ("not PEM", with("private_key", json!("hunter2"))),
            ("EC key label", with("private_key", json!(pem("EC PRIVATE KEY", b"x")))),
        ] {
            let err = ServiceAccount::from_json(&raw, DEFAULT_ISSUER).unwrap_err();
            assert!(
                matches!(err, ExternalTokenError::ServiceAccount(_)),
                "{what}: {err}"
            );
        }
    }
}
