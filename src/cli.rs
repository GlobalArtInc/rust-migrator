//! The hand-run side: what is left once the services apply their own migrations
//! on the way up.

use std::process::ExitCode;

use sea_orm::DatabaseConnection;

use crate::error::{Error, Result};
use crate::{Checksum, Migrations};

const USAGE: &str = "\
migrator <command>

  status                  what the binary carries and what the database has (default)
  up [--to NAME]          apply everything pending, or stop after NAME
  down [--steps N]        take the last migration back, or the last N
  mark [NAME]             record as applied without running - for a schema that already has it
  mark --to NAME          the same, for everything pending up to and including NAME
  unmark NAME             drop the ledger row without running the down
  verify                  report files edited after the database ran them
  create SLUG             write a new .up.sql / .down.sql pair into the checkout

  --dry-run               with up or down: print the sql instead of running it
";

/// The command line owns its strings; `Marking` borrows them.
pub enum Marking {
    All,
    One(String),
    Through(String),
}

impl Marking {
    fn borrowed(&self) -> crate::Marking<'_> {
        match self {
            Marking::All => crate::Marking::All,
            Marking::One(name) => crate::Marking::One(name),
            Marking::Through(name) => crate::Marking::Through(name),
        }
    }
}

pub enum Command {
    Status,
    Up { target: Option<String>, dry_run: bool },
    Down { steps: usize, dry_run: bool },
    Mark { marking: Marking },
    Unmark { name: String },
    Verify,
    Create { slug: String },
    Help,
}

/// Reads the command line.
pub fn command() -> Result<Command> {
    parse(std::env::args().skip(1))
}

pub fn parse(args: impl IntoIterator<Item = String>) -> Result<Command> {
    let mut args = args.into_iter().peekable();
    let head = args.next().unwrap_or_else(|| "status".to_owned());

    let mut dry_run = false;
    let mut target = None;
    let mut steps = None;
    let mut positional = Vec::new();

    while let Some(argument) = args.next() {
        match argument.as_str() {
            "--dry-run" => dry_run = true,
            "--to" => target = Some(args.next().ok_or_else(|| Error::usage("--to wants a name"))?),
            "--steps" => {
                let value = args.next().ok_or_else(|| Error::usage("--steps wants a number"))?;
                steps = Some(
                    value
                        .parse()
                        .map_err(|_| Error::usage(format!("{value} is not a number")))?,
                );
            }
            "--all" => {}
            other if other.starts_with('-') => return Err(Error::usage(format!("unknown option {other}"))),
            other => positional.push(other.to_owned()),
        }
    }

    Ok(match head.as_str() {
        "status" | "show" => Command::Status,
        "up" => Command::Up {
            target: target.or_else(|| positional.first().cloned()),
            dry_run,
        },
        "down" => Command::Down {
            steps: steps.unwrap_or(1),
            dry_run,
        },
        "mark" => Command::Mark {
            marking: match (target, positional.first().cloned()) {
                (Some(through), _) => Marking::Through(through),
                (None, Some(one)) => Marking::One(one),
                (None, None) => Marking::All,
            },
        },
        "unmark" => Command::Unmark {
            name: positional
                .first()
                .cloned()
                .ok_or_else(|| Error::usage("unmark wants the name of a migration"))?,
        },
        "verify" => Command::Verify,
        "create" | "new" => Command::Create {
            slug: positional
                .first()
                .cloned()
                .ok_or_else(|| Error::usage("create wants a slug, such as add-billing-plan"))?,
        },
        "help" | "--help" | "-h" => Command::Help,
        other => return Err(Error::usage(format!("unknown command {other}\n\n{USAGE}"))),
    })
}

impl Command {
    /// `create` and `help` are answered without one.
    pub fn needs_database(&self) -> bool {
        !matches!(self, Command::Create { .. } | Command::Help)
    }

    pub async fn run(&self, migrations: &Migrations, db: Option<&DatabaseConnection>) -> Result<()> {
        if let Command::Help = self {
            print!("{USAGE}");
            return Ok(());
        }

        if let Command::Create { slug } = self {
            let stamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_err(|_| Error::usage("the clock is behind the epoch"))?
                .as_millis() as i64;

            for path in migrations.create(slug, stamp)? {
                tracing::info!(path = %path.display(), "wrote a migration");
            }

            return Ok(());
        }

        let db = db.ok_or_else(|| Error::usage("this command needs a database connection"))?;

        match self {
            Command::Status => status(migrations, db).await,
            Command::Verify => verify(migrations, db).await,
            Command::Up { target, dry_run } => {
                if *dry_run {
                    return plan(migrations, db, target.as_deref()).await;
                }

                // the library logs each one as it goes, and the count when it is done
                migrations.apply_through(db, target.as_deref()).await?;

                Ok(())
            }
            Command::Down { steps, dry_run } => {
                if *dry_run {
                    return plan_down(migrations, db, *steps).await;
                }

                migrations.revert(db, *steps).await?;

                Ok(())
            }
            Command::Mark { marking } => {
                migrations.mark(db, marking.borrowed()).await?;

                Ok(())
            }
            Command::Unmark { name } => {
                migrations.unmark(db, name).await?;
                tracing::info!(migration = name, "dropped the ledger row");

                Ok(())
            }
            Command::Create { .. } | Command::Help => Ok(()),
        }
    }
}

async fn status(migrations: &Migrations, db: &DatabaseConnection) -> Result<()> {
    let status = migrations.status(db).await?;

    for entry in &status.entries {
        tracing::info!(
            migration = entry.name,
            state = if entry.applied { "applied" } else { "pending" },
            checksum = match entry.checksum {
                Checksum::Match => "match",
                Checksum::Drift => "edited since it ran",
                Checksum::Unrecorded => "applied before this tool",
                Checksum::NotApplied => "not applied",
            },
            reversible = entry.reversible,
            "migration"
        );
    }

    for name in &status.unknown {
        tracing::warn!(
            migration = name,
            "recorded in the database but not carried by this build"
        );
    }

    Ok(())
}

async fn verify(migrations: &Migrations, db: &DatabaseConnection) -> Result<()> {
    let status = migrations.status(db).await?;
    let drifted: Vec<&str> = status
        .entries
        .iter()
        .filter(|entry| entry.checksum == Checksum::Drift)
        .map(|entry| entry.name.as_str())
        .collect();

    if drifted.is_empty() {
        tracing::info!(
            checked = status.entries.len(),
            "every applied migration is the file that ran"
        );
        return Ok(());
    }

    Err(Error::usage(format!(
        "edited after the database ran them, so the schema is not what these files describe:\n  {}",
        drifted.join("\n  ")
    )))
}

async fn plan(migrations: &Migrations, db: &DatabaseConnection, target: Option<&str>) -> Result<()> {
    let status = migrations.status(db).await?;
    let pending: Vec<&str> = status
        .entries
        .iter()
        .filter(|entry| !entry.applied)
        .map(|entry| entry.name.as_str())
        .collect();

    for migration in migrations.all()? {
        if !pending.contains(&migration.name.as_str()) {
            continue;
        }

        tracing::info!(migration = migration.name, sql = migration.up.trim_end(), "would apply");

        if target == Some(migration.name.as_str()) {
            break;
        }
    }

    Ok(())
}

async fn plan_down(migrations: &Migrations, db: &DatabaseConnection, steps: usize) -> Result<()> {
    let status = migrations.status(db).await?;
    let carried = migrations.all()?;

    for entry in status.entries.iter().filter(|entry| entry.applied).rev().take(steps) {
        let Some(migration) = carried.iter().find(|migration| migration.name == entry.name) else {
            continue;
        };
        let Some(down) = migration.down else {
            return Err(Error::Irreversible {
                name: migration.name.clone(),
            });
        };

        tracing::info!(migration = migration.name, sql = down.trim_end(), "would revert");
    }

    Ok(())
}

/// Turns the outcome into an exit code, saying what went wrong through the same
/// subscriber as everything else: a job's output is read by a machine, and one
/// plain line in the middle of a stream of json is a line nobody sees.
pub fn report<E: std::fmt::Display>(result: std::result::Result<(), E>) -> ExitCode {
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            // the alternate form is what unwinds an anyhow chain; without it only
            // the outermost context survives and the cause is lost
            tracing::error!(error = whole(&format!("{error:#}")), "the migrator stopped");

            ExitCode::FAILURE
        }
    }
}

/// A database error carries its own source in its message, and unwinding the
/// chain on top of that says the same thing four times over. A part that has
/// already been said is dropped; the order of the rest is kept.
fn whole(chain: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();

    for part in chain.split(": ") {
        if !parts.contains(&part) {
            parts.push(part);
        }
    }

    parts.join(": ")
}

#[cfg(test)]
mod tests {
    use super::whole;

    #[test]
    fn a_cause_that_repeats_is_said_once() {
        // this is what sea-orm and anyhow together make of one missing table
        assert_eq!(
            whole(
                "Init failed: Execution Error: error returned from database: no such relation: \
                 Execution Error: error returned from database: no such relation: \
                 error returned from database: no such relation: no such relation"
            ),
            "Init failed: Execution Error: error returned from database: no such relation"
        );
    }

    #[test]
    fn a_chain_that_says_something_new_each_time_is_left_alone() {
        assert_eq!(
            whole("failed to connect: no route to host"),
            "failed to connect: no route to host"
        );
    }
}
