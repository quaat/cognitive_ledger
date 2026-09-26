//! Administrative entry points that must never run inside request handling.
//!
//! ```text
//! LEDGER_DATABASE_URL=postgres://… ledger-admin migrate-fs-to-pg --source <dir> \
//!     [--graph default] [--branch main] [--json]
//! ```
//! The database URL carries credentials: prefer the environment variable. `--database-url`
//! is accepted for scripted use but exposes the URL in process listings. No argument value
//! is ever echoed back, so a misplaced URL cannot leak through an error message.

use ledger_core::GraphId;
use ledger_store::{FileStore, FsToPgMigration, PgRefStore, PostgresImmutableStore, V1Binding};
use std::{env, process::ExitCode};

struct Args {
    source: String,
    database_url: String,
    graph: String,
    branch: String,
    json: bool,
}

fn usage() -> &'static str {
    "usage: ledger-admin migrate-fs-to-pg --source <dir> [--database-url <url>] \
     [--graph <graph_id>] [--branch <name>] [--json]\n\
     (database url defaults to $LEDGER_DATABASE_URL, which is preferred; argument values \
     are never echoed)"
}

/// Take the value for `flag`, refusing a missing value or another flag in its place.
fn value(argv: &mut impl Iterator<Item = String>, flag: &str) -> Result<String, String> {
    match argv.next() {
        Some(v) if !v.starts_with("--") && !v.is_empty() => Ok(v),
        _ => Err(format!("{flag} needs a value\n{}", usage())),
    }
}

fn parse(mut argv: impl Iterator<Item = String>) -> Result<Args, String> {
    match argv.next().as_deref() {
        Some("migrate-fs-to-pg") => {}
        _ => return Err(usage().into()),
    }
    let mut source = None;
    let mut database_url = env::var("LEDGER_DATABASE_URL")
        .ok()
        .filter(|u| !u.is_empty());
    let mut graph = "default".to_owned();
    let mut branch = "main".to_owned();
    let mut json = false;
    let mut position = 0usize;
    while let Some(flag) = argv.next() {
        position += 1;
        match flag.as_str() {
            "--source" => source = Some(value(&mut argv, "--source")?),
            "--database-url" => database_url = Some(value(&mut argv, "--database-url")?),
            "--graph" => graph = value(&mut argv, "--graph")?,
            "--branch" => branch = value(&mut argv, "--branch")?,
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
                "--database-url or LEDGER_DATABASE_URL is required\n{}",
                usage()
            )
        })?,
        graph,
        branch,
        json,
    })
}

#[tokio::main]
async fn main() -> ExitCode {
    let args = match parse(env::args().skip(1)) {
        Ok(args) => args,
        Err(message) => {
            eprintln!("{message}");
            return ExitCode::from(2);
        }
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
