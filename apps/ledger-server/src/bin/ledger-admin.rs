//! Administrative entry points that must never run inside request handling.
//!
//! ```text
//! LEDGER_MIGRATION_DATABASE_URL=postgres://… ledger-admin migrate [--runtime-role <name>] \
//!     [--projector-role <name>]
//! LEDGER_MIGRATION_DATABASE_URL=postgres://… ledger-admin projection enable|disable \
//!     --graph <id> --target <target_id> [--ref main]
//! LEDGER_MIGRATION_DATABASE_URL=postgres://… ledger-admin projection status [--target <id>] [--json]
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
    /// Container health probe: GET a local URL and exit 0 on 2xx (the runtime image has no
    /// shell or curl).
    Probe(String),
    /// Database invariant verification (Plan 0005 §20): inspect only, exit 1 on violations.
    Verify {
        database_url: String,
        json: bool,
    },
    /// Projection stream administration (ADR-0021): owner identity, database only.
    Projection(ProjectionArgs),
}

enum ProjectionAction {
    Enable,
    Disable,
    Status,
}

struct ProjectionArgs {
    action: ProjectionAction,
    database_url: String,
    graph: Option<String>,
    target: Option<String>,
    branch: String,
    json: bool,
    /// `disable` without the target fence (escape hatch; ADR-0021).
    unfenced: bool,
}

struct MigrateArgs {
    database_url: String,
    runtime_role: Option<String>,
    projector_role: Option<String>,
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
    "usage: ledger-admin migrate [--runtime-role <role>] [--projector-role <role>] [--database-url <url>]\n\
     \x20      ledger-admin projection enable|disable --graph <graph_id> --target <target_id> \
     [--ref <name>] [--unfenced] [--database-url <url>]\n\
     \x20      ledger-admin projection status [--target <target_id>] [--json] [--database-url <url>]\n\
     \x20      ledger-admin migrate-fs-to-pg --source <dir> [--database-url <url>] \
     [--graph <graph_id>] [--branch <name>] [--runtime-role <role>] [--json]\n\
     \x20      ledger-admin verify [--database-url <url>] [--json]\n\
     \x20      ledger-admin probe http://127.0.0.1:8080/ready\n\
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

/// The probe target must be plain HTTP to loopback: scheme `http`, no userinfo, host a
/// loopback IPv4 address, `::1` or exactly `localhost`. Parsed rather than prefix-matched,
/// so `http://localhost.evil.com`, `http://127.0.0.1@evil.com` and similar are refused. The
/// value is never echoed because a pasted URL may carry credentials.
fn probe_url(raw: &str) -> Result<String, String> {
    let url = url::Url::parse(raw).map_err(|_| "probe: URL does not parse (value not shown)")?;
    if url.scheme() != "http" {
        return Err("probe: only plain http to loopback is supported".into());
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err("probe: URL must not carry credentials".into());
    }
    let loopback = match url.host() {
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        Some(url::Host::Domain(name)) => name.eq_ignore_ascii_case("localhost"),
        None => false,
    };
    if !loopback {
        return Err("probe: host must be a loopback address or localhost".into());
    }
    Ok(url.into())
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
    let mut projector_role = None;
    while let Some(flag) = argv.next() {
        match flag.as_str() {
            "--database-url" => database_url = Some(value(&mut argv, "--database-url")?),
            "--runtime-role" => {
                runtime_role = Some(role_name(value(&mut argv, "--runtime-role")?)?)
            }
            "--projector-role" => {
                projector_role = Some(role_name(value(&mut argv, "--projector-role")?)?)
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
    if runtime_role.is_some() && runtime_role == projector_role {
        return Err(
            "--runtime-role and --projector-role must name distinct roles (ADR-0021)".into(),
        );
    }
    Ok(MigrateArgs {
        projector_role,
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
    if let Some(role) = &args.projector_role {
        ledger_store::schema::grant_projector_role(&mut conn, role).await?;
        println!("granted projector privileges to role {role} (ledger_grant_projector)");
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
        Some("verify") => {
            let mut database_url = database_url_from_env();
            let mut json = false;
            while let Some(flag) = argv.next() {
                match flag.as_str() {
                    "--database-url" => database_url = Some(value(&mut argv, "--database-url")?),
                    "--json" => json = true,
                    other => return Err(format!("unknown flag {other}\n{}", usage())),
                }
            }
            Ok(Command::Verify {
                database_url: database_url.ok_or_else(|| {
                    format!(
                        "--database-url or LEDGER_MIGRATION_DATABASE_URL is required\n{}",
                        usage()
                    )
                })?,
                json,
            })
        }
        Some("probe") => match (argv.next(), argv.next()) {
            (Some(url), None) => probe_url(&url).map(Command::Probe),
            _ => Err(
                "usage: ledger-admin probe http://127.0.0.1:<port>/ready (loopback only)".into(),
            ),
        },
        Some("migrate-fs-to-pg") => parse(argv).map(Command::MigrateFs),
        Some("projection") => parse_projection(argv).map(Command::Projection),
        Some("graph") => match argv.next().as_deref() {
            Some("create") => parse_graph_create(argv).map(Command::CreateGraph),
            _ => Err(usage().into()),
        },
        _ => Err(usage().into()),
    }
}

fn parse_projection(mut argv: impl Iterator<Item = String>) -> Result<ProjectionArgs, String> {
    let action = match argv.next().as_deref() {
        Some("enable") => ProjectionAction::Enable,
        Some("disable") => ProjectionAction::Disable,
        Some("status") => ProjectionAction::Status,
        _ => return Err(usage().into()),
    };
    let mut database_url = database_url_from_env();
    let (mut graph, mut target, mut branch, mut json) = (None, None, "main".to_owned(), false);
    let mut unfenced = false;
    while let Some(flag) = argv.next() {
        match flag.as_str() {
            "--database-url" => database_url = Some(value(&mut argv, "--database-url")?),
            "--graph" => graph = Some(value(&mut argv, "--graph")?),
            "--target" => target = Some(value(&mut argv, "--target")?),
            "--ref" => branch = value(&mut argv, "--ref")?,
            "--json" => json = true,
            "--unfenced" if matches!(action, ProjectionAction::Disable) => unfenced = true,
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
    if !matches!(action, ProjectionAction::Status) && (graph.is_none() || target.is_none()) {
        return Err(format!("--graph and --target are required\n{}", usage()));
    }
    Ok(ProjectionArgs {
        action,
        database_url: database_url.ok_or_else(|| {
            format!(
                "--database-url or LEDGER_MIGRATION_DATABASE_URL is required\n{}",
                usage()
            )
        })?,
        graph,
        target,
        branch,
        json,
        unfenced,
    })
}

async fn projection(args: &ProjectionArgs) -> Result<(), Box<dyn std::error::Error>> {
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect(&args.database_url)
        .await?;
    ledger_store::schema::verify(&pool).await?;
    let repo = ledger_store::ProjectionRepository::new(pool);
    let key = || -> Result<ledger_store::StreamKey, Box<dyn std::error::Error>> {
        Ok(ledger_store::StreamKey {
            graph_id: GraphId::new(args.graph.clone().unwrap_or_default())?,
            branch: args.branch.clone(),
            target_id: args.target.clone().unwrap_or_default(),
        })
    };
    match args.action {
        ProjectionAction::Enable => {
            let key = key()?;
            let graph = repo
                .enable(&key, |kb| {
                    ledger_projection::CognitiveGraph::for_knowledge_base(kb)
                        .map(|g| g.as_iri().to_owned())
                        .map_err(|e| e.to_string())
                })
                .await?;
            println!(
                "projection enabled: graph {} ref {} -> target {} cognitive graph <{graph}>",
                key.graph_id, key.branch, key.target_id
            );
        }
        ProjectionAction::Disable => {
            let key = key()?;
            if args.unfenced {
                if !repo.disable_unfenced(&key).await? {
                    return Err("no such projection stream".into());
                }
                println!(
                    "projection disabled WITHOUT fencing the target: graph {} ref {} target {} \
                     (a write of this stream still in flight could land after another stream \
                     takes its cognitive graph; ADR-0021)",
                    key.graph_id, key.branch, key.target_id
                );
            } else {
                if !repo.disable(&key).await? {
                    return Err("no such projection stream".into());
                }
                println!(
                    "projection disabling: graph {} ref {} target {} — a projector fences the \
                     target, then the stream is disabled (check `projection status`)",
                    key.graph_id, key.branch, key.target_id
                );
            }
        }
        ProjectionAction::Status => {
            let streams = repo.status(args.target.as_deref()).await?;
            let unconfigured = repo.unconfigured_pending().await?;
            if args.json {
                let rows: Vec<serde_json::Value> = streams
                    .iter()
                    .map(|s| {
                        serde_json::json!({
                            "graph_id": s.key.graph_id.as_str(),
                            "ref": s.key.branch,
                            "target_id": s.key.target_id,
                            "cognitive_graph": s.cognitive_graph,
                            "status": s.status,
                            "projected_commit": s.projected_commit,
                            "projected_ref_version": s.projected_ref_version,
                            "head_commit": s.head_commit,
                            "head_version": s.head_version,
                            "lag_versions": s.lag_versions(),
                            "pending_events": s.pending_events,
                            "oldest_pending_seconds": s.oldest_pending_seconds,
                            "last_success_seconds_ago": s.last_success_seconds_ago,
                            "last_error_code": s.last_error_code,
                            "consecutive_failures": s.consecutive_failures,
                            "rebuilds": s.rebuilds,
                            "leased": s.leased,
                        })
                    })
                    .collect();
                println!(
                    "{}",
                    serde_json::json!({"streams": rows, "unconfigured_pending_events": unconfigured})
                );
            } else {
                for s in &streams {
                    println!(
                        "{} {} -> {} [{}] projected v{} of head v{} (lag {}, {} pending{}){}",
                        s.key.graph_id,
                        s.key.branch,
                        s.key.target_id,
                        s.status,
                        s.projected_ref_version.unwrap_or(0),
                        s.head_version.unwrap_or(0),
                        s.lag_versions(),
                        s.pending_events,
                        s.oldest_pending_seconds
                            .map(|a| format!(", oldest {a:.0}s"))
                            .unwrap_or_default(),
                        s.last_error_code
                            .as_ref()
                            .map(|c| format!(" last error {c}"))
                            .unwrap_or_default()
                    );
                }
                println!("outbox events without an enabled stream: {unconfigured}");
            }
        }
    }
    Ok(())
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
    if ledger_store::TEST_HOOKS_COMPILED {
        eprintln!(
            "ledger-admin: this build contains ledger-store's test-only fault injection \
             (`test-hooks`); refusing to run"
        );
        return ExitCode::FAILURE;
    }
    let command = match parse_command(env::args().skip(1)) {
        Ok(command) => command,
        Err(message) => {
            eprintln!("{message}");
            // Usage errors exit 1: Docker reserves healthcheck status 2.
            return ExitCode::FAILURE;
        }
    };
    let args = match command {
        Command::Probe(url) => {
            // Direct loopback only: no proxy, no redirects, bounded well inside the
            // container healthcheck's own timeout. The URL is not echoed.
            let client = reqwest::Client::builder()
                .no_proxy()
                .connect_timeout(std::time::Duration::from_millis(1500))
                .timeout(std::time::Duration::from_secs(2))
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .expect("static client configuration");
            return match client.get(&url).send().await {
                Ok(response) if response.status().is_success() => ExitCode::SUCCESS,
                Ok(response) => {
                    eprintln!("probe: HTTP {}", response.status().as_u16());
                    ExitCode::FAILURE
                }
                Err(e) => {
                    eprintln!(
                        "probe: {}",
                        if e.is_timeout() {
                            "timeout"
                        } else {
                            "connection failed"
                        }
                    );
                    ExitCode::FAILURE
                }
            };
        }
        Command::Verify { database_url, json } => {
            let pool = match sqlx::postgres::PgPoolOptions::new()
                .max_connections(2)
                .connect(&database_url)
                .await
            {
                Ok(pool) => pool,
                Err(e) => {
                    eprintln!("verify: cannot connect: {e}");
                    return ExitCode::FAILURE;
                }
            };
            return match ledger_store::verify::run(&pool).await {
                Ok(report) => {
                    if json {
                        let checks: Vec<serde_json::Value> = report
                            .checks
                            .iter()
                            .map(|c| serde_json::json!({"check": c.name, "violations": c.violations, "sample": c.sample}))
                            .collect();
                        let counts: serde_json::Map<String, serde_json::Value> = report
                            .counts
                            .iter()
                            .map(|(t, n)| ((*t).to_owned(), serde_json::json!(n)))
                            .collect();
                        println!(
                            "{}",
                            serde_json::json!({"clean": report.is_clean(), "checks": checks, "counts": counts})
                        );
                    } else {
                        for c in &report.checks {
                            println!(
                                "{} {} ({} violation(s)){}",
                                if c.violations == 0 { "ok  " } else { "FAIL" },
                                c.name,
                                c.violations,
                                if c.sample.is_empty() {
                                    String::new()
                                } else {
                                    format!(": {}", c.sample.join(", "))
                                }
                            );
                        }
                        for (t, n) in &report.counts {
                            println!("count {t} = {n}");
                        }
                        println!(
                            "{}",
                            if report.is_clean() {
                                "VERIFY OK"
                            } else {
                                "VERIFY FAILED"
                            }
                        );
                    }
                    if report.is_clean() {
                        ExitCode::SUCCESS
                    } else {
                        ExitCode::FAILURE
                    }
                }
                Err(e) => {
                    eprintln!("verify failed to run: {e}");
                    ExitCode::FAILURE
                }
            };
        }
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
        Command::Projection(args) => {
            return match projection(&args).await {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("projection command failed: {e}");
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
    use super::{probe_url, role_name};

    #[test]
    fn probe_accepts_only_plain_http_loopback_without_credentials() {
        for ok in [
            "http://127.0.0.1:8080/ready",
            "http://localhost:8080/ready",
            "http://LOCALHOST/ready",
            "http://[::1]:8080/ready",
            "http://127.9.9.9/health",
        ] {
            assert!(probe_url(ok).is_ok(), "{ok}");
        }
        for bad in [
            "http://localhost.evil.com/",
            "http://127.0.0.1.nip.io/",
            "http://127.0.0.1@evil.com/",
            "http://localhost:80@evil.com/",
            "http://user:secret@127.0.0.1/ready",
            "https://127.0.0.1/ready",
            "http://10.0.0.1/ready",
            "http://[::2]/ready",
            "ftp://127.0.0.1/",
            "not a url",
        ] {
            let err = probe_url(bad).unwrap_err();
            assert!(
                !err.contains("secret") && !err.contains("evil"),
                "{bad}: {err}"
            );
        }
    }

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
