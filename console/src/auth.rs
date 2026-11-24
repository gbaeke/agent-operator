//! Who is using the console.
//!
//! oauth2-proxy, a sidecar in front of the console, signs users in with
//! OIDC (Keycloak) and passes each request on with the user's access token
//! in `X-Forwarded-Access-Token`. The console does not take that header on
//! trust: it checks the token's signature against the issuer's keys, its
//! issuer, audience and expiry, and only then knows the user.
//!
//! Set `OIDC_ISSUER` (and `OIDC_AUDIENCE`, default `agent-platform`) to turn
//! this on. Without it the console is open, as for local runs.

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, anyhow, bail};
use axum::Json;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use jsonwebtoken::jwk::JwkSet;
use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode, decode_header};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::sync::RwLock;

/// Refetch the issuer's keys at most this often when a token names an unknown key.
const KEY_REFRESH: Duration = Duration::from_secs(30);

pub struct Auth {
    issuer: String,
    audience: String,
    http: reqwest::Client,
    keys: RwLock<Keys>,
}

#[derive(Default)]
struct Keys {
    set: JwkSet,
    fetched: Option<Instant>,
}

/// The signed-in user, from a checked token.
#[derive(Clone, Debug, Serialize)]
pub struct User {
    pub sub: String,
    pub username: String,
    pub name: Option<String>,
    pub email: Option<String>,
    pub groups: Vec<String>,
    /// The access token itself, to pass on. Never serialized or logged.
    #[serde(skip)]
    #[allow(dead_code)]
    pub token: String,
}

#[derive(Deserialize)]
struct Claims {
    sub: String,
    preferred_username: Option<String>,
    name: Option<String>,
    email: Option<String>,
    #[serde(default)]
    groups: Vec<String>,
}

impl Auth {
    /// Sign-in as the environment asks for it: `None` when `OIDC_ISSUER` is unset.
    pub fn from_env(http: reqwest::Client) -> Option<Arc<Self>> {
        let issuer = std::env::var("OIDC_ISSUER")
            .ok()
            .filter(|issuer| !issuer.is_empty())?;
        let audience =
            std::env::var("OIDC_AUDIENCE").unwrap_or_else(|_| "agent-platform".to_owned());
        Some(Arc::new(Self {
            issuer: issuer.trim_end_matches('/').to_owned(),
            audience,
            http,
            keys: RwLock::default(),
        }))
    }

    pub fn issuer(&self) -> &str {
        &self.issuer
    }

    /// The user a token belongs to, if the token is genuine and current.
    pub async fn verify(&self, token: &str) -> anyhow::Result<User> {
        let header = decode_header(token).context("not a JWT")?;
        if header.alg != Algorithm::RS256 {
            bail!("unexpected algorithm {:?}", header.alg);
        }
        let kid = header.kid.context("the token names no key")?;
        let key = self.key(&kid).await?;
        let mut validation = Validation::new(Algorithm::RS256);
        validation.set_issuer(&[&self.issuer]);
        validation.set_audience(&[&self.audience]);
        validation.set_required_spec_claims(&["exp", "iss", "aud", "sub"]);
        let claims = decode::<Claims>(token, &key, &validation)?.claims;
        Ok(User {
            username: claims
                .preferred_username
                .unwrap_or_else(|| claims.sub.clone()),
            sub: claims.sub,
            name: claims.name,
            email: claims.email,
            groups: claims.groups,
            token: token.to_owned(),
        })
    }

    /// The issuer's key with this id, fetched again if it is new (keys rotate).
    async fn key(&self, kid: &str) -> anyhow::Result<DecodingKey> {
        {
            let keys = self.keys.read().await;
            if let Some(jwk) = keys.set.find(kid) {
                return Ok(DecodingKey::from_jwk(jwk)?);
            }
            if keys.fetched.is_some_and(|at| at.elapsed() < KEY_REFRESH) {
                bail!("unknown key {kid}");
            }
        }
        let mut keys = self.keys.write().await;
        keys.set = self.fetch_keys().await?;
        keys.fetched = Some(Instant::now());
        let jwk = keys
            .set
            .find(kid)
            .ok_or_else(|| anyhow!("unknown key {kid}"))?;
        Ok(DecodingKey::from_jwk(jwk)?)
    }

    async fn fetch_keys(&self) -> anyhow::Result<JwkSet> {
        #[derive(Deserialize)]
        struct Discovery {
            jwks_uri: String,
        }
        let discovery: Discovery = self
            .http
            .get(format!("{}/.well-known/openid-configuration", self.issuer))
            .timeout(Duration::from_secs(5))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await
            .context("the issuer's OpenID configuration")?;
        let keys = self
            .http
            .get(&discovery.jwks_uri)
            .timeout(Duration::from_secs(5))
            .send()
            .await?
            .error_for_status()?;
        keys.json().await.context("the issuer's keys")
    }
}

/// The token oauth2-proxy passes on, or a bearer token sent directly.
fn token(headers: &HeaderMap) -> Option<&str> {
    if let Some(token) = headers
        .get("x-forwarded-access-token")
        .and_then(|value| value.to_str().ok())
    {
        return Some(token);
    }
    headers
        .get(header::AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
}

/// Lets a request through only with a valid token, and adds its `User`.
/// Open (no sign-in configured), every request goes through without one.
pub async fn require_user(
    State(auth): State<Option<Arc<Auth>>>,
    mut request: Request,
    next: Next,
) -> Response {
    let Some(auth) = auth else {
        return next.run(request).await;
    };
    let Some(token) = token(request.headers()) else {
        return unauthorized("not signed in");
    };
    match auth.verify(token).await {
        Ok(user) => {
            request.extensions_mut().insert(user);
            next.run(request).await
        }
        Err(error) => {
            tracing::warn!("rejected a token: {error:#}");
            unauthorized("sign-in expired or invalid")
        }
    }
}

fn unauthorized(message: &str) -> Response {
    (StatusCode::UNAUTHORIZED, Json(json!({ "error": message }))).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    #[test]
    fn finds_the_token() {
        let mut headers = HeaderMap::new();
        assert_eq!(token(&headers), None);
        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("Bearer abc"),
        );
        assert_eq!(token(&headers), Some("abc"));
        headers.insert("x-forwarded-access-token", HeaderValue::from_static("xyz"));
        assert_eq!(token(&headers), Some("xyz"));
    }

    #[tokio::test]
    async fn rejects_garbage_before_fetching_keys() {
        let auth = Auth {
            issuer: "http://127.0.0.1:9/realms/x".into(),
            audience: "agent-platform".into(),
            http: reqwest::Client::new(),
            keys: RwLock::default(),
        };
        assert!(auth.verify("not-a-token").await.is_err());
    }
}
