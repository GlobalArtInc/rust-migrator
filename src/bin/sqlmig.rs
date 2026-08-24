//! The migrator on its own, for the image that carries no schema of its own.
//!
//! One image runs every schema: the files come off a mount rather than from
//! inside the binary, so the same tag migrates ownlate and wfs and whatever is
//! written next.
//!
//!   MIGRATIONS_DIR    where the .up.sql / .down.sql pairs are (/migrations)
//!   MIGRATIONS_TABLE  the ledger (migrations)
//!   DATABASE_URL      the whole connection, or the DB_* below
//!   DB_HOST DB_PORT DB_USER DB_PASS|DB_PASSWORD DB_NAME|DB_DATABASE DB_SCHEMA DB_SSL_MODE

use std::env::var;
use std::process::ExitCode;

use sea_orm::{ConnectOptions, Database, DatabaseConnection};
use sqlmig::Migrations;

type Failure = Box<dyn std::error::Error>;

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .json()
        .flatten_event(true)
        .with_current_span(false)
        .with_span_list(false)
        .with_target(true)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    sqlmig::cli::report(run().await)
}

async fn run() -> Result<(), Failure> {
    let command = sqlmig::cli::command()?;

    let directory = var("MIGRATIONS_DIR").unwrap_or_else(|_| "/migrations".to_owned());
    let mut migrations = Migrations::from_dir(&directory);
    if let Ok(table) = var("MIGRATIONS_TABLE") {
        // the process runs once and exits; a leaked name costs nothing and keeps
        // the builder the same one a service uses with a literal
        migrations = migrations.table(Box::leak(table.into_boxed_str()));
    }

    let db = match command.needs_database() {
        true => Some(connect().await?),
        false => None,
    };

    tracing::info!(
        migrations = directory,
        ledger = migrations.ledger(),
        carried = migrations.all()?.len(),
        "starting"
    );

    command.run(&migrations, db.as_ref()).await?;

    Ok(())
}

async fn connect() -> Result<DatabaseConnection, Failure> {
    let mut options = ConnectOptions::new(url()?);
    options.sqlx_logging(false).max_connections(2);

    Ok(Database::connect(options).await?)
}

/// `DATABASE_URL` if it is there, otherwise the pieces. The names differ between
/// the services this has to run for, so both spellings of the two that differ
/// are read.
fn url() -> Result<String, Failure> {
    if let Ok(url) = var("DATABASE_URL") {
        return Ok(url);
    }

    let either = |first: &str, second: &str| var(first).or_else(|_| var(second));
    let host = var("DB_HOST").map_err(|_| "neither DATABASE_URL nor DB_HOST is set")?;
    let port = var("DB_PORT").unwrap_or_else(|_| "5432".to_owned());
    let user = var("DB_USER").map_err(|_| "DB_USER is not set")?;
    let password = either("DB_PASS", "DB_PASSWORD").map_err(|_| "neither DB_PASS nor DB_PASSWORD is set")?;
    let name = either("DB_NAME", "DB_DATABASE").map_err(|_| "neither DB_NAME nor DB_DATABASE is set")?;

    let mut url = format!(
        "postgres://{}:{}@{host}:{port}/{}",
        encode(&user),
        encode(&password),
        encode(&name)
    );

    let mut query = Vec::new();
    if let Ok(mode) = var("DB_SSL_MODE") {
        query.push(format!("sslmode={mode}"));
    }
    if let Ok(schema) = var("DB_SCHEMA") {
        query.push(format!("options=-c%20search_path%3D{}", encode(&schema)));
    }
    if !query.is_empty() {
        url.push('?');
        url.push_str(&query.join("&"));
    }

    Ok(url)
}

/// Passwords carry punctuation, and a bare `@` or `/` in one turns the url into
/// a different url.
fn encode(value: &str) -> String {
    value
        .bytes()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (byte as char).to_string(),
            other => format!("%{other:02X}"),
        })
        .collect()
}
