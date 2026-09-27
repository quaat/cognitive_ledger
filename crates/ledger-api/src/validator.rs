//! The outbound boundary to the semantic validation service (ADR-0014; contract in
//! `docs/design/sculpin-validation-service.md`). Same hardening standard as the OIDC JWKS
//! fetch: one configured endpoint, no redirects, no proxy from the environment, a total
//! timeout, a streamed and capped response body, a strict content type and a strict JSON
//! shape. The bearer credential is never logged, echoed or placed in an error.

use ledger_validation_protocol::{
    ValidationClient, ValidationClientError, ValidationRequest, ValidatorResponse,
};
use std::time::Duration;

/// Configuration of the HTTP validator client. `endpoint` is the full URL of the
/// service's validate operation.
#[derive(Clone)]
pub struct HttpValidatorConfig {
    pub endpoint: String,
    /// Optional static bearer credential for the validation service (workload identity
    /// token or API key). Never logged.
    pub bearer_token: Option<String>,
    pub timeout: Duration,
    pub max_response_bytes: usize,
    /// Refuse plain `http://` unless the host is loopback (development only).
    pub allow_insecure_loopback: bool,
}

impl std::fmt::Debug for HttpValidatorConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpValidatorConfig")
            .field("endpoint", &"<redacted>")
            .field(
                "bearer_token",
                &self.bearer_token.as_ref().map(|_| "<redacted>"),
            )
            .field("timeout", &self.timeout)
            .field("max_response_bytes", &self.max_response_bytes)
            .finish()
    }
}

pub struct HttpValidationClient {
    http: reqwest::Client,
    endpoint: String,
    bearer_token: Option<String>,
    max_response_bytes: usize,
}

/// Why a configuration is refused (reported at start-up; never contains the URL).
#[derive(Debug)]
pub struct ValidatorConfigError(pub String);

impl std::fmt::Display for ValidatorConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for ValidatorConfigError {}

/// Endpoint policy: https always; plain http only for a loopback host when explicitly
/// allowed (development). No userinfo in the URL (credentials go in the token setting).
pub fn check_endpoint(
    endpoint: &str,
    allow_insecure_loopback: bool,
) -> Result<(), ValidatorConfigError> {
    let (scheme, rest) = endpoint
        .split_once("://")
        .ok_or_else(|| ValidatorConfigError("validator URL must be absolute".into()))?;
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    if authority.is_empty() {
        return Err(ValidatorConfigError("validator URL has no host".into()));
    }
    if authority.contains('@') {
        return Err(ValidatorConfigError(
            "validator URL must not carry credentials; use the token setting".into(),
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
                Err(ValidatorConfigError(
                    "validator URL must be https (plain http only to a loopback host in development)"
                        .into(),
                ))
            }
        }
        _ => Err(ValidatorConfigError(
            "validator URL scheme must be https".into(),
        )),
    }
}

impl HttpValidationClient {
    pub fn new(config: HttpValidatorConfig) -> Result<Self, ValidatorConfigError> {
        check_endpoint(&config.endpoint, config.allow_insecure_loopback)?;
        if let Some(token) = &config.bearer_token
            && (token.is_empty() || token.chars().any(|c| c.is_control() || c.is_whitespace()))
        {
            return Err(ValidatorConfigError(
                "validator bearer token must be a non-empty token without whitespace".into(),
            ));
        }
        let http = reqwest::Client::builder()
            .timeout(config.timeout)
            .connect_timeout(config.timeout.min(Duration::from_secs(10)))
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .build()
            .map_err(|_| ValidatorConfigError("validator HTTP client could not be built".into()))?;
        Ok(Self {
            http,
            endpoint: config.endpoint,
            bearer_token: config.bearer_token,
            max_response_bytes: config.max_response_bytes,
        })
    }
}

/// reqwest errors embed the URL; keep only the kind of failure.
fn redact(e: &reqwest::Error) -> &'static str {
    if e.is_timeout() {
        "timeout"
    } else if e.is_connect() {
        "connection failed"
    } else if e.is_body() || e.is_decode() {
        "body read failed"
    } else if e.is_request() {
        "request failed"
    } else {
        "transport error"
    }
}

#[async_trait::async_trait]
impl ValidationClient for HttpValidationClient {
    async fn validate(
        &self,
        request: &ValidationRequest,
    ) -> Result<ValidatorResponse, ValidationClientError> {
        use ValidationClientError::{Rejected, Unavailable};
        let mut builder = self
            .http
            .post(&self.endpoint)
            .header(reqwest::header::ACCEPT, "application/json")
            .json(request);
        if let Some(token) = &self.bearer_token {
            builder = builder.bearer_auth(token);
        }
        if let Some(correlation) = &request.correlation_id {
            builder = builder.header("x-correlation-id", correlation);
        }
        let mut response = builder
            .send()
            .await
            .map_err(|e| Unavailable(format!("validator call failed: {}", redact(&e))))?;
        let status = response.status();
        if status.is_redirection() {
            return Err(Rejected(format!(
                "validator answered with a redirect (HTTP {}); redirects are not followed",
                status.as_u16()
            )));
        }
        if status.is_server_error() || status.as_u16() == 429 {
            return Err(Unavailable(format!(
                "validator answered HTTP {}",
                status.as_u16()
            )));
        }
        if !status.is_success() {
            return Err(Rejected(format!(
                "validator refused the request (HTTP {})",
                status.as_u16()
            )));
        }
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_ascii_lowercase();
        let media = content_type.split(';').next().unwrap_or("").trim();
        if media != "application/json" {
            return Err(Rejected(
                "validator response is not application/json".into(),
            ));
        }
        if response
            .content_length()
            .is_some_and(|n| n > self.max_response_bytes as u64)
        {
            return Err(Rejected("validator response exceeds the size limit".into()));
        }
        let mut bytes: Vec<u8> = Vec::new();
        loop {
            let chunk = response.chunk().await.map_err(|e| {
                Unavailable(format!("validator response read failed: {}", redact(&e)))
            })?;
            let Some(chunk) = chunk else {
                break;
            };
            if bytes.len() + chunk.len() > self.max_response_bytes {
                return Err(Rejected("validator response exceeds the size limit".into()));
            }
            bytes.extend_from_slice(&chunk);
        }
        // The shape is strict (unknown fields refused); the message never echoes the body.
        serde_json::from_slice(&bytes)
            .map_err(|_| Rejected("validator response is not a valid validation response".into()))
    }

    fn describe(&self) -> String {
        "HTTP validation service (endpoint redacted)".into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_policy_is_https_or_loopback_http_in_development() {
        assert!(check_endpoint("https://validator.internal/validate", false).is_ok());
        assert!(check_endpoint("http://validator.internal/validate", true).is_err());
        assert!(check_endpoint("http://127.0.0.1:9000/validate", false).is_err());
        assert!(check_endpoint("http://127.0.0.1:9000/validate", true).is_ok());
        assert!(check_endpoint("http://localhost/validate", true).is_ok());
        assert!(check_endpoint("http://[::1]:9/v", true).is_ok());
        assert!(check_endpoint("http://localhost.evil.com/v", true).is_err());
        assert!(check_endpoint("http://127.0.0.1@evil.com/v", true).is_err());
        assert!(check_endpoint("https://user:pw@validator/v", false).is_err());
        assert!(check_endpoint("ftp://validator/v", false).is_err());
        assert!(check_endpoint("validator/v", false).is_err());
    }

    #[test]
    fn configuration_debug_never_prints_the_endpoint_or_token() {
        let config = HttpValidatorConfig {
            endpoint: "https://secret-host.internal/validate".into(),
            bearer_token: Some("super-secret-token".into()),
            timeout: Duration::from_secs(1),
            max_response_bytes: 1,
            allow_insecure_loopback: false,
        };
        let shown = format!("{config:?}");
        assert!(!shown.contains("secret-host") && !shown.contains("super-secret-token"));
        let bad_token = HttpValidatorConfig {
            bearer_token: Some("has space".into()),
            ..config
        };
        assert!(HttpValidationClient::new(bad_token).is_err());
    }
}
