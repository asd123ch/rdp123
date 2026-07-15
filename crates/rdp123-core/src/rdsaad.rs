//! Microsoft Entra RDS AAD authentication (`enablerdsaadauth:i:1`).
//!
//! Implements [MS-RDPBCGR] section 5.4.7: a Microsoft OAuth access token is
//! bound to an ephemeral RSA proof-of-possession key and exchanged directly
//! over the RDP TLS transport before the normal MCS handshake.

use std::net::IpAddr;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Context, Result};
use aws_lc_rs::rand::SystemRandom;
use aws_lc_rs::rsa::{KeyPair as RsaKeyPair, KeySize};
use aws_lc_rs::signature::{KeyPair as _, RSA_PKCS1_SHA256};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use serde::Deserialize;
use serde_json::json;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _};
use tokio::sync::oneshot;
use url::Url;
use zeroize::Zeroizing;

use crate::session::SessionEvent;

const CLIENT_ID: &str = "a85cf173-4192-42f8-81fa-777a763e6e2c";
// The ms-appx-web redirect used by Windows Web Account Manager expects the
// Windows AAD broker and can leave a macOS web view spinning after MFA. Use
// the public-client native redirect also used by FreeRDP's browser flow.
const REDIRECT_URI: &str = "https://login.microsoftonline.com/common/oauth2/nativeclient";
const AUTHORIZE_ENDPOINT: &str = "https://login.microsoftonline.com/common/oauth2/v2.0/authorize";
const TOKEN_ENDPOINT: &str = "https://login.microsoftonline.com/common/oauth2/v2.0/token";
const AUTH_TIMEOUT: Duration = Duration::from_secs(120);
const INTERACTIVE_AUTH_TIMEOUT: Duration = Duration::from_secs(10 * 60);
const MAX_MESSAGE_SIZE: usize = 64 * 1024;

pub(crate) struct RdsAadClient {
    resource: String,
    scope: String,
    key: RsaKeyPair,
    exponent: String,
    modulus: String,
    request_confirmation: String,
    access_token: Option<Zeroizing<String>>,
    token_valid_until: Option<Instant>,
    http: reqwest::Client,
}

impl RdsAadClient {
    pub(crate) fn new(host: &str) -> Result<Self> {
        // reqwest's rustls backend deliberately has no built-in provider in
        // this build; use the same AWS-LC provider as the RDP TLS transport.
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        let host = host.trim().trim_end_matches('.');
        if host.parse::<IpAddr>().is_ok() {
            bail!("Microsoft Entra web authentication requires a hostname, not an IP address");
        }

        // Entra device names are NetBIOS-style hostnames. Match FreeRDP and
        // Windows by using the first DNS label when the profile contains an FQDN.
        let hostname = host
            .split('.')
            .next()
            .filter(|part| !part.is_empty())
            .context("Microsoft Entra web authentication requires a hostname")?
            .to_string();
        let resource = format!("ms-device-service://termsrv.wvd.microsoft.com/name/{hostname}");
        let scope = format!("{resource}/user_impersonation");

        let key = RsaKeyPair::generate(KeySize::Rsa2048)
            .map_err(|_| anyhow!("could not generate the Entra proof-of-possession key"))?;
        let public = key.public_key();
        let exponent = URL_SAFE_NO_PAD.encode(public.exponent().big_endian_without_leading_zero());
        let modulus = URL_SAFE_NO_PAD.encode(public.modulus().big_endian_without_leading_zero());

        // RFC 7638 requires this exact member order for an RSA JWK thumbprint.
        let canonical_jwk = format!(r#"{{"e":"{exponent}","kty":"RSA","n":"{modulus}"}}"#);
        let thumbprint = URL_SAFE_NO_PAD.encode(Sha256::digest(canonical_jwk.as_bytes()));
        let request_confirmation = URL_SAFE_NO_PAD.encode(
            serde_json::to_vec(&json!({ "kid": thumbprint }))
                .expect("serializing a static JSON object cannot fail"),
        );

        Ok(Self {
            resource,
            scope,
            key,
            exponent,
            modulus,
            request_confirmation,
            access_token: None,
            token_valid_until: None,
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(30))
                .build()
                .context("creating Microsoft authentication client")?,
        })
    }

    pub(crate) async fn authenticate<S>(
        &mut self,
        stream: &mut S,
        username_hint: &str,
        event_cb: &dyn Fn(SessionEvent),
    ) -> Result<()>
    where
        S: AsyncRead + AsyncWrite + Unpin,
    {
        self.ensure_access_token(username_hint, event_cb).await?;
        let aad_nonce = self.fetch_aad_nonce().await?;
        let server_nonce = read_json_message::<ServerNonce, _>(stream)
            .await
            .context("reading the RDS AAD server nonce")?;
        let assertion = self
            .create_assertion(&server_nonce.ts_nonce, &aad_nonce)
            .context("creating the RDS AAD proof-of-possession assertion")?;
        write_json_message(stream, &json!({ "rdp_assertion": assertion }))
            .await
            .context("sending the RDS AAD assertion")?;

        let result = read_json_message::<AuthenticationResult, _>(stream)
            .await
            .context("reading the RDS AAD authentication result")?;
        if result.authentication_result != 0 {
            let code = result.authentication_result as u32;
            bail!(
                "Microsoft Entra rejected the RDP sign-in ({})",
                auth_error(code)
            );
        }
        Ok(())
    }

    async fn ensure_access_token(
        &mut self,
        username_hint: &str,
        event_cb: &dyn Fn(SessionEvent),
    ) -> Result<()> {
        if self
            .token_valid_until
            .is_some_and(|until| until > Instant::now() + Duration::from_secs(60))
            && self.access_token.is_some()
        {
            return Ok(());
        }

        let state = random_state()?;
        let mut authorize = Url::parse(AUTHORIZE_ENDPOINT).expect("static Microsoft URL is valid");
        {
            let mut query = authorize.query_pairs_mut();
            query
                .append_pair("client_id", CLIENT_ID)
                .append_pair("response_type", "code")
                .append_pair("response_mode", "query")
                .append_pair("scope", &self.scope)
                .append_pair("redirect_uri", REDIRECT_URI)
                .append_pair("state", &state);
            if !username_hint.trim().is_empty() {
                query.append_pair("login_hint", username_hint.trim());
            }
        }

        let (reply, rx) = oneshot::channel();
        event_cb(SessionEvent::EntraSignIn {
            authorization_url: authorize.into(),
            redirect_uri: REDIRECT_URI.to_string(),
            reply,
        });
        let redirected = tokio::time::timeout(INTERACTIVE_AUTH_TIMEOUT, rx)
            .await
            .map_err(|_| anyhow!("Microsoft sign-in timed out; try again"))?
            .map_err(|_| anyhow!("the Microsoft sign-in window was closed"))?
            .map_err(|message| anyhow!(message))?;
        let code = authorization_code(&redirected, &state)?;

        let response = self
            .http
            .post(TOKEN_ENDPOINT)
            .form(&[
                ("grant_type", "authorization_code"),
                ("code", code.as_str()),
                ("client_id", CLIENT_ID),
                ("scope", self.scope.as_str()),
                ("redirect_uri", REDIRECT_URI),
                ("req_cnf", self.request_confirmation.as_str()),
            ])
            .send()
            .await
            .context("contacting Microsoft for the RDP access token")?;
        let token: TokenResponse = decode_response(response, "RDP access token").await?;
        self.token_valid_until =
            Some(Instant::now() + Duration::from_secs(token.expires_in.unwrap_or(3600).max(60)));
        self.access_token = Some(Zeroizing::new(token.access_token));
        Ok(())
    }

    async fn fetch_aad_nonce(&self) -> Result<Zeroizing<String>> {
        let response = self
            .http
            .post(TOKEN_ENDPOINT)
            .form(&[("grant_type", "srv_challenge")])
            .send()
            .await
            .context("requesting the Microsoft RDP nonce")?;
        let nonce: NonceResponse = decode_response(response, "RDP nonce").await?;
        Ok(Zeroizing::new(nonce.nonce))
    }

    fn create_assertion(&self, server_nonce: &str, aad_nonce: &str) -> Result<String> {
        let access_token = self
            .access_token
            .as_deref()
            .context("Microsoft access token is missing")?;
        let header = json!({
            "alg": "RS256",
            "kid": self.request_confirmation,
        });
        let client_claims = serde_json::to_string(&json!({ "aad_nonce": aad_nonce }))?;
        let payload = json!({
            "ts": SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs().to_string(),
            "at": access_token,
            "u": self.resource,
            "nonce": server_nonce,
            "cnf": {
                "jwk": {
                    "kty": "RSA",
                    "e": self.exponent,
                    "n": self.modulus,
                }
            },
            "client_claims": client_claims,
        });
        let encoded_header = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&header)?);
        let encoded_payload = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&payload)?);
        let signing_input = format!("{encoded_header}.{encoded_payload}");
        let mut signature = vec![0; self.key.public_modulus_len()];
        self.key
            .sign(
                &RSA_PKCS1_SHA256,
                &SystemRandom::new(),
                signing_input.as_bytes(),
                &mut signature,
            )
            .map_err(|_| anyhow!("signing the RDS AAD assertion failed"))?;
        Ok(format!(
            "{signing_input}.{}",
            URL_SAFE_NO_PAD.encode(signature)
        ))
    }
}

fn random_state() -> Result<String> {
    let rng = SystemRandom::new();
    let mut bytes = [0_u8; 32];
    aws_lc_rs::rand::SecureRandom::fill(&rng, &mut bytes)
        .map_err(|_| anyhow!("could not generate OAuth state"))?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

fn authorization_code(redirected: &str, expected_state: &str) -> Result<String> {
    let url = Url::parse(redirected).context("Microsoft returned an invalid redirect URL")?;
    let expected = Url::parse(REDIRECT_URI).expect("static Microsoft redirect URL is valid");
    if url.scheme() != expected.scheme()
        || url.host_str() != expected.host_str()
        || url.path() != expected.path()
    {
        bail!("Microsoft returned an unexpected redirect URL");
    }
    let values: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();
    if let Some(error) = values.get("error") {
        let description = values
            .get("error_description")
            .map(String::as_str)
            .unwrap_or(error);
        bail!("Microsoft sign-in failed: {description}");
    }
    if values.get("state").map(String::as_str) != Some(expected_state) {
        bail!("Microsoft sign-in returned an invalid OAuth state");
    }
    values
        .get("code")
        .cloned()
        .context("Microsoft sign-in did not return an authorization code")
}

async fn decode_response<T: for<'de> Deserialize<'de>>(
    response: reqwest::Response,
    operation: &str,
) -> Result<T> {
    let status = response.status();
    let body = response
        .text()
        .await
        .with_context(|| format!("reading the Microsoft {operation} response"))?;
    if !status.is_success() {
        let detail = serde_json::from_str::<OAuthError>(&body)
            .ok()
            .and_then(|error| error.error_description.or(error.error))
            .unwrap_or_else(|| format!("HTTP {status}"));
        bail!("Microsoft could not issue the {operation}: {detail}");
    }
    serde_json::from_str(&body)
        .with_context(|| format!("decoding the Microsoft {operation} response"))
}

async fn read_json_message<T, S>(stream: &mut S) -> Result<T>
where
    T: for<'de> Deserialize<'de>,
    S: AsyncRead + Unpin,
{
    let bytes = tokio::time::timeout(AUTH_TIMEOUT, async {
        let mut bytes = Vec::new();
        loop {
            let byte = stream.read_u8().await?;
            if byte == 0 {
                break;
            }
            if bytes.len() >= MAX_MESSAGE_SIZE {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "RDS AAD message is too large",
                ));
            }
            bytes.push(byte);
        }
        Ok::<_, std::io::Error>(bytes)
    })
    .await
    .context("timed out waiting for the RDS AAD server")??;
    serde_json::from_slice(&bytes).context("server returned invalid RDS AAD JSON")
}

async fn write_json_message<S>(stream: &mut S, value: &serde_json::Value) -> Result<()>
where
    S: AsyncWrite + Unpin,
{
    let mut bytes = serde_json::to_vec(value)?;
    bytes.push(0);
    tokio::time::timeout(AUTH_TIMEOUT, stream.write_all(&bytes))
        .await
        .context("timed out sending RDS AAD authentication")??;
    stream.flush().await?;
    Ok(())
}

fn auth_error(code: u32) -> String {
    let label = match code {
        0x8009_0308 => "invalid token",
        0x8007_0005 => "access denied",
        0xC000_006D => "logon failed",
        0xC000_005E => "no logon servers available",
        0xC000_006F => "invalid logon hours",
        0xC000_0070 => "invalid workstation",
        0xC000_0071 => "password expired",
        0xC000_0072 => "account disabled",
        _ => "unknown server error",
    };
    format!("{label}, 0x{code:08X}")
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    expires_in: Option<u64>,
}

#[derive(Deserialize)]
struct NonceResponse {
    #[serde(rename = "Nonce")]
    nonce: String,
}

#[derive(Deserialize)]
struct OAuthError {
    error: Option<String>,
    error_description: Option<String>,
}

#[derive(Deserialize)]
struct ServerNonce {
    ts_nonce: String,
}

#[derive(Deserialize)]
struct AuthenticationResult {
    authentication_result: i64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oauth_redirect_is_state_checked() {
        let url = format!("{REDIRECT_URI}?code=abc&state=expected");
        assert_eq!(authorization_code(&url, "expected").unwrap(), "abc");
        assert!(authorization_code(&url, "different").is_err());
    }

    #[test]
    fn client_uses_short_device_hostname() {
        let client = RdsAadClient::new("workstation.example.com").unwrap();
        assert!(client.resource.ends_with("/name/workstation"));
        assert!(client
            .scope
            .contains("/name/workstation/user_impersonation"));
    }

    #[test]
    fn assertion_contains_the_bound_token_and_has_a_valid_signature() {
        use aws_lc_rs::signature::{UnparsedPublicKey, RSA_PKCS1_2048_8192_SHA256};

        let mut client = RdsAadClient::new("workstation.example.com").unwrap();
        client.access_token = Some(Zeroizing::new("access-token".to_string()));
        let assertion = client
            .create_assertion("server-nonce", "aad-nonce")
            .unwrap();
        let parts: Vec<_> = assertion.split('.').collect();
        assert_eq!(parts.len(), 3);

        let payload: serde_json::Value =
            serde_json::from_slice(&URL_SAFE_NO_PAD.decode(parts[1]).expect("base64url payload"))
                .unwrap();
        assert_eq!(payload["at"], "access-token");
        assert_eq!(payload["nonce"], "server-nonce");
        assert_eq!(payload["client_claims"], r#"{"aad_nonce":"aad-nonce"}"#);

        let signing_input = format!("{}.{}", parts[0], parts[1]);
        let signature = URL_SAFE_NO_PAD
            .decode(parts[2])
            .expect("base64url signature");
        UnparsedPublicKey::new(
            &RSA_PKCS1_2048_8192_SHA256,
            client.key.public_key().as_ref(),
        )
        .verify(signing_input.as_bytes(), &signature)
        .expect("valid assertion signature");
    }
}
