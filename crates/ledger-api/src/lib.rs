//! Minimal HTTP adapter. It deliberately exposes no query language.
use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use ledger_core::{CommitId, LedgerError};
use ledger_rdf::{Operation, OperationKind, Patch, Quad};
use ledger_store::{CommitRequest, Ledger};
use serde::{Deserialize, Serialize};
use std::{str::FromStr, sync::Arc};

pub fn router(ledger: Arc<Ledger>) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/v1/refs/main", get(head))
        .route("/v1/commits", post(commit))
        .route("/v1/states/{id}", get(state_at))
        .with_state(ledger)
}
async fn health() -> Json<Health> {
    Json(Health { status: "ok" })
}
#[derive(Serialize)]
struct Health {
    status: &'static str,
}
#[derive(Serialize)]
struct Head {
    head: Option<CommitId>,
}
async fn head(State(l): State<Arc<Ledger>>) -> Result<Json<Head>, ApiError> {
    Ok(Json(Head {
        head: l.head().await?,
    }))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommitBody {
    pub expected_head: Option<CommitId>,
    pub operations: Vec<OperationBody>,
    pub author: String,
    pub message: String,
    pub event_time: String,
}
#[derive(Deserialize)]
pub struct OperationBody {
    pub op: String,
    pub quad: String,
}
#[derive(Serialize)]
struct CommitResponse {
    id: CommitId,
}
async fn commit(
    State(l): State<Arc<Ledger>>,
    Json(body): Json<CommitBody>,
) -> Result<(StatusCode, Json<CommitResponse>), ApiError> {
    let operations = body
        .operations
        .into_iter()
        .map(|o| {
            let kind = match o.op.as_str() {
                "add" => Ok(OperationKind::Add),
                "delete" => Ok(OperationKind::Delete),
                _ => Err(ApiError::bad_request("operation must be add or delete")),
            }?;
            let quad = Quad::from_str(&o.quad).map_err(|e| ApiError::bad_request(e.to_string()))?;
            Ok(Operation { kind, quad })
        })
        .collect::<Result<Vec<_>, ApiError>>()?;
    let patch = Patch::new(operations).map_err(|e| ApiError::bad_request(e.to_string()))?;
    let id = l
        .commit(CommitRequest {
            expected_head: body.expected_head,
            patch,
            author: body.author,
            message: body.message,
            event_time: body.event_time,
        })
        .await?;
    Ok((StatusCode::CREATED, Json(CommitResponse { id })))
}
#[derive(Serialize)]
struct StateResponse {
    quads: Vec<String>,
}
async fn state_at(
    State(l): State<Arc<Ledger>>,
    Path(id): Path<String>,
) -> Result<Json<StateResponse>, ApiError> {
    let id = CommitId::from_str(&id).map_err(|e| ApiError::bad_request(e.to_string()))?;
    let quads = l
        .state_at(&id)
        .await?
        .into_iter()
        .map(|q| q.to_string())
        .collect();
    Ok(Json(StateResponse { quads }))
}
#[derive(Debug)]
struct ApiError {
    status: StatusCode,
    code: &'static str,
    message: String,
}
impl ApiError {
    fn bad_request(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            code: "INVALID_REQUEST",
            message: message.into(),
        }
    }
}
impl From<LedgerError> for ApiError {
    fn from(e: LedgerError) -> Self {
        match e {
            LedgerError::HeadChanged { .. } => Self {
                status: StatusCode::CONFLICT,
                code: "HEAD_CHANGED",
                message: e.to_string(),
            },
            LedgerError::NotFound(_) => Self {
                status: StatusCode::NOT_FOUND,
                code: "NOT_FOUND",
                message: e.to_string(),
            },
            LedgerError::MissingParent(_)
            | LedgerError::MissingPatch(_)
            | LedgerError::MissingTarget(_)
            | LedgerError::InvalidContentId(_)
            | LedgerError::InvalidCommit(_) => Self::bad_request(e.to_string()),
            _ => Self {
                status: StatusCode::INTERNAL_SERVER_ERROR,
                code: "INTERNAL",
                message: "internal ledger error".into(),
            },
        }
    }
}
#[derive(Serialize)]
struct ErrorBody<'a> {
    code: &'a str,
    message: &'a str,
}
impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(ErrorBody {
                code: self.code,
                message: &self.message,
            }),
        )
            .into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::{Body, to_bytes},
        http::Request,
    };
    use tower::ServiceExt;
    #[tokio::test]
    async fn health_is_ok() {
        let d = tempfile::tempdir().unwrap();
        let app = router(Arc::new(Ledger::open(d.path()).unwrap()));
        let r = app
            .oneshot(
                Request::builder()
                    .uri("/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::OK);
    }
    #[tokio::test]
    async fn stale_head_is_409() {
        let d = tempfile::tempdir().unwrap();
        let ledger = Arc::new(Ledger::open(d.path()).unwrap());
        let patch = Patch::new([Operation {
            kind: OperationKind::Add,
            quad: "<urn:s> <urn:p> \"v\" .".parse().unwrap(),
        }])
        .unwrap();
        let request = |expected| CommitRequest {
            expected_head: expected,
            patch: patch.clone(),
            author: "a".into(),
            message: "m".into(),
            event_time: "e".into(),
        };
        let c1 = ledger.commit(request(None)).await.unwrap();
        let _c2 = ledger.commit(request(Some(c1.clone()))).await.unwrap();
        let json = format!(
            r#"{{"expected_head":"{c1}","operations":[{{"op":"add","quad":"<urn:x> <urn:p> <urn:o> ."}}],"author":"a","message":"m","event_time":"e"}}"#
        );
        let app = router(ledger);
        let r = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/commits")
                    .header("content-type", "application/json")
                    .body(Body::from(json))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::CONFLICT);
        let bytes = to_bytes(r.into_body(), 1024).await.unwrap();
        assert!(
            std::str::from_utf8(&bytes)
                .unwrap()
                .contains("HEAD_CHANGED")
        );
    }
    #[tokio::test]
    async fn clients_cannot_supply_recorded_time() {
        let d = tempfile::tempdir().unwrap();
        let ledger = Arc::new(Ledger::open(d.path()).unwrap());
        let body = r#"{"expected_head":null,"operations":[],"author":"a","message":"m","event_time":"e","recorded_time":"forged"}"#;
        let response = router(Arc::clone(&ledger))
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/commits")
                    .header("content-type", "application/json")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert!(response.status().is_client_error());
        assert_eq!(ledger.head().await.unwrap(), None);
    }
}
