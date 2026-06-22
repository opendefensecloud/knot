//! Thin wrapper around `openidconnect-rs` for the PKCE authorization-code
//! flow against a single configured IdP. Handles discovery, authorize-URL
//! generation, code exchange, and id_token verification.

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use openidconnect::{
    AuthorizationCode, ClientId, ClientSecret, CsrfToken, EndpointMaybeSet, EndpointNotSet,
    EndpointSet, IssuerUrl, Nonce, PkceCodeChallenge, PkceCodeVerifier, RedirectUrl, Scope,
    TokenResponse,
    core::{CoreAuthenticationFlow, CoreClient, CoreProviderMetadata},
    reqwest,
};
use thiserror::Error;
use url::Url;

#[derive(Debug, Error)]
pub enum OidcError {
    #[error("discovery: {0}")]
    Discovery(String),
    #[error("exchange: {0}")]
    Exchange(String),
    #[error("verification: {0}")]
    Verification(String),
    #[error("config: {0}")]
    Config(String),
}

/// Concrete `CoreClient` state after `from_provider_metadata` +
/// `set_redirect_uri`: auth URL is set (from discovery), token + userinfo
/// URLs are "maybe set" (the IdP may or may not advertise them via
/// discovery), and the rest remain unset.
type ConfiguredCoreClient = CoreClient<
    EndpointSet,
    EndpointNotSet,
    EndpointNotSet,
    EndpointNotSet,
    EndpointMaybeSet,
    EndpointMaybeSet,
>;

pub struct OidcClient {
    client: ConfiguredCoreClient,
    issuer: String,
    /// Extra `aud` values to trust in the ID token beyond the client id (see
    /// `audience_is_trusted`). Empty for IdPs that only ever set the client id.
    extra_audiences: Vec<String>,
}

impl OidcClient {
    pub async fn discover(
        issuer: &str,
        client_id: &str,
        client_secret: &str,
        redirect_url: &str,
        extra_audiences: Vec<String>,
    ) -> Result<Self, OidcError> {
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| OidcError::Discovery(e.to_string()))?;
        let issuer_url =
            IssuerUrl::new(issuer.to_string()).map_err(|e| OidcError::Config(e.to_string()))?;
        let metadata = CoreProviderMetadata::discover_async(issuer_url, &http)
            .await
            .map_err(|e| OidcError::Discovery(e.to_string()))?;
        let client = CoreClient::from_provider_metadata(
            metadata,
            ClientId::new(client_id.to_string()),
            Some(ClientSecret::new(client_secret.to_string())),
        )
        .set_redirect_uri(
            RedirectUrl::new(redirect_url.to_string())
                .map_err(|e| OidcError::Config(e.to_string()))?,
        );
        Ok(Self {
            client,
            issuer: issuer.to_string(),
            extra_audiences,
        })
    }

    pub fn issuer(&self) -> &str {
        &self.issuer
    }

    pub fn build_authorize_url(&self) -> FlowStart {
        let (pkce_challenge, pkce_verifier) = PkceCodeChallenge::new_random_sha256();
        let (auth_url, csrf, nonce) = self
            .client
            .authorize_url(
                CoreAuthenticationFlow::AuthorizationCode,
                CsrfToken::new_random,
                Nonce::new_random,
            )
            .add_scope(Scope::new("openid".into()))
            .add_scope(Scope::new("email".into()))
            .add_scope(Scope::new("profile".into()))
            .add_scope(Scope::new("groups".into()))
            .set_pkce_challenge(pkce_challenge)
            .url();
        FlowStart {
            authorize_url: auth_url,
            csrf_state: csrf.secret().to_string(),
            nonce: nonce.secret().to_string(),
            pkce_verifier: pkce_verifier.secret().to_string(),
        }
    }

    pub async fn exchange_code(
        &self,
        code: &str,
        pkce_verifier: &str,
        expected_nonce: &str,
    ) -> Result<VerifiedIdentity, OidcError> {
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| OidcError::Exchange(e.to_string()))?;

        let response = self
            .client
            .exchange_code(AuthorizationCode::new(code.to_string()))
            .map_err(|e| OidcError::Exchange(e.to_string()))?
            .set_pkce_verifier(PkceCodeVerifier::new(pkce_verifier.to_string()))
            .request_async(&http)
            .await
            .map_err(|e| OidcError::Exchange(e.to_string()))?;

        let id_token = response
            .id_token()
            .ok_or_else(|| OidcError::Verification("missing id_token".into()))?;
        let nonce = Nonce::new(expected_nonce.to_string());
        // Some IdPs put extra `aud` values in the ID token alongside our client
        // id — Zitadel, for example, adds the project id. openidconnect rejects
        // any audience other than the client id by default ("not a trusted
        // audience"), so explicitly trust the configured extras. The client id
        // is still required to be present, and the crate still enforces that
        // `azp` equals the client id whenever multiple audiences are present.
        let trusted = self.extra_audiences.clone();
        let id_verifier = self
            .client
            .id_token_verifier()
            .set_other_audience_verifier_fn(move |aud| audience_is_trusted(&trusted, aud.as_str()));
        let claims = id_token
            .claims(&id_verifier, &nonce)
            .map_err(|e| OidcError::Verification(e.to_string()))?;

        let email = claims
            .email()
            .ok_or_else(|| OidcError::Verification("missing email claim".into()))?
            .to_string();
        let display_name = claims
            .name()
            .and_then(|n| n.get(None))
            .map(|n| n.to_string())
            .unwrap_or_else(|| email.clone());

        // Option A: decode the id_token JWT payload to pluck `groups` from
        // the raw JSON. The signature has already been verified above; this
        // is just decoding the payload portion of a JWS we trust.
        let raw = id_token.to_string();
        let groups = extract_groups_from_jwt(&raw).unwrap_or_default();

        // Whether the IdP asserts the email is verified. Domain-based
        // auto-provisioning must not trust an unverified email (an attacker
        // could claim any address in a trusted domain otherwise).
        let email_verified = claims.email_verified().unwrap_or(false);

        Ok(VerifiedIdentity {
            subject: claims.subject().to_string(),
            email,
            email_verified,
            display_name,
            groups,
        })
    }
}

pub struct FlowStart {
    pub authorize_url: Url,
    pub csrf_state: String,
    pub nonce: String,
    pub pkce_verifier: String,
}

#[derive(Debug, Clone)]
pub struct VerifiedIdentity {
    pub subject: String,
    pub email: String,
    /// IdP's `email_verified` claim (false if absent).
    pub email_verified: bool,
    pub display_name: String,
    pub groups: Vec<String>,
}

fn extract_groups_from_jwt(jwt: &str) -> Option<Vec<String>> {
    let mut parts = jwt.split('.');
    let _header = parts.next()?;
    let payload_b64 = parts.next()?;
    let payload_bytes = URL_SAFE_NO_PAD.decode(payload_b64).ok()?;
    let v: serde_json::Value = serde_json::from_slice(&payload_bytes).ok()?;
    let arr = v.get("groups")?.as_array()?;
    Some(
        arr.iter()
            .filter_map(|g| g.as_str().map(str::to_string))
            .collect(),
    )
}

/// Whether `aud` is an audience we explicitly trust beyond the client id.
/// Exact string match against the configured extra-audience list; an empty
/// list trusts nothing extra (the openidconnect default).
fn audience_is_trusted(trusted: &[String], aud: &str) -> bool {
    trusted.iter().any(|a| a == aud)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flow_start_url_has_pkce_and_state_shape() {
        // We can't easily build a real `OidcClient` without discovery, so we
        // smoke-test the PKCE primitive shape — that the verifier+challenge
        // are well-formed and at least 43 chars (RFC 7636 minimum).
        let (challenge, verifier) = PkceCodeChallenge::new_random_sha256();
        assert!(challenge.as_str().len() >= 43);
        assert!(verifier.secret().len() >= 43);
    }

    #[test]
    fn audience_trust_requires_explicit_match() {
        // With no configured extra audiences nothing beyond the client_id is
        // trusted — mirrors openidconnect's default-deny for other audiences.
        assert!(!audience_is_trusted(&[], "366700366412350659"));
        // An exact configured match is trusted (e.g. the Zitadel project id).
        let trusted = vec!["366700366412350659".to_string()];
        assert!(audience_is_trusted(&trusted, "366700366412350659"));
        // A non-listed audience is not trusted.
        assert!(!audience_is_trusted(&trusted, "999999999999999999"));
        // Matching is exact: neither prefixes nor truncations count.
        assert!(!audience_is_trusted(&["3667".to_string()], "366700366412350659"));
        assert!(!audience_is_trusted(&trusted, "36670036641235065"));
        // Any entry in the list matches.
        let many = vec!["a".to_string(), "366700366412350659".to_string()];
        assert!(audience_is_trusted(&many, "366700366412350659"));
    }

    #[test]
    fn extract_groups_pulls_known_payload() {
        // A real-looking JWT (signature-free for the test). We only decode
        // the payload portion to pluck `groups`.
        let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"none","typ":"JWT"}"#);
        let payload = URL_SAFE_NO_PAD.encode(br#"{"sub":"abc","email":"a@b","groups":["x","y"]}"#);
        let jwt = format!("{header}.{payload}.fake-sig");
        let groups = extract_groups_from_jwt(&jwt).expect("groups");
        assert_eq!(groups, vec!["x".to_string(), "y".to_string()]);
    }

    #[test]
    fn extract_groups_returns_none_when_absent() {
        let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"none","typ":"JWT"}"#);
        let payload = URL_SAFE_NO_PAD.encode(br#"{"sub":"abc","email":"a@b"}"#);
        let jwt = format!("{header}.{payload}.fake-sig");
        assert!(extract_groups_from_jwt(&jwt).is_none());
    }

    #[test]
    fn extract_groups_handles_malformed_jwt() {
        assert!(extract_groups_from_jwt("").is_none());
        assert!(extract_groups_from_jwt("not-a-jwt").is_none());
        assert!(extract_groups_from_jwt("only.two").is_none());
        assert!(extract_groups_from_jwt("bad.@@@@.sig").is_none());
        // Valid base64 but not JSON.
        let header = URL_SAFE_NO_PAD.encode(b"hdr");
        let payload = URL_SAFE_NO_PAD.encode(b"not-json");
        let jwt = format!("{header}.{payload}.sig");
        assert!(extract_groups_from_jwt(&jwt).is_none());
        // Valid JSON but groups is not an array.
        let payload = URL_SAFE_NO_PAD.encode(br#"{"groups":"not-array"}"#);
        let jwt = format!("{header}.{payload}.sig");
        assert!(extract_groups_from_jwt(&jwt).is_none());
    }
}
