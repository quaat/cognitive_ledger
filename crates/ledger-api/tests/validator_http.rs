//! The outbound validator client against a real local HTTP server (no PostgreSQL): the same
//! hardening standard as the JWKS fetch — no redirects followed, strict content type,
//! streamed size cap, retryable vs refused classification, bearer credential sent but never
//! echoed.

use axum::{
    Router,
    body::Body,
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
    routing::post,
};
use ledger_api::validator::{HttpValidationClient, HttpValidatorConfig};
use ledger_core::{CommitId, ContentId, GraphId};
use ledger_validation_protocol::{
    CandidateDescriptor, RequestedContext, VALIDATION_RESPONSE_PROTOCOL, ValidationClient,
    ValidationClientError, ValidationInvocationId, ValidationRequest,
};
use serde_json::json;
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

fn invocation() -> ValidationInvocationId {
    ValidationInvocationId(ContentId::for_bytes(b"one logical invocation"))
}

fn request() -> ValidationRequest {
    ValidationRequest::new(
        invocation(),
        CandidateDescriptor {
            graph_id: GraphId::new("g").unwrap(),
            knowledge_base_id: None,
            commit: CommitId(ContentId::for_bytes(b"c")),
            state_digest: ContentId::for_bytes(b"s"),
            state_href: None,
            quads: vec!["<urn:s> <urn:p> \"o\" .".into()],
        },
        RequestedContext::default(),
    )
}

fn valid_response() -> serde_json::Value {
    json!({
        "protocol": VALIDATION_RESPONSE_PROTOCOL,
        "candidate_commit": CommitId(ContentId::for_bytes(b"c")).to_string(),
        "candidate_state_digest": ContentId::for_bytes(b"s").to_string(),
        "context": {
            "base_kb": {"kb_id": "kb", "revision": "r"},
            "shapes": {"id": "s", "version": "1"},
            "virtual_contexts": [],
            "validator": {"service_version": "1", "configuration_version": "1"}
        },
        "outcome": {"kind": "conforms"},
        "report": {"digest": ContentId::for_bytes(b"report").to_string()}
    })
}

/// What the local server saw of the last request.
#[derive(Default)]
struct Seen {
    authorization: Option<String>,
    idempotency_key: Option<String>,
    body_invocation_id: Option<String>,
}

/// A local server whose single POST handler returns whatever `respond` builds; records the
/// Authorization and Idempotency-Key headers and the body's `invocation_id` it saw.
async fn server(
    respond: impl Fn() -> Response + Clone + Send + Sync + 'static,
) -> (String, Arc<Mutex<Seen>>) {
    let seen = Arc::new(Mutex::new(Seen::default()));
    let seen_in = seen.clone();
    let app = Router::new()
        .route(
            "/validate",
            post(move |headers: HeaderMap, body: axum::body::Bytes| {
                let respond = respond.clone();
                let seen = seen_in.clone();
                async move {
                    let header = |name| {
                        headers
                            .get(name)
                            .and_then(|v| v.to_str().ok())
                            .map(str::to_owned)
                    };
                    let body: serde_json::Value = serde_json::from_slice(&body).unwrap_or_default();
                    *seen.lock().unwrap() = Seen {
                        authorization: header(header::AUTHORIZATION.as_str()),
                        idempotency_key: header("idempotency-key"),
                        body_invocation_id: body["invocation_id"].as_str().map(str::to_owned),
                    };
                    respond()
                }
            }),
        )
        .route(
            "/elsewhere",
            post(|| async { axum::Json(valid_response()) }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}/validate"), seen)
}

fn client(endpoint: &str, max: usize) -> HttpValidationClient {
    HttpValidationClient::new(HttpValidatorConfig {
        endpoint: endpoint.into(),
        bearer_token: Some("workload-token-123".into()),
        timeout: Duration::from_secs(2),
        max_response_bytes: max,
        allow_insecure_loopback: true,
    })
    .unwrap()
}

#[tokio::test]
async fn a_valid_response_is_parsed_and_the_credential_is_sent() {
    let (url, seen) = server(|| axum::Json(valid_response()).into_response()).await;
    let response = client(&url, 64 * 1024).validate(&request()).await.unwrap();
    assert!(response.outcome.is_conforming());
    let seen = seen.lock().unwrap();
    assert_eq!(
        seen.authorization.as_deref(),
        Some("Bearer workload-token-123")
    );
    // One identifier, two representations: the Idempotency-Key header is the body's
    // invocation id (ADR-0019 amendment).
    let expected = invocation().to_string();
    assert_eq!(seen.idempotency_key.as_deref(), Some(expected.as_str()));
    assert_eq!(seen.body_invocation_id.as_deref(), Some(expected.as_str()));
}

#[tokio::test]
async fn redirects_are_not_followed() {
    let (url, _) = server(|| {
        Response::builder()
            .status(StatusCode::TEMPORARY_REDIRECT)
            .header(header::LOCATION, "/elsewhere")
            .body(Body::empty())
            .unwrap()
    })
    .await;
    match client(&url, 64 * 1024).validate(&request()).await {
        Err(ValidationClientError::Rejected(m)) => assert!(m.contains("redirect"), "{m}"),
        other => panic!("a redirect must be refused, got {other:?}"),
    }
}

#[tokio::test]
async fn unexpected_content_types_and_shapes_are_refused_without_echoing_the_body() {
    let (url, _) =
        server(|| ([(header::CONTENT_TYPE, "text/plain")], "SECRET-BODY").into_response()).await;
    match client(&url, 64 * 1024).validate(&request()).await {
        Err(ValidationClientError::Rejected(m)) => {
            assert!(
                m.contains("application/json") && !m.contains("SECRET-BODY"),
                "{m}"
            )
        }
        other => panic!("{other:?}"),
    }
    let (url, _) = server(|| {
        let mut body = valid_response();
        body["validator_identity"] = json!("forged");
        axum::Json(body).into_response()
    })
    .await;
    match client(&url, 64 * 1024).validate(&request()).await {
        Err(ValidationClientError::Rejected(m)) => assert!(!m.contains("forged"), "{m}"),
        other => panic!("unknown response fields must be refused, got {other:?}"),
    }
}

#[tokio::test]
async fn oversized_responses_are_cut_at_the_cap_even_without_content_length() {
    let (url, _) = server(|| {
        let chunks = futures_util::stream::iter(
            (0..64)
                .map(|_| Ok::<_, std::io::Error>(axum::body::Bytes::from(vec![b' '; 16 * 1024]))),
        );
        Response::builder()
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from_stream(chunks))
            .unwrap()
    })
    .await;
    match client(&url, 64 * 1024).validate(&request()).await {
        Err(ValidationClientError::Rejected(m)) => assert!(m.contains("size limit"), "{m}"),
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn server_errors_and_throttling_are_retryable_client_errors_are_not() {
    for (status, retryable) in [
        (StatusCode::SERVICE_UNAVAILABLE, true),
        (StatusCode::INTERNAL_SERVER_ERROR, true),
        (StatusCode::TOO_MANY_REQUESTS, true),
        (StatusCode::BAD_REQUEST, false),
        (StatusCode::UNAUTHORIZED, false),
    ] {
        let (url, _) = server(move || status.into_response()).await;
        let result = client(&url, 64 * 1024).validate(&request()).await;
        match (result, retryable) {
            (Err(ValidationClientError::Unavailable(_)), true)
            | (Err(ValidationClientError::Rejected(_)), false) => {}
            (other, _) => panic!("{status}: {other:?}"),
        }
    }
    // Nothing listening: unavailable, and the message names no URL.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    match client(&format!("http://{addr}/validate"), 1024)
        .validate(&request())
        .await
    {
        Err(ValidationClientError::Unavailable(m)) => {
            assert!(!m.contains(&addr.port().to_string()), "{m}")
        }
        other => panic!("{other:?}"),
    }
}
