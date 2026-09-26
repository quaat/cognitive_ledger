//! Authentication and authorization at the service boundary (ADR-0011).
//!
//! An [`Authenticator`] turns a bearer token into a [`VerifiedIdentity`]: the
//! `AuthenticatedPrincipal` recorded in commits plus the caller's ledger capabilities.
//! Every field is derived from cryptographically verified claims and explicit
//! configuration — never from request bodies or unsigned headers.
//!
//! Two implementations exist. [`OidcAuthenticator`] validates RS256/ES256 tokens against
//! a configured issuer and audience using the issuer's JWKS (fetched, cached, refreshed
//! on an unknown key id, failing closed). [`DevHs256Authenticator`] validates HS256 tokens
//! against a configured secret with the same issuer/audience/expiry rules; it is not
//! production authentication and the server refuses to bind it to a non-loopback address
//! unless a conspicuously named override is set.

use ledger_core::{AuthenticatedPrincipal, PrincipalId, PrincipalType, TenantId};
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::RwLock;

/// Ledger capabilities (authorization). Never part of commit identity.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Capability {
    /// Inspect authorized graphs, refs and states.
    Read,
    /// Prepare cognitive changes (proposals).
    Propose,
    /// Accept or reject proposals.
    Review,
    /// Administrative graph/lifecycle operations (reserved).
    Admin,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Capabilities(pub BTreeSet<Capability>);

impl Capabilities {
    pub fn has(&self, capability: Capability) -> bool {
        self.0.contains(&capability)
    }
}

#[derive(Clone, Debug)]
pub struct VerifiedIdentity {
    pub principal: AuthenticatedPrincipal,
    pub capabilities: Capabilities,
}

/// Why authentication failed. The variant is logged under the correlation id; clients see
/// only `UNAUTHENTICATED` (or `DEPENDENCY_UNAVAILABLE` when the key source is down).
#[derive(Debug)]
pub enum AuthError {
    Unauthenticated(String),
    KeySourceUnavailable(String),
}

#[async_trait::async_trait]
pub trait Authenticator: Send + Sync {
    async fn authenticate(&self, bearer: &str) -> Result<VerifiedIdentity, AuthError>;
    /// Whether this authenticator is acceptable for a non-loopback deployment.
    fn is_production_grade(&self) -> bool;
    fn describe(&self) -> String;
}

/// How verified claims map to ledger identity. Everything here is explicit configuration:
/// the ledger does not guess how an identity provider distinguishes agents from services.
#[derive(Clone, Debug)]
pub struct ClaimsPolicy {
    /// Claim holding the tenant (Entra: `tid`).
    pub tenant_claim: String,
    /// Claim holding the stable principal identifier (Entra: `oid`; generic OIDC: `sub`).
    pub principal_claim: String,
    /// Optional claim that states the principal type explicitly (`human|agent|service`).
    pub principal_type_claim: Option<String>,
    /// Client identifiers (matched against `client_id_claims`) that are AI agents.
    pub agent_client_ids: BTreeSet<String>,
    /// Client identifiers that are ordinary services.
    pub service_client_ids: BTreeSet<String>,
    /// Claims that carry the client/application id (Entra: `azp`, `appid`).
    pub client_id_claims: Vec<String>,
    /// Claim holding role names (array of strings; a single string is accepted).
    pub roles_claim: String,
    /// Role name → capability.
    pub role_map: BTreeMap<String, Capability>,
    /// Optional claim naming the human a delegated actor acts for.
    pub on_behalf_of_claim: Option<String>,
    /// Prefix applied to principal ids (`urn:sculpin:<type>:`), keeping ids opaque tokens.
    pub principal_prefix: bool,
}

impl Default for ClaimsPolicy {
    fn default() -> Self {
        Self {
            tenant_claim: "tid".into(),
            principal_claim: "oid".into(),
            principal_type_claim: Some("sculpin_principal_type".into()),
            agent_client_ids: BTreeSet::new(),
            service_client_ids: BTreeSet::new(),
            client_id_claims: vec!["azp".into(), "appid".into()],
            roles_claim: "roles".into(),
            role_map: [
                ("ledger.read", Capability::Read),
                ("ledger.propose", Capability::Propose),
                ("ledger.review", Capability::Review),
                ("ledger.admin", Capability::Admin),
            ]
            .into_iter()
            .map(|(k, v)| (k.to_owned(), v))
            .collect(),
            on_behalf_of_claim: None,
            principal_prefix: true,
        }
    }
}

fn claim_str<'a>(claims: &'a Value, name: &str) -> Option<&'a str> {
    claims
        .get(name)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
}

impl ClaimsPolicy {
    /// Derive the ledger identity from verified claims. Fails closed: a missing tenant or
    /// principal, an undeterminable principal type, or no recognised role is
    /// `Unauthenticated`.
    pub fn identity_from(&self, claims: &Value) -> Result<VerifiedIdentity, AuthError> {
        let tenant = claim_str(claims, &self.tenant_claim).ok_or_else(|| {
            AuthError::Unauthenticated(format!("missing tenant claim {}", self.tenant_claim))
        })?;
        let subject = claim_str(claims, &self.principal_claim).ok_or_else(|| {
            AuthError::Unauthenticated(format!("missing principal claim {}", self.principal_claim))
        })?;
        let principal_type = self.principal_type(claims)?;
        let principal_id = if self.principal_prefix {
            format!("urn:sculpin:{}:{subject}", principal_type.as_str())
        } else {
            subject.to_owned()
        };
        let on_behalf_of = match &self.on_behalf_of_claim {
            Some(claim) => claim_str(claims, claim)
                .map(|v| PrincipalId::new(format!("urn:sculpin:human:{v}")))
                .transpose()
                .map_err(|e| AuthError::Unauthenticated(format!("on_behalf_of: {e}")))?,
            None => None,
        };
        let capabilities = self.capabilities(claims);
        if capabilities.0.is_empty() {
            return Err(AuthError::Unauthenticated(
                "no recognised ledger role".into(),
            ));
        }
        Ok(VerifiedIdentity {
            principal: AuthenticatedPrincipal {
                principal_id: PrincipalId::new(principal_id)
                    .map_err(|e| AuthError::Unauthenticated(format!("principal id: {e}")))?,
                principal_type,
                tenant_id: TenantId::new(tenant)
                    .map_err(|e| AuthError::Unauthenticated(format!("tenant: {e}")))?,
                on_behalf_of,
            },
            capabilities,
        })
    }

    /// Principal type from the explicit claim and/or the configured client-id lists. When
    /// both apply they must agree; a disagreement is refused rather than resolved, so a
    /// claim an identity provider lets clients influence cannot override configuration.
    fn principal_type(&self, claims: &Value) -> Result<PrincipalType, AuthError> {
        let from_claim = match &self.principal_type_claim {
            Some(claim) => match claim_str(claims, claim) {
                Some("human") => Some(PrincipalType::Human),
                Some("agent") => Some(PrincipalType::Agent),
                Some("service") => Some(PrincipalType::Service),
                Some(other) => {
                    return Err(AuthError::Unauthenticated(format!(
                        "unknown principal type claim value {other:?}"
                    )));
                }
                None => None,
            },
            None => None,
        };
        let mut from_config = None;
        for claim in &self.client_id_claims {
            if let Some(client) = claim_str(claims, claim) {
                if self.agent_client_ids.contains(client) {
                    from_config = Some(PrincipalType::Agent);
                    break;
                }
                if self.service_client_ids.contains(client) {
                    from_config = Some(PrincipalType::Service);
                    break;
                }
            }
        }
        match (from_claim, from_config) {
            (Some(a), Some(b)) if a != b => Err(AuthError::Unauthenticated(
                "principal type claim disagrees with configured client identity".into(),
            )),
            (Some(t), _) | (None, Some(t)) => Ok(t),
            (None, None) => Err(AuthError::Unauthenticated(
                "principal type not determinable from verified claims or configuration".into(),
            )),
        }
    }

    fn capabilities(&self, claims: &Value) -> Capabilities {
        let mut set = BTreeSet::new();
        let roles: Vec<&str> = match claims.get(&self.roles_claim) {
            Some(Value::Array(items)) => items.iter().filter_map(Value::as_str).collect(),
            Some(Value::String(s)) => s.split(' ').filter(|s| !s.is_empty()).collect(),
            _ => Vec::new(),
        };
        for role in roles {
            if let Some(capability) = self.role_map.get(role) {
                set.insert(*capability);
            }
        }
        Capabilities(set)
    }
}

fn validation(
    issuer: &str,
    audience: &str,
    algorithms: &[jsonwebtoken::Algorithm],
) -> jsonwebtoken::Validation {
    let mut validation = jsonwebtoken::Validation::new(algorithms[0]);
    validation.algorithms = algorithms.to_vec();
    validation.set_issuer(&[issuer]);
    validation.set_audience(&[audience]);
    validation.validate_exp = true;
    validation.validate_nbf = true;
    validation.leeway = 30;
    validation.required_spec_claims = ["exp", "iss", "aud"]
        .into_iter()
        .map(str::to_owned)
        .collect();
    validation
}

/// Bearer tokens validated against an OIDC issuer's JWKS.
///
/// Key handling: keys are cached by `kid`; a refresh happens for an unknown `kid` or when
/// the cache is older than `max_key_age` (so a withdrawn key stops validating without a
/// restart), at most once per `min_refresh_interval` (also after a failed fetch, so an
/// unavailable issuer is not hammered), and under a single-flight lock so concurrent
/// requests share one fetch. Only keys with `use = sig` (or no `use`) and a `kid` are
/// loaded; the token's `alg` must match the key family. An unknown key fails closed; an
/// unreachable key source is `KeySourceUnavailable` (503), never a bypass.
pub struct OidcAuthenticator {
    issuer: String,
    audience: String,
    jwks_url: String,
    http: reqwest::Client,
    keys: RwLock<KeyCache>,
    refresh: tokio::sync::Mutex<()>,
    min_refresh_interval: Duration,
    max_key_age: Duration,
    policy: ClaimsPolicy,
}

/// Largest JWKS document accepted (a real issuer's set is a few kilobytes).
const MAX_JWKS_BYTES: usize = 256 * 1024;

struct KeyCache {
    /// When keys were last successfully loaded.
    loaded_at: Option<Instant>,
    /// When a fetch was last attempted (success or failure); bounds retry rate.
    attempted_at: Option<Instant>,
    by_kid: HashMap<String, (jsonwebtoken::DecodingKey, jsonwebtoken::Algorithm)>,
}

impl OidcAuthenticator {
    pub fn new(issuer: String, audience: String, jwks_url: String, policy: ClaimsPolicy) -> Self {
        Self {
            issuer,
            audience,
            jwks_url,
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(10))
                // The configured URL is the only endpoint trusted for keys: no redirects
                // (an https URL must not be able to bounce to http or elsewhere).
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .expect("static client configuration"),
            keys: RwLock::new(KeyCache {
                loaded_at: None,
                attempted_at: None,
                by_kid: HashMap::new(),
            }),
            refresh: tokio::sync::Mutex::new(()),
            min_refresh_interval: Duration::from_secs(60),
            max_key_age: Duration::from_secs(60 * 60),
            policy,
        }
    }

    /// Tune the refresh policy (tests use a zero interval; deployments rarely need this).
    pub fn with_refresh_policy(
        mut self,
        min_refresh_interval: Duration,
        max_key_age: Duration,
    ) -> Self {
        self.min_refresh_interval = min_refresh_interval;
        self.max_key_age = max_key_age;
        self
    }

    async fn fetch_jwks(&self) -> Result<jsonwebtoken::jwk::JwkSet, AuthError> {
        // Errors never echo the URL (it may carry a tenant id): the message says what kind
        // of failure occurred, the log carries the correlation id.
        let response = self.http.get(&self.jwks_url).send().await.map_err(|e| {
            AuthError::KeySourceUnavailable(format!("jwks fetch failed: {}", redact(&e)))
        })?;
        if !response.status().is_success() {
            return Err(AuthError::KeySourceUnavailable(format!(
                "jwks fetch returned HTTP {}",
                response.status().as_u16()
            )));
        }
        if response
            .content_length()
            .is_some_and(|n| n > MAX_JWKS_BYTES as u64)
        {
            return Err(AuthError::KeySourceUnavailable(
                "jwks document too large".into(),
            ));
        }
        let bytes = response.bytes().await.map_err(|e| {
            AuthError::KeySourceUnavailable(format!("jwks read failed: {}", redact(&e)))
        })?;
        if bytes.len() > MAX_JWKS_BYTES {
            return Err(AuthError::KeySourceUnavailable(
                "jwks document too large".into(),
            ));
        }
        serde_json::from_slice(&bytes)
            .map_err(|e| AuthError::KeySourceUnavailable(format!("jwks decode failed: {e}")))
    }

    /// Refresh under the single-flight lock, honouring the minimum interval between
    /// attempts. Returns whether a fetch was performed.
    async fn refresh_keys(&self) -> Result<bool, AuthError> {
        let _flight = self.refresh.lock().await;
        let throttled = self
            .keys
            .read()
            .await
            .attempted_at
            .is_some_and(|t| t.elapsed() < self.min_refresh_interval);
        if throttled {
            return Ok(false);
        }
        self.keys.write().await.attempted_at = Some(Instant::now());
        let set = self.fetch_jwks().await?;
        let mut by_kid = HashMap::new();
        for jwk in set.keys {
            let Some(kid) = jwk.common.key_id.clone() else {
                continue;
            };
            if jwk
                .common
                .public_key_use
                .as_ref()
                .is_some_and(|u| *u != jsonwebtoken::jwk::PublicKeyUse::Signature)
            {
                continue;
            }
            let Ok(key) = jsonwebtoken::DecodingKey::from_jwk(&jwk) else {
                continue;
            };
            let algorithm = match &jwk.algorithm {
                jsonwebtoken::jwk::AlgorithmParameters::RSA(_) => jsonwebtoken::Algorithm::RS256,
                jsonwebtoken::jwk::AlgorithmParameters::EllipticCurve(ec)
                    if ec.curve == jsonwebtoken::jwk::EllipticCurve::P256 =>
                {
                    jsonwebtoken::Algorithm::ES256
                }
                _ => continue,
            };
            by_kid.insert(kid, (key, algorithm));
        }
        let mut cache = self.keys.write().await;
        cache.by_kid = by_kid;
        cache.loaded_at = Some(Instant::now());
        Ok(true)
    }

    async fn key_for(
        &self,
        kid: &str,
    ) -> Result<(jsonwebtoken::DecodingKey, jsonwebtoken::Algorithm), AuthError> {
        {
            let cache = self.keys.read().await;
            if let Some(found) = cache.by_kid.get(kid)
                && cache
                    .loaded_at
                    .is_some_and(|t| t.elapsed() < self.max_key_age)
            {
                return Ok(found.clone());
            }
        }
        // Unknown or aged key: one shared, rate-limited refresh, then fail closed. An aged
        // key whose source cannot be reached is not trusted on the strength of its age.
        self.refresh_keys().await?;
        if let Some(found) = self.keys.read().await.by_kid.get(kid) {
            return Ok(found.clone());
        }
        Err(AuthError::Unauthenticated(format!(
            "unknown signing key id {kid:?}"
        )))
    }
}

/// reqwest errors embed the URL; keep only the kind of failure.
fn redact(e: &reqwest::Error) -> &'static str {
    if e.is_timeout() {
        "timeout"
    } else if e.is_connect() {
        "connection failed"
    } else if e.is_body() || e.is_decode() {
        "body error"
    } else {
        "request error"
    }
}

#[async_trait::async_trait]
impl Authenticator for OidcAuthenticator {
    async fn authenticate(&self, bearer: &str) -> Result<VerifiedIdentity, AuthError> {
        let header = jsonwebtoken::decode_header(bearer)
            .map_err(|e| AuthError::Unauthenticated(format!("malformed token header: {e}")))?;
        let kid = header
            .kid
            .ok_or_else(|| AuthError::Unauthenticated("token has no key id".into()))?;
        let (key, algorithm) = self.key_for(&kid).await?;
        if header.alg != algorithm {
            return Err(AuthError::Unauthenticated(
                "token algorithm does not match its key".into(),
            ));
        }
        let data = jsonwebtoken::decode::<Value>(
            bearer,
            &key,
            &validation(&self.issuer, &self.audience, &[algorithm]),
        )
        .map_err(|e| AuthError::Unauthenticated(format!("token rejected: {e}")))?;
        self.policy.identity_from(&data.claims)
    }

    fn is_production_grade(&self) -> bool {
        true
    }

    fn describe(&self) -> String {
        format!("oidc issuer={} audience={}", self.issuer, self.audience)
    }
}

/// HS256 tokens against a configured shared secret. Development and CI only.
pub struct DevHs256Authenticator {
    issuer: String,
    audience: String,
    key: jsonwebtoken::DecodingKey,
    policy: ClaimsPolicy,
}

impl DevHs256Authenticator {
    pub fn new(
        issuer: String,
        audience: String,
        secret: &[u8],
        policy: ClaimsPolicy,
    ) -> Result<Self, String> {
        if secret.len() < 32 {
            return Err("development HS256 secret must be at least 32 bytes".into());
        }
        Ok(Self {
            issuer,
            audience,
            key: jsonwebtoken::DecodingKey::from_secret(secret),
            policy,
        })
    }
}

#[async_trait::async_trait]
impl Authenticator for DevHs256Authenticator {
    async fn authenticate(&self, bearer: &str) -> Result<VerifiedIdentity, AuthError> {
        let data = jsonwebtoken::decode::<Value>(
            bearer,
            &self.key,
            &validation(
                &self.issuer,
                &self.audience,
                &[jsonwebtoken::Algorithm::HS256],
            ),
        )
        .map_err(|e| AuthError::Unauthenticated(format!("token rejected: {e}")))?;
        self.policy.identity_from(&data.claims)
    }

    fn is_production_grade(&self) -> bool {
        false
    }

    fn describe(&self) -> String {
        format!(
            "DEVELOPMENT HS256 (not production authentication) issuer={} audience={}",
            self.issuer, self.audience
        )
    }
}

/// Shared handle used by the router.
pub type SharedAuthenticator = Arc<dyn Authenticator>;

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::Arc;

    const ISS: &str = "https://issuer.test/";
    const AUD: &str = "api://ledger";
    const RSA_A: &str = include_str!("../tests/fixtures/test-rsa-a.pkcs8");
    const EC_B: &str = include_str!("../tests/fixtures/test-ec-b.pkcs8");
    const RSA_C: &str = include_str!("../tests/fixtures/test-rsa-c.pkcs8");
    const JWKS_INITIAL: &str = include_str!("../tests/fixtures/jwks-initial.json");
    const JWKS_ROTATED: &str = include_str!("../tests/fixtures/jwks-rotated.json");

    fn now() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
    }

    fn claims() -> Value {
        json!({"iss": ISS, "aud": AUD, "exp": now() + 300, "tid": "t1", "oid": "u1",
               "sculpin_principal_type": "human", "roles": ["ledger.read"]})
    }

    fn sign(
        alg: jsonwebtoken::Algorithm,
        kid: &str,
        key: &jsonwebtoken::EncodingKey,
        c: &Value,
    ) -> String {
        let mut header = jsonwebtoken::Header::new(alg);
        header.kid = Some(kid.to_owned());
        jsonwebtoken::encode(&header, c, key).unwrap()
    }

    fn rsa(pem: &str) -> jsonwebtoken::EncodingKey {
        jsonwebtoken::EncodingKey::from_rsa_pem(pem.as_bytes()).unwrap()
    }

    /// A local JWKS endpoint whose document can be swapped, plus a request counter.
    struct Jwks {
        url: String,
        document: Arc<std::sync::Mutex<String>>,
        hits: Arc<std::sync::atomic::AtomicUsize>,
    }

    async fn jwks_server(initial: &str) -> Jwks {
        let document = Arc::new(std::sync::Mutex::new(initial.to_owned()));
        let hits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (d, h) = (document.clone(), hits.clone());
        let app = axum::Router::new().route(
            "/keys",
            axum::routing::get(move || {
                let d = d.clone();
                let h = h.clone();
                async move {
                    h.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    (
                        [(axum::http::header::CONTENT_TYPE, "application/json")],
                        d.lock().unwrap().clone(),
                    )
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/keys", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Jwks {
            url,
            document,
            hits,
        }
    }

    fn oidc(url: &str) -> OidcAuthenticator {
        OidcAuthenticator::new(
            ISS.into(),
            AUD.into(),
            url.to_owned(),
            ClaimsPolicy::default(),
        )
        .with_refresh_policy(Duration::ZERO, Duration::from_secs(3600))
    }

    #[tokio::test]
    async fn oidc_accepts_rsa_and_ec_keys_from_the_jwks_and_maps_identity() {
        let server = jwks_server(JWKS_INITIAL).await;
        let auth = oidc(&server.url);
        let rs = sign(
            jsonwebtoken::Algorithm::RS256,
            "kid-a",
            &rsa(RSA_A),
            &claims(),
        );
        let id = auth.authenticate(&rs).await.unwrap();
        assert_eq!(id.principal.principal_id.as_str(), "urn:sculpin:human:u1");
        assert_eq!(id.principal.tenant_id.as_str(), "t1");
        assert!(id.capabilities.has(Capability::Read));
        assert!(!id.capabilities.has(Capability::Propose));
        let es = sign(
            jsonwebtoken::Algorithm::ES256,
            "kid-b",
            &jsonwebtoken::EncodingKey::from_ec_pem(EC_B.as_bytes()).unwrap(),
            &claims(),
        );
        auth.authenticate(&es).await.unwrap();
        assert!(auth.is_production_grade());
    }

    #[tokio::test]
    async fn oidc_unknown_kid_fails_closed_then_rotation_is_picked_up() {
        let server = jwks_server(JWKS_INITIAL).await;
        let auth = oidc(&server.url);
        let c = sign(
            jsonwebtoken::Algorithm::RS256,
            "kid-c",
            &rsa(RSA_C),
            &claims(),
        );
        assert!(matches!(
            auth.authenticate(&c).await,
            Err(AuthError::Unauthenticated(m)) if m.contains("unknown signing key")
        ));
        *server.document.lock().unwrap() = JWKS_ROTATED.to_owned();
        auth.authenticate(&c).await.unwrap();
        // The encryption-use key in the rotated set is never loaded as a signing key.
        let enc = sign(
            jsonwebtoken::Algorithm::RS256,
            "kid-enc",
            &rsa(RSA_C),
            &claims(),
        );
        assert!(matches!(
            auth.authenticate(&enc).await,
            Err(AuthError::Unauthenticated(_))
        ));
    }

    #[tokio::test]
    async fn oidc_refresh_is_rate_limited_and_aged_keys_are_refetched() {
        let server = jwks_server(JWKS_INITIAL).await;
        // One-hour minimum interval: repeated unknown kids cause exactly one fetch.
        let auth = OidcAuthenticator::new(
            ISS.into(),
            AUD.into(),
            server.url.clone(),
            ClaimsPolicy::default(),
        )
        .with_refresh_policy(Duration::from_secs(3600), Duration::from_secs(3600));
        for _ in 0..5 {
            let bad = sign(
                jsonwebtoken::Algorithm::RS256,
                "kid-c",
                &rsa(RSA_C),
                &claims(),
            );
            let _ = auth.authenticate(&bad).await;
        }
        assert_eq!(server.hits.load(std::sync::atomic::Ordering::SeqCst), 1);
        // Zero max age: a known key is re-validated against the source on every use, and a
        // key the issuer withdrew stops working without a restart.
        let auth = OidcAuthenticator::new(
            ISS.into(),
            AUD.into(),
            server.url.clone(),
            ClaimsPolicy::default(),
        )
        .with_refresh_policy(Duration::ZERO, Duration::ZERO);
        let a = sign(
            jsonwebtoken::Algorithm::RS256,
            "kid-a",
            &rsa(RSA_A),
            &claims(),
        );
        auth.authenticate(&a).await.unwrap();
        *server.document.lock().unwrap() = r#"{"keys":[]}"#.to_owned();
        assert!(matches!(
            auth.authenticate(&a).await,
            Err(AuthError::Unauthenticated(_))
        ));
    }

    #[tokio::test]
    async fn oidc_rejects_algorithm_confusion_missing_kid_and_bad_claims() {
        let server = jwks_server(JWKS_INITIAL).await;
        let auth = oidc(&server.url);
        // HS256 token naming the RSA key: the alg must match the key family.
        let mut header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::HS256);
        header.kid = Some("kid-a".into());
        let hs = jsonwebtoken::encode(
            &header,
            &claims(),
            &jsonwebtoken::EncodingKey::from_secret(JWKS_INITIAL.as_bytes()),
        )
        .unwrap();
        assert!(matches!(
            auth.authenticate(&hs).await,
            Err(AuthError::Unauthenticated(m)) if m.contains("does not match")
        ));
        // ES256 header with the RSA kid.
        let mut header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::ES256);
        header.kid = Some("kid-a".into());
        let es = jsonwebtoken::encode(
            &header,
            &claims(),
            &jsonwebtoken::EncodingKey::from_ec_pem(EC_B.as_bytes()).unwrap(),
        )
        .unwrap();
        assert!(matches!(
            auth.authenticate(&es).await,
            Err(AuthError::Unauthenticated(_))
        ));
        // No kid at all.
        let no_kid = jsonwebtoken::encode(
            &jsonwebtoken::Header::new(jsonwebtoken::Algorithm::RS256),
            &claims(),
            &rsa(RSA_A),
        )
        .unwrap();
        assert!(matches!(
            auth.authenticate(&no_kid).await,
            Err(AuthError::Unauthenticated(m)) if m.contains("no key id")
        ));
        // Missing iss / aud / exp are each refused (required spec claims).
        for missing in ["iss", "aud", "exp"] {
            let mut c = claims();
            c.as_object_mut().unwrap().remove(missing);
            let t = sign(jsonwebtoken::Algorithm::RS256, "kid-a", &rsa(RSA_A), &c);
            assert!(
                matches!(
                    auth.authenticate(&t).await,
                    Err(AuthError::Unauthenticated(_))
                ),
                "token without {missing} must be refused"
            );
        }
        // Wrong signature with a known kid.
        let forged = sign(
            jsonwebtoken::Algorithm::RS256,
            "kid-a",
            &rsa(RSA_C),
            &claims(),
        );
        assert!(matches!(
            auth.authenticate(&forged).await,
            Err(AuthError::Unauthenticated(_))
        ));
    }

    #[tokio::test]
    async fn oidc_unavailable_key_source_is_a_dependency_failure_not_a_bypass() {
        // Nothing listens here; the connection is refused.
        let auth = oidc("http://127.0.0.1:1/keys");
        let t = sign(
            jsonwebtoken::Algorithm::RS256,
            "kid-a",
            &rsa(RSA_A),
            &claims(),
        );
        match auth.authenticate(&t).await {
            Err(AuthError::KeySourceUnavailable(m)) => {
                assert!(!m.contains("127.0.0.1"), "url must not leak: {m}");
            }
            other => panic!("expected KeySourceUnavailable, got {other:?}"),
        }
    }

    #[test]
    fn claims_policy_maps_types_roles_and_delegation_explicitly() {
        let mut policy = ClaimsPolicy::default();
        policy.agent_client_ids.insert("app-agent".into());
        policy.service_client_ids.insert("app-svc".into());
        policy.on_behalf_of_claim = Some("obo".into());
        let base =
            || json!({"tid": "t", "oid": "s", "roles": ["ledger.propose", "Directory.Read"]});
        // Configured client id → agent; unknown roles ignored.
        let mut c = base();
        c["azp"] = json!("app-agent");
        let id = policy.identity_from(&c).unwrap();
        assert_eq!(id.principal.principal_type, PrincipalType::Agent);
        assert_eq!(id.principal.principal_id.as_str(), "urn:sculpin:agent:s");
        assert!(id.capabilities.has(Capability::Propose) && !id.capabilities.has(Capability::Read));
        // Service client id; roles as a space-separated string.
        let mut c = base();
        c["appid"] = json!("app-svc");
        c["roles"] = json!("ledger.read ledger.admin");
        let id = policy.identity_from(&c).unwrap();
        assert_eq!(id.principal.principal_type, PrincipalType::Service);
        assert!(id.capabilities.has(Capability::Admin));
        // Claim and configuration disagree → refused.
        let mut c = base();
        c["azp"] = json!("app-agent");
        c["sculpin_principal_type"] = json!("human");
        assert!(matches!(
            policy.identity_from(&c),
            Err(AuthError::Unauthenticated(m)) if m.contains("disagrees")
        ));
        // Claim alone, with delegation.
        let mut c = base();
        c["sculpin_principal_type"] = json!("agent");
        c["obo"] = json!("alice");
        let id = policy.identity_from(&c).unwrap();
        assert_eq!(
            id.principal.on_behalf_of.as_ref().unwrap().as_str(),
            "urn:sculpin:human:alice"
        );
        // Invalid type value, undeterminable type, no recognised role, missing tenant.
        let mut c = base();
        c["sculpin_principal_type"] = json!("robot");
        assert!(policy.identity_from(&c).is_err());
        assert!(policy.identity_from(&base()).is_err());
        let mut c = base();
        c["sculpin_principal_type"] = json!("human");
        c["roles"] = json!(["Directory.Read"]);
        assert!(policy.identity_from(&c).is_err());
        let mut c = base();
        c["sculpin_principal_type"] = json!("human");
        c.as_object_mut().unwrap().remove("tid");
        assert!(policy.identity_from(&c).is_err());
        // No-prefix mode keeps the subject verbatim.
        policy.principal_prefix = false;
        let mut c = base();
        c["sculpin_principal_type"] = json!("human");
        c["oid"] = json!("urn:x:alice");
        assert_eq!(
            policy
                .identity_from(&c)
                .unwrap()
                .principal
                .principal_id
                .as_str(),
            "urn:x:alice"
        );
    }

    #[test]
    fn dev_hs256_requires_a_long_secret_and_is_not_production_grade() {
        assert!(
            DevHs256Authenticator::new(ISS.into(), AUD.into(), b"short", ClaimsPolicy::default())
                .is_err()
        );
        let auth =
            DevHs256Authenticator::new(ISS.into(), AUD.into(), &[7u8; 32], ClaimsPolicy::default())
                .unwrap();
        assert!(!auth.is_production_grade());
        assert!(auth.describe().contains("DEVELOPMENT"));
    }
}
