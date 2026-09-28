//! The projection target adapter for a SPARQL 1.1 Protocol server (Apache Jena Fuseki;
//! ADR-0020). Same hardening as the OIDC and validator clients: endpoints are deployment
//! configuration only (never request data), https in production (plain http only to loopback
//! in development), no redirects, no proxy from the environment, connect and total timeouts,
//! bounded request and response bodies, strict content types, and credentials that are never
//! logged, echoed or placed in an error.

pub mod sparql;

use ledger_projection::{
    CognitiveGraph, MarkerTerm, Observation, ProjectedState, ProjectionClient, ProjectionError,
    ProjectionErrorCode as Code, ProjectionMarker, WriteMode,
};
use ledger_rdf::Quad;
use std::{collections::BTreeSet, time::Duration};

/// How the projector authenticates to the target (read from files at start-up).
#[derive(Clone)]
pub enum TargetCredentials {
    None,
    Basic { username: String, password: String },
    Bearer(String),
}

impl std::fmt::Debug for TargetCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::None => "None",
            Self::Basic { .. } => "Basic(<redacted>)",
            Self::Bearer(_) => "Bearer(<redacted>)",
        })
    }
}

#[derive(Clone)]
pub struct FusekiConfig {
    /// SPARQL query endpoint (e.g. `https://fuseki.internal/ledger/query`).
    pub query_endpoint: String,
    /// SPARQL update endpoint (e.g. `https://fuseki.internal/ledger/update`).
    pub update_endpoint: String,
    pub credentials: TargetCredentials,
    pub connect_timeout: Duration,
    pub request_timeout: Duration,
    pub max_update_bytes: usize,
    pub max_response_bytes: usize,
    /// Plain `http://` to a loopback host (development only).
    pub allow_insecure_loopback: bool,
}

impl std::fmt::Debug for FusekiConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Endpoints are deployment configuration and never logged (docs/quality/security.md).
        f.debug_struct("FusekiConfig")
            .field("query_endpoint", &"<redacted>")
            .field("update_endpoint", &"<redacted>")
            .field("credentials", &self.credentials)
            .field("connect_timeout", &self.connect_timeout)
            .field("request_timeout", &self.request_timeout)
            .field("max_update_bytes", &self.max_update_bytes)
            .field("max_response_bytes", &self.max_response_bytes)
            .field("allow_insecure_loopback", &self.allow_insecure_loopback)
            .finish()
    }
}

/// Why a configuration is refused (never contains the URL or a credential).
#[derive(Debug)]
pub struct TargetConfigError(pub String);

impl std::fmt::Display for TargetConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for TargetConfigError {}

/// Endpoint policy: absolute, https (plain http only to a loopback host when allowed), no
/// userinfo, no query or fragment.
pub fn check_endpoint(
    endpoint: &str,
    allow_insecure_loopback: bool,
) -> Result<(), TargetConfigError> {
    let err = |m: &str| TargetConfigError(format!("projection target endpoint {m}"));
    let (scheme, rest) = endpoint
        .split_once("://")
        .ok_or_else(|| err("must be an absolute URL"))?;
    if rest.contains(['?', '#']) {
        return Err(err("must not carry a query or fragment"));
    }
    let authority = rest.split('/').next().unwrap_or("");
    if authority.is_empty() {
        return Err(err("has no host"));
    }
    if authority.contains('@') {
        return Err(err(
            "must not carry credentials; use the credential file settings",
        ));
    }
    let host = if let Some(bracketed) = authority.strip_prefix('[') {
        bracketed.split(']').next().unwrap_or("")
    } else {
        authority.rsplit_once(':').map_or(authority, |(h, _)| h)
    };
    match scheme {
        "https" => Ok(()),
        "http" => {
            let loopback = host == "localhost"
                || host
                    .parse::<std::net::IpAddr>()
                    .is_ok_and(|ip| ip.is_loopback());
            if allow_insecure_loopback && loopback {
                Ok(())
            } else {
                Err(err(
                    "must be https (plain http only to a loopback host in development)",
                ))
            }
        }
        _ => Err(err("scheme must be https")),
    }
}

pub struct FusekiClient {
    http: reqwest::Client,
    query_endpoint: String,
    update_endpoint: String,
    credentials: TargetCredentials,
    max_update_bytes: usize,
    max_response_bytes: usize,
}

/// reqwest errors embed the URL; keep only the kind of failure.
fn transport(e: &reqwest::Error) -> ProjectionError {
    if e.is_timeout() {
        ProjectionError::retryable(Code::TargetTimeout, "projection target timed out")
    } else if e.is_connect() {
        ProjectionError::retryable(
            Code::TargetUnavailable,
            "projection target connection failed",
        )
    } else {
        ProjectionError::retryable(Code::TargetUnavailable, "projection target transport error")
    }
}

fn classify(status: reqwest::StatusCode) -> ProjectionError {
    let code = status.as_u16();
    if status.is_server_error() {
        ProjectionError::retryable(
            Code::TargetServerError,
            format!("projection target answered HTTP {code}"),
        )
    } else if code == 429 {
        ProjectionError::retryable(Code::TargetThrottled, "projection target answered HTTP 429")
    } else if code == 401 || code == 403 {
        ProjectionError::permanent(
            Code::TargetAuth,
            format!("projection target refused the credentials (HTTP {code})"),
        )
    } else if status.is_redirection() {
        ProjectionError::permanent(
            Code::TargetProtocol,
            format!(
                "projection target answered with a redirect (HTTP {code}); redirects are not followed"
            ),
        )
    } else {
        ProjectionError::permanent(
            Code::TargetProtocol,
            format!("projection target refused the request (HTTP {code})"),
        )
    }
}

impl FusekiClient {
    pub fn new(config: FusekiConfig) -> Result<Self, TargetConfigError> {
        check_endpoint(&config.query_endpoint, config.allow_insecure_loopback)?;
        check_endpoint(&config.update_endpoint, config.allow_insecure_loopback)?;
        let token_ok = |t: &str| !t.is_empty() && t.bytes().all(|b| b.is_ascii_graphic());
        match &config.credentials {
            TargetCredentials::Bearer(t) if !token_ok(t) => {
                return Err(TargetConfigError(
                    "projection target bearer token must be non-empty visible ASCII".into(),
                ));
            }
            TargetCredentials::Basic { username, password }
                if username.is_empty()
                    || username.contains(':')
                    || username.chars().any(char::is_control)
                    || password.is_empty()
                    || password.chars().any(char::is_control) =>
            {
                return Err(TargetConfigError(
                    "projection target basic credentials must be a non-empty user without ':' and a \
                     non-empty password without control characters"
                        .into(),
                ));
            }
            _ => {}
        }
        let http = reqwest::Client::builder()
            .timeout(config.request_timeout)
            .connect_timeout(config.connect_timeout)
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .pool_max_idle_per_host(8)
            .build()
            .map_err(|_| TargetConfigError("projection HTTP client could not be built".into()))?;
        Ok(Self {
            http,
            query_endpoint: config.query_endpoint,
            update_endpoint: config.update_endpoint,
            credentials: config.credentials,
            max_update_bytes: config.max_update_bytes,
            max_response_bytes: config.max_response_bytes,
        })
    }

    fn authorize(&self, builder: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match &self.credentials {
            TargetCredentials::None => builder,
            TargetCredentials::Basic { username, password } => {
                builder.basic_auth(username, Some(password))
            }
            TargetCredentials::Bearer(token) => builder.bearer_auth(token),
        }
    }

    fn too_large(&self, bytes: usize) -> ProjectionError {
        ProjectionError::permanent(
            Code::StateTooLarge,
            format!(
                "the projection request (~{bytes} bytes) exceeds the configured limit of {} bytes",
                self.max_update_bytes
            ),
        )
    }

    /// POST a SPARQL Update; `Ok` only on 2xx.
    async fn update(&self, body: String) -> Result<(), ProjectionError> {
        let (status, _) = self.update_raw(body).await?;
        if status.is_success() {
            Ok(())
        } else {
            Err(classify(status))
        }
    }

    /// POST a SPARQL Update and return the status with a bounded body excerpt (probe).
    async fn update_raw(
        &self,
        body: String,
    ) -> Result<(reqwest::StatusCode, String), ProjectionError> {
        if body.len() > self.max_update_bytes {
            return Err(self.too_large(body.len()));
        }
        let mut response = self
            .authorize(
                self.http
                    .post(&self.update_endpoint)
                    .header(reqwest::header::CONTENT_TYPE, "application/sparql-update"),
            )
            .body(body)
            .send()
            .await
            .map_err(|e| transport(&e))?;
        let status = response.status();
        let mut excerpt = Vec::new();
        while excerpt.len() < 4096 {
            match response.chunk().await {
                Ok(Some(chunk)) => {
                    // Keep at most the excerpt; never copy a whole oversized chunk.
                    let take = (4096 - excerpt.len()).min(chunk.len());
                    excerpt.extend_from_slice(&chunk[..take]);
                }
                _ => break,
            }
        }
        excerpt.truncate(4096);
        Ok((status, String::from_utf8_lossy(&excerpt).into_owned()))
    }

    /// POST a SPARQL query and return the bounded body of the expected media type.
    async fn query(&self, query: String, accept: &str) -> Result<Vec<u8>, ProjectionError> {
        if query.len() > self.max_update_bytes {
            return Err(self.too_large(query.len()));
        }
        let mut response = self
            .authorize(
                self.http
                    .post(&self.query_endpoint)
                    .header(reqwest::header::CONTENT_TYPE, "application/sparql-query")
                    .header(reqwest::header::ACCEPT, accept),
            )
            .body(query)
            .send()
            .await
            .map_err(|e| transport(&e))?;
        let status = response.status();
        if !status.is_success() {
            return Err(classify(status));
        }
        let media = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .split(';')
            .next()
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();
        if media != accept {
            return Err(ProjectionError::permanent(
                Code::TargetProtocol,
                "projection target answered with an unexpected content type",
            ));
        }
        let too_large = || {
            ProjectionError::permanent(
                Code::StateTooLarge,
                "projection target response exceeds the configured size limit",
            )
        };
        if response
            .content_length()
            .is_some_and(|n| n > self.max_response_bytes as u64)
        {
            return Err(too_large());
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|e| transport(&e))? {
            if bytes.len() + chunk.len() > self.max_response_bytes {
                return Err(too_large());
            }
            bytes.extend_from_slice(&chunk);
        }
        Ok(bytes)
    }
}

const SPARQL_JSON: &str = "application/sparql-results+json";
const N_TRIPLES: &str = "application/n-triples";

#[async_trait::async_trait]
impl ProjectionClient for FusekiClient {
    async fn observe(&self, graph: &CognitiveGraph) -> Result<Observation, ProjectionError> {
        let body = self
            .query(sparql::observe_query(graph), SPARQL_JSON)
            .await?;
        sparql::parse_observation(&body)
    }

    async fn write(
        &self,
        graph: &CognitiveGraph,
        state: &ProjectedState,
        marker: &ProjectionMarker,
        mode: WriteMode,
        expected: &[(String, MarkerTerm)],
    ) -> Result<(), ProjectionError> {
        // Refuse before formatting the request: no multi-copy of an oversized state.
        let estimate = state.byte_len() + 16 * 1024;
        if estimate > self.max_update_bytes {
            return Err(self.too_large(estimate));
        }
        self.update(sparql::write_update(graph, state, marker, mode, expected)?)
            .await
    }

    async fn fence(
        &self,
        graph: &CognitiveGraph,
        expected: &[(String, MarkerTerm)],
        write_id: &str,
    ) -> Result<(), ProjectionError> {
        self.update(sparql::fence_update(graph, expected, write_id)?)
            .await
    }

    async fn contains_all(
        &self,
        graph: &CognitiveGraph,
        state: &ProjectedState,
    ) -> Result<bool, ProjectionError> {
        if state.triples().is_empty() {
            return Ok(true);
        }
        let estimate = state.byte_len() + 1024;
        if estimate > self.max_update_bytes {
            return Err(self.too_large(estimate));
        }
        let body = self
            .query(sparql::contains_all_query(graph, state), SPARQL_JSON)
            .await?;
        sparql::parse_ask(&body)
    }

    async fn bind_target(&self, target_id: &str) -> Result<(), ProjectionError> {
        if target_id.is_empty()
            || !target_id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b':' | b'-'))
        {
            return Err(ProjectionError::permanent(
                Code::InvalidTargetGraph,
                "the target id must match [A-Za-z0-9._:-]+",
            ));
        }
        self.update(sparql::bind_target_update(target_id)).await?;
        let bound = sparql::parse_bound_target(
            &self
                .query(sparql::bound_target_query(), SPARQL_JSON)
                .await?,
        )?;
        // A union default graph would merge every cognitive graph and the markers into
        // readers' default-graph queries (ADR-0020): the binding must not be visible there.
        if sparql::parse_ask(
            &self
                .query(sparql::union_default_graph_ask(), SPARQL_JSON)
                .await?,
        )? {
            return Err(ProjectionError::permanent(
                Code::TargetProtocol,
                "the target dataset exposes its named graphs in the default graph (union default \
                 graph); refusing to project into it (ADR-0020)",
            ));
        }
        if bound == [target_id] {
            Ok(())
        } else {
            Err(ProjectionError::permanent(
                Code::TargetConflict,
                format!(
                    "the target dataset is bound to {} other target id(s); one dataset serves one \
                     target id (ADR-0020)",
                    bound.iter().filter(|b| *b != target_id).count()
                ),
            ))
        }
    }

    async fn read_graph(&self, graph: &CognitiveGraph) -> Result<BTreeSet<Quad>, ProjectionError> {
        let body = self
            .query(sparql::read_graph_query(graph), N_TRIPLES)
            .await?;
        let text = std::str::from_utf8(&body).map_err(|_| {
            ProjectionError::permanent(Code::TargetProtocol, "the graph read is not UTF-8")
        })?;
        text.lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .map(|line| {
                line.parse::<Quad>().map_err(|_| {
                    ProjectionError::permanent(
                        Code::VerificationFailed,
                        "the target graph holds a statement the ledger cannot represent",
                    )
                })
            })
            .collect()
    }

    async fn probe_transactional(&self) -> Result<(), ProjectionError> {
        // The probe request must fail as a whole, and it must fail *executing* (the LOAD
        // step): only an HTTP 500 naming the LOAD proves the request ran and aborted. A
        // proxy's 502/503/504 or a 4xx rejection before execution proves nothing.
        let (status, body) = self.update_raw(sparql::probe_update()).await?;
        if status.is_success() {
            let _ = self.update(sparql::probe_cleanup()).await;
            return Err(ProjectionError::permanent(
                Code::TargetNotTransactional,
                "the target accepted an update whose last operation must fail",
            ));
        }
        if status.as_u16() != 500 || !body.contains("LOAD") {
            return Err(match classify(status) {
                e if e.is_retryable() || e.code() == Code::TargetAuth => e,
                _ => ProjectionError::permanent(
                    Code::TargetNotTransactional,
                    "the target rejected the transactional probe before executing it",
                ),
            });
        }
        let kept = sparql::parse_ask(&self.query(sparql::probe_ask(), SPARQL_JSON).await?)?;
        if kept {
            let _ = self.update(sparql::probe_cleanup()).await;
            return Err(ProjectionError::permanent(
                Code::TargetNotTransactional,
                "the target kept the first operation of a failed update: its dataset is not \
                 transactional (use TDB2 or the transactional in-memory dataset; ADR-0020)",
            ));
        }
        Ok(())
    }

    fn describe(&self) -> String {
        "SPARQL 1.1 projection target (endpoints redacted)".into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_policy_matches_the_other_outbound_clients() {
        for ok in [
            "https://fuseki.internal/ledger/update",
            "https://[::1]:3030/ds/query",
        ] {
            assert!(check_endpoint(ok, false).is_ok(), "{ok}");
        }
        assert!(check_endpoint("http://127.0.0.1:3030/ledger/update", true).is_ok());
        assert!(check_endpoint("http://localhost:3030/ledger/update", true).is_ok());
        for bad in [
            "http://fuseki.internal/ledger/update", // http to a non-loopback host
            "https://user:pw@fuseki.internal/ledger", // credentials in the URL
            "https://fuseki.internal/ledger?x=1",   // query
            "https://fuseki.internal/ledger#f",     // fragment
            "ftp://fuseki.internal/ledger",
            "fuseki.internal/ledger",
            "https:///ledger",
        ] {
            assert!(check_endpoint(bad, true).is_err(), "{bad}");
        }
        // plain http to loopback only with the development allowance
        assert!(check_endpoint("http://127.0.0.1:3030/ledger/update", false).is_err());
    }

    #[test]
    fn credentials_are_never_printed() {
        let basic = TargetCredentials::Basic {
            username: "projector".into(),
            password: "s3cret".into(),
        };
        let bearer = TargetCredentials::Bearer("tok-123".into());
        for c in [basic, bearer] {
            let shown = format!("{c:?}");
            assert!(
                !shown.contains("s3cret")
                    && !shown.contains("tok-123")
                    && !shown.contains("projector")
            );
        }
    }

    #[test]
    fn status_classification_is_stable() {
        use reqwest::StatusCode as S;
        let cases = [
            (S::INTERNAL_SERVER_ERROR, Code::TargetServerError, true),
            (S::SERVICE_UNAVAILABLE, Code::TargetServerError, true),
            (S::TOO_MANY_REQUESTS, Code::TargetThrottled, true),
            (S::UNAUTHORIZED, Code::TargetAuth, false),
            (S::FORBIDDEN, Code::TargetAuth, false),
            (S::BAD_REQUEST, Code::TargetProtocol, false),
            (S::NOT_FOUND, Code::TargetProtocol, false),
            (S::TEMPORARY_REDIRECT, Code::TargetProtocol, false),
        ];
        for (status, code, retry) in cases {
            let e = classify(status);
            assert_eq!((e.code(), e.is_retryable()), (code, retry), "{status}");
        }
    }
}
