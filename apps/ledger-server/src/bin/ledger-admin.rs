//! Administrative entry points that must never run inside request handling.
//!
//! ```text
//! LEDGER_MIGRATION_DATABASE_URL=postgres://… ledger-admin migrate [--runtime-role <name>]
//! LEDGER_MIGRATION_DATABASE_URL=postgres://… ledger-admin migrate-fs-to-pg --source <dir> \
//!     [--graph default] [--branch main] [--json]
//! LEDGER_MIGRATION_DATABASE_URL=postgres://… ledger-admin graph create --graph <id> --tenant <id> \
//!     [--status active|importing] [--kb <id>] [--purpose <text>]
//! ```
//! Every command runs under the **schema owner / migration identity** (ADR-0016): the
//! runtime identity cannot run DDL or provision graphs. Graph provisioning is deliberately
//! an operator command (ADR-0010): the public HTTP surface has no graph administration API.
//! The database URL carries credentials: prefer the environment variable. `--database-url`
//! is accepted for scripted use but exposes the URL in process listings. No argument value
//! is ever echoed back, so a misplaced URL cannot leak through an error message.

use ledger_core::{GraphId, TenantId};
use ledger_store::{
    FileStore, FsToPgMigration, GraphStatus, NewGraph, PgGraphs, PgRefStore,
    PostgresImmutableStore, V1Binding,
};
use std::{env, process::ExitCode};

enum Command {
    MigrateFs(Args),
    CreateGraph(GraphArgs),
    Migrate(MigrateArgs),
}

struct MigrateArgs {
    database_url: String,
    runtime_role: Option<String>,
}

struct GraphArgs {
    database_url: String,
    graph: String,
    tenant: String,
    status: GraphStatus,
    knowledge_base: Option<String>,
    purpose: Option<String>,
}

struct Args {
    source: String,
    database_url: String,
    graph: String,
    branch: String,
    json: bool,
    runtime_role: Option<String>,
}

fn usage() -> &'static str {
    "usage: ledger-admin migrate [--runtime-role <role>] [--database-url <url>]\n\
     \x20      ledger-admin migrate-fs-to-pg --source <dir> [--database-url <url>] \
     [--graph <graph_id>] [--branch <name>] [--runtime-role <role>] [--json]\n\
     \x20      ledger-admin graph create --graph <graph_id> --tenant <tenant_id> \
     [--status active|importing] [--kb <knowledge_base_id>] [--purpose <text>] \
     [--database-url <url>]\n\
     (the database url is the schema OWNER / migration identity and defaults to \
     $LEDGER_MIGRATION_DATABASE_URL, which is preferred; the runtime LEDGER_DATABASE_URL is \
     deliberately not used; argument values are never echoed)"
}

fn database_url_from_env() -> Option<String> {
    env::var("LEDGER_MIGRATION_DATABASE_URL")
        .ok()
        .filter(|u| !u.is_empty())
}

/// A runtime role must be a plain SQL identifier; anything else (for example a database
/// URL pasted by mistake) is refused before it can be echoed or sent to the server.
fn role_name(value: String) -> Result<String, String> {
    let ok = value.len() <= 63
        && value
            .bytes()
            .next()
            .is_some_and(|b| b.is_ascii_lowercase() || b == b'_')
        && value
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_');
    if ok {
        Ok(value)
    } else {
        Err("--runtime-role must be a lower-case SQL identifier ([a-z_][a-z0-9_]{0,62}); value not shown".into())
    }
}

fn parse_migrate(mut argv: impl Iterator<Item = String>) -> Result<MigrateArgs, String> {
    let mut database_url = database_url_from_env();
    let mut runtime_role = None;
    while let Some(flag) = argv.next() {
        match flag.as_str() {
            "--database-url" => database_url = Some(value(&mut argv, "--database-url")?),
            "--runtime-role" => {
                runtime_role = Some(role_name(value(&mut argv, "--runtime-role")?)?)
            }
            other if other.starts_with("--") => {
                return Err(format!("unknown flag {other}\n{}", usage()));
            }
            _ => {
                return Err(format!(
                    "unexpected positional argument (value not shown)\n{}",
                    usage()
                ));
            }
        }
    }
    Ok(MigrateArgs {
        database_url: database_url.ok_or_else(|| {
            format!(
                "--database-url or LEDGER_MIGRATION_DATABASE_URL is required\n{}",
                usage()
            )
        })?,
        runtime_role,
    })
}

/// Apply every migration on one dedicated owner connection, then (optionally) grant the
/// runtime role its privileges through the versioned function from migration 0008.
async fn migrate_schema(args: &MigrateArgs) -> Result<(), Box<dyn std::error::Error>> {
    use sqlx::{Connection, Executor};
    let mut conn = sqlx::postgres::PgConnection::connect(&args.database_url).await?;
    // Never hang silently behind a replica that is still running or a held migration lock.
    conn.execute("SET lock_timeout = '60s'").await?;
    ledger_store::schema::migrate_all_on(&mut conn).await?;
    if let Some(role) = &args.runtime_role {
        ledger_store::schema::grant_runtime_role(&mut conn, role).await?;
        println!("granted runtime privileges to role {role} (ledger_grant_runtime)");
    } else {
        println!(
            "no --runtime-role given: migrations applied, runtime grants not touched (a runtime \
             identity without grants cannot serve; see ADR-0016)"
        );
    }
    conn.close().await?;
    // Verify with a fresh pool exactly as the runtime would (owner identity here).
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect(&args.database_url)
        .await?;
    let report = ledger_store::schema::verify(&pool).await?;
    println!(
        "schema at {:04} (required {:04})",
        report.version,
        ledger_store::schema::REQUIRED_SCHEMA_VERSION
    );
    Ok(())
}

fn parse_graph_create(mut argv: impl Iterator<Item = String>) -> Result<GraphArgs, String> {
    let mut database_url = database_url_from_env();
    let mut graph = None;
    let mut tenant = None;
    let mut status = GraphStatus::Active;
    let mut knowledge_base = None;
    let mut purpose = None;
    while let Some(flag) = argv.next() {
        match flag.as_str() {
            "--graph" => graph = Some(value(&mut argv, "--graph")?),
            "--tenant" => tenant = Some(value(&mut argv, "--tenant")?),
            "--database-url" => database_url = Some(value(&mut argv, "--database-url")?),
            "--kb" => knowledge_base = Some(value(&mut argv, "--kb")?),
            "--purpose" => purpose = Some(value(&mut argv, "--purpose")?),
            "--status" => {
                status = match value(&mut argv, "--status")?.as_str() {
                    "active" => GraphStatus::Active,
                    "importing" => GraphStatus::Importing,
                    // `bootstrap` is reserved for the pre-v2 topology and `archived` is a
                    // lifecycle transition, not a creation state.
                    _ => return Err(format!("--status must be active or importing\n{}", usage())),
                }
            }
            other if other.starts_with("--") => {
                return Err(format!("unknown flag {other}\n{}", usage()));
            }
            _ => {
                return Err(format!(
                    "unexpected positional argument (value not shown)\n{}",
                    usage()
                ));
            }
        }
    }
    Ok(GraphArgs {
        database_url: database_url.ok_or_else(|| {
            format!(
                "--database-url or LEDGER_MIGRATION_DATABASE_URL is required\n{}",
                usage()
            )
        })?,
        graph: graph.ok_or_else(|| format!("--graph is required\n{}", usage()))?,
        tenant: tenant.ok_or_else(|| format!("--tenant is required\n{}", usage()))?,
        status,
        knowledge_base,
        purpose,
    })
}

fn parse_command(mut argv: impl Iterator<Item = String>) -> Result<Command, String> {
    match argv.next().as_deref() {
        Some("migrate") => parse_migrate(argv).map(Command::Migrate),
        Some("migrate-fs-to-pg") => parse(argv).map(Command::MigrateFs),
        Some("graph") => match argv.next().as_deref() {
            Some("create") => parse_graph_create(argv).map(Command::CreateGraph),
            _ => Err(usage().into()),
        },
        _ => Err(usage().into()),
    }
}

async fn create_graph(args: &GraphArgs) -> Result<(), Box<dyn std::error::Error>> {
    let graph_id = GraphId::new(args.graph.clone())?;
    let tenant_id = TenantId::new(args.tenant.clone())?;
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect(&args.database_url)
        .await?;
    // Provisioning never migrates; the schema must already be at the required level.
    ledger_store::schema::verify(&pool).await?;
    PgGraphs::new(pool)
        .create(&NewGraph {
            graph_id: graph_id.clone(),
            tenant_id: tenant_id.clone(),
            knowledge_base_id: args.knowledge_base.clone(),
            purpose: args.purpose.clone(),
            status: args.status,
        })
        .await?;
    println!(
        "created graph {} for tenant {} with status {}",
        graph_id,
        tenant_id,
        args.status.as_str()
    );
    Ok(())
}

/// Take the value for `flag`, refusing a missing value or another flag in its place.
fn value(argv: &mut impl Iterator<Item = String>, flag: &str) -> Result<String, String> {
    match argv.next() {
        Some(v) if !v.starts_with("--") && !v.is_empty() => Ok(v),
        _ => Err(format!("{flag} needs a value\n{}", usage())),
    }
}

fn parse(mut argv: impl Iterator<Item = String>) -> Result<Args, String> {
    let mut source = None;
    let mut database_url = database_url_from_env();
    let mut graph = "default".to_owned();
    let mut branch = "main".to_owned();
    let mut json = false;
    let mut runtime_role = None;
    let mut position = 0usize;
    while let Some(flag) = argv.next() {
        position += 1;
        match flag.as_str() {
            "--source" => source = Some(value(&mut argv, "--source")?),
            "--database-url" => database_url = Some(value(&mut argv, "--database-url")?),
            "--graph" => graph = value(&mut argv, "--graph")?,
            "--branch" => branch = value(&mut argv, "--branch")?,
            "--runtime-role" => {
                runtime_role = Some(role_name(value(&mut argv, "--runtime-role")?)?)
            }
            "--json" => json = true,
            other if other.starts_with("--") => {
                return Err(format!("unknown flag {other}\n{}", usage()));
            }
            _ => {
                // Never echo a stray value: it may be a misplaced database URL.
                return Err(format!(
                    "unexpected positional argument at position {position} (value not shown)\n{}",
                    usage()
                ));
            }
        }
    }
    Ok(Args {
        source: source.ok_or_else(|| format!("--source is required\n{}", usage()))?,
        database_url: database_url.ok_or_else(|| {
            format!(
                "--database-url or LEDGER_MIGRATION_DATABASE_URL is required\n{}",
                usage()
            )
        })?,
        graph,
        branch,
        json,
        runtime_role,
    })
}

#[tokio::main]
async fn main() -> ExitCode {
    let command = match parse_command(env::args().skip(1)) {
        Ok(command) => command,
        Err(message) => {
            eprintln!("{message}");
            return ExitCode::from(2);
        }
    };
    let args = match command {
        Command::Migrate(args) => {
            return match migrate_schema(&args).await {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("migrate failed: {e}");
                    eprintln!(
                        "the schema was left at its previous level or at the last successfully \
                         applied migration; re-run after fixing the cause"
                    );
                    ExitCode::FAILURE
                }
            };
        }
        Command::CreateGraph(args) => {
            return match create_graph(&args).await {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("graph create failed: {e}");
                    ExitCode::FAILURE
                }
            };
        }
        Command::MigrateFs(args) => args,
    };
    match migrate(&args).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("migration aborted: {e}");
            eprintln!(
                "no destination ref was moved to unverified content; re-run after fixing the \
                 cause (the run is idempotent)"
            );
            ExitCode::FAILURE
        }
    }
}

async fn migrate(args: &Args) -> Result<(), Box<dyn std::error::Error>> {
    let graph = GraphId::new(args.graph.clone())?;
    // Read-only: a mistyped source path is an error, never a freshly created empty store.
    let source = FileStore::open_existing(&args.source)?;
    // Schema order matters for a legacy database whose shared ref already points at
    // filesystem-only content: bring the schema to the content level (0005), import and
    // verify the content, and only then apply the workflow schema (0006), whose guard
    // requires every ref head to be an indexed commit of its graph.
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(8)
        .connect(&args.database_url)
        .await?;
    ledger_store::schema::migrate_up_to(&pool, ledger_store::schema::CONTENT_SCHEMA_VERSION)
        .await?;
    let destination =
        PostgresImmutableStore::from_pool_migrated(pool.clone(), V1Binding::BindTo(graph));
    let refs = PgRefStore::with_ref_migrated(pool.clone(), &args.graph, &args.branch);
    let report = FsToPgMigration::new(source, destination, refs)?
        .run()
        .await?;
    ledger_store::schema::migrate_all(&pool).await?;
    if let Some(role) = &args.runtime_role {
        use sqlx::Connection;
        let mut conn = sqlx::postgres::PgConnection::connect(&args.database_url).await?;
        ledger_store::schema::grant_runtime_role(&mut conn, role).await?;
        conn.close().await?;
    } else {
        eprintln!(
            "note: no --runtime-role given; run `ledger-admin migrate --runtime-role <role>` before \
             starting the server (ADR-0016)"
        );
    }
    if args.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        println!("outcome:             {:?}", report.outcome);
        println!(
            "graph / branch:      {} / {}",
            report.graph_id, report.branch
        );
        println!("source objects:      {}", report.source_objects);
        println!("content objects:     {}", report.content_objects);
        println!("commits:             {}", report.commit_objects);
        println!("index rows verified: {}", report.commit_index_rows_verified);
        println!(
            "source head:         {}",
            report.source_head.as_deref().unwrap_or("(none)")
        );
        println!(
            "dest head before:    {}",
            report
                .destination_head_before
                .as_deref()
                .unwrap_or("(none)")
        );
        println!(
            "dest head after:     {}",
            report.destination_head_after.as_deref().unwrap_or("(none)")
        );
        if let Some(digest) = &report.head_state_digest {
            println!(
                "head state:          {} quads, {digest}",
                report.head_state_quads.unwrap_or(0)
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::role_name;

    #[test]
    fn runtime_role_must_be_a_plain_identifier() {
        assert_eq!(
            role_name("ledger_runtime".into()).unwrap(),
            "ledger_runtime"
        );
        assert_eq!(role_name("_rt9".into()).unwrap(), "_rt9");
        for bad in [
            "Ledger",
            "9start",
            "with-dash",
            "postgres://user:secret@host/db",
            "",
            &"a".repeat(64),
            "role;drop",
        ] {
            let err = role_name(bad.into()).unwrap_err();
            assert!(!err.contains("secret"), "must never echo the value");
        }
    }
}
