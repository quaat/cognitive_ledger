//! Graph authority rows (ADR-0010): the minimal create/read primitive the storage layer
//! and the migration tooling need. Graph lifecycle policy, authorization and the HTTP
//! surface belong to later phases; this module only guarantees the schema's invariants
//! (global `graph_id` uniqueness, immutable tenant binding, many graphs per KB).

use crate::storage;
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
}

impl PgGraphs {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
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
        let row = sqlx::query(
            "SELECT graph_id, tenant_id, knowledge_base_id, purpose, status FROM graphs \
             WHERE graph_id = $1",
        )
        .bind(graph_id.as_str())
        .fetch_optional(&self.pool)
        .await
        .map_err(storage)?;
        let Some(row) = row else {
            return Ok(None);
        };
        let graph_id: String = row.try_get("graph_id").map_err(storage)?;
        let tenant_id: String = row.try_get("tenant_id").map_err(storage)?;
        let status: String = row.try_get("status").map_err(storage)?;
        Ok(Some(GraphRecord {
            graph_id: GraphId::new(graph_id)?,
            tenant_id: TenantId::new(tenant_id)?,
            knowledge_base_id: row.try_get("knowledge_base_id").map_err(storage)?,
            purpose: row.try_get("purpose").map_err(storage)?,
            status: GraphStatus::parse(&status)?,
        }))
    }
}
