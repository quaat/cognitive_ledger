//! Graph authority rows (ADR-0010): the minimal create/read primitive the storage layer
//! and the migration tooling need. Graph lifecycle policy, authorization and the HTTP
//! surface belong to later phases; this module only guarantees the schema's invariants
//! (global `graph_id` uniqueness, immutable tenant binding, many graphs per KB).

use crate::{db_error, storage};
use ledger_core::{GraphId, LedgerError, TenantId};
use sqlx::{PgPool, Row};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GraphStatus {
    /// The pre-v2 single-graph topology (`default`). Not a production tenant.
    Bootstrap,
    Active,
    /// Receiving an audited history import; not yet serving writes.
    Importing,
    Archived,
}

impl GraphStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Bootstrap => "bootstrap",
            Self::Active => "active",
            Self::Importing => "importing",
            Self::Archived => "archived",
        }
    }
    pub fn parse(value: &str) -> Result<Self, LedgerError> {
        match value {
            "bootstrap" => Ok(Self::Bootstrap),
            "active" => Ok(Self::Active),
            "importing" => Ok(Self::Importing),
            "archived" => Ok(Self::Archived),
            other => Err(LedgerError::Storage(format!(
                "unknown graph status {other:?}"
            ))),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NewGraph {
    pub graph_id: GraphId,
    pub tenant_id: TenantId,
    pub knowledge_base_id: Option<String>,
    pub purpose: Option<String>,
    pub status: GraphStatus,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GraphRecord {
    pub graph_id: GraphId,
    pub tenant_id: TenantId,
    pub knowledge_base_id: Option<String>,
    pub purpose: Option<String>,
    pub status: GraphStatus,
}

#[derive(Clone, Debug)]
pub struct PgGraphs {
    pool: PgPool,
    /// How long a request-path lookup may wait for a connection (bounded further by the
    /// request budget, ADR-0026 §8).
    acquire_timeout: std::time::Duration,
}

impl PgGraphs {
    pub fn new(pool: PgPool) -> Self {
        Self {
            pool,
            acquire_timeout: crate::DbSessionLimits::default().acquire_timeout,
        }
    }

    /// Lifecycle limits for request-path lookups (ADR-0026).
    pub fn with_session_limits(mut self, session: crate::DbSessionLimits) -> Self {
        self.acquire_timeout = session.acquire_timeout;
        self
    }

    /// Insert a graph. A `graph_id` that already exists — under any tenant — is
    /// `GraphAlreadyExists`; the row is never updated.
    pub async fn create(&self, graph: &NewGraph) -> Result<(), LedgerError> {
        let result = sqlx::query(
            "INSERT INTO graphs (graph_id, tenant_id, knowledge_base_id, purpose, status) \
             VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(graph.graph_id.as_str())
        .bind(graph.tenant_id.as_str())
        .bind(graph.knowledge_base_id.as_deref())
        .bind(graph.purpose.as_deref())
        .bind(graph.status.as_str())
        .execute(&self.pool)
        .await;
        match result {
            Ok(_) => Ok(()),
            Err(sqlx::Error::Database(e)) if e.is_unique_violation() => {
                Err(LedgerError::GraphAlreadyExists(graph.graph_id.to_string()))
            }
            Err(e) => Err(storage(e)),
        }
    }

    pub async fn get(&self, graph_id: &GraphId) -> Result<Option<GraphRecord>, LedgerError> {
        // The first pooled query of every authenticated route (`authorized_graph`): waited for
        // within the request budget like every other request-path acquisition.
        let mut conn = crate::lifecycle::acquire(&self.pool, self.acquire_timeout).await?;
        let row = sqlx::query(
            "SELECT graph_id, tenant_id, knowledge_base_id, purpose, status FROM graphs \
             WHERE graph_id = $1",
        )
        .bind(graph_id.as_str())
        .fetch_optional(&mut *conn)
        .await
        .map_err(db_error)?;
        let Some(row) = row else {
            return Ok(None);
        };
        let graph_id: String = row.try_get("graph_id").map_err(db_error)?;
        let tenant_id: String = row.try_get("tenant_id").map_err(db_error)?;
        let status: String = row.try_get("status").map_err(db_error)?;
        Ok(Some(GraphRecord {
            graph_id: GraphId::new(graph_id)?,
            tenant_id: TenantId::new(tenant_id)?,
            knowledge_base_id: row.try_get("knowledge_base_id").map_err(db_error)?,
            purpose: row.try_get("purpose").map_err(db_error)?,
            status: GraphStatus::parse(&status)?,
        }))
    }
}
