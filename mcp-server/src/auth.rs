//! Who is calling a tool.
//!
//! Agents connect to this server as the signed-in user: their requests carry
//! the user's access token (`Authorization: Bearer`), which agentgateway has
//! already checked and passes on. The server checks it again (signature,
//! issuer, audience, expiry) rather than trusting the gateway alone, and puts
//! the `Caller` in the request, where tools find it.
//!
//! Set `OIDC_ISSUER` (and `OIDC_AUDIENCE`, default `agent-platform`) to
//! require a token on every request. Without it the server is open and tools
//! see no caller, as for local runs.

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, anyhow, bail};
use axum::extract::{Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use jsonwebtoken::jwk::JwkSet;
use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode, decode_header};
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

const KEY_REFRESH: Duration = Duration::from_secs(30);

/// The user a tool call is made for, from a checked token.
#[derive(Clone, Debug, Serialize)]
pub struct Caller {
    pub sub: String,
    pub username: String,
    pub name: Option<String>,
    pub email: Option<String>,
    pub groups: Vec<String>,
    /// When the token stops being valid, as a Unix time.
    pub expires_at: u64,
    /// Which client the user signed in with, e.g. `agent-console`.
    pub client: Option<String>,
}

#[derive(Deserialize)]
struct Claims {
    sub: String,
    exp: u64,
    preferred_username: Option<String>,
    name: Option<String>,
    email: Option<String>,
    #[serde(default)]
    groups: Vec<String>,
    azp: Option<String>,
}

pub struct Verifier {
    issuer: String,
    audience: String,
    http: reqwest::Client,
    keys: RwLock<(JwkSet, Option<Instant>)>,
}

impl Verifier {
    pub fn from_env() -> Option<Arc<Self>> {
        let issuer = std::env::var("OIDC_ISSUER").ok().filter(|issuer| !issuer.is_empty())?;
        Some(Arc::new(Self {
            issuer: issuer.trim_end_matches('/').to_owned(),
            audience: std::env::var("OIDC_AUDIENCE").unwrap_or_else(|_| "agent-platform".to_owned()),
            http: reqwest::Client::new(),
            keys: RwLock::new((JwkSet { keys: Vec::new() }, None)),
        }))
    }

    pub fn issuer(&self) -> &str {
        &self.issuer
    }

    async fn verify(&self, token: &str) -> anyhow::Result<Caller> {
        let header = decode_header(token).context("not a JWT")?;
        if header.alg != Algorithm::RS256 {
            bail!("unexpected algorithm {:?}", header.alg);
        }
        let key = self.key(&header.kid.context("the token names no key")?).await?;
        let mut validation = Validation::new(Algorithm::RS256);
        validation.set_issuer(&[&self.issuer]);
        validation.set_audience(&[&self.audience]);
        validation.set_required_spec_claims(&["exp", "iss", "aud", "sub"]);
        let claims = decode::<Claims>(token, &key, &validation)?.claims;
        Ok(Caller {
            username: claims.preferred_username.unwrap_or_else(|| claims.sub.clone()),
            sub: claims.sub,
            name: claims.name,
            email: claims.email,
            groups: claims.groups,
            expires_at: claims.exp,
            client: claims.azp,
        })
    }

    async fn key(&self, kid: &str) -> anyhow::Result<DecodingKey> {
        {
            let keys = self.keys.read().await;
            if let Some(jwk) = keys.0.find(kid) {
                return Ok(DecodingKey::from_jwk(jwk)?);
            }
            if keys.1.is_some_and(|at| at.elapsed() < KEY_REFRESH) {
                bail!("unknown key {kid}");
            }
        }
        #[derive(Deserialize)]
        struct Discovery {
            jwks_uri: String,
        }
        let mut keys = self.keys.write().await;
        let discovery: Discovery = self
            .http
            .get(format!("{}/.well-known/openid-configuration", self.issuer))
            .timeout(Duration::from_secs(5))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        keys.0 = self.http.get(&discovery.jwks_uri).timeout(Duration::from_secs(5)).send().await?.error_for_status()?.json().await?;
        keys.1 = Some(Instant::now());
        let jwk = keys.0.find(kid).ok_or_else(|| anyhow!("unknown key {kid}"))?;
        Ok(DecodingKey::from_jwk(jwk)?)
    }
}

/// Requires a valid user token when a verifier is configured, and adds the `Caller`.
pub async fn require_caller(State(verifier): State<Option<Arc<Verifier>>>, mut request: Request, next: Next) -> Response {
    let Some(verifier) = verifier else {
        return next.run(request).await;
    };
    let token = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "));
    let Some(token) = token else {
        return (StatusCode::UNAUTHORIZED, "a user token is required").into_response();
    };
    match verifier.verify(token).await {
        Ok(caller) => {
            request.extensions_mut().insert(caller);
            next.run(request).await
        }
        Err(error) => {
            tracing::warn!("rejected a token: {error:#}");
            (StatusCode::UNAUTHORIZED, "invalid or expired user token").into_response()
        }
    }
}
