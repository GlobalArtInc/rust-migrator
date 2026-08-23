//! The hand-run side: what is left once the services apply their own migrations
//! on the way up.

use sea_orm::DatabaseConnection;

use crate::error::{Error, Result};
use crate::{Checksum, Migrations};

const USAGE: &str = "\
migrator <command>

  status                  what the binary carries and what the database has (default)
  up [--to NAME]          apply everything pending, or stop after NAME
  down [--steps N]        take the last migration back, or the last N
  mark [NAME]             record as applied without running - for a schema that already has it
  unmark NAME             drop the ledger row without running the down
  verify                  report files edited after the database ran them
  create SLUG             write a new .up.sql / .down.sql pair into the checkout

  --dry-run               with up or down: print the sql instead of running it
";

pub enum Command {
    Status,
    Up { target: Option<String>, dry_run: bool },
    Down { steps: usize, dry_run: bool },
    Mark { target: Option<String> },
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
            target: positional.first().cloned(),
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
                println!("{}", path.display());
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

                match migrations.apply_through(db, target.as_deref()).await?.len() {
                    0 => println!("nothing to apply"),
                    applied => println!("applied {applied}"),
                }

                Ok(())
            }
            Command::Down { steps, dry_run } => {
                if *dry_run {
                    return plan_down(migrations, db, *steps).await;
                }

                for name in migrations.revert(db, *steps).await? {
                    println!("reverted {name}");
                }

                Ok(())
            }
            Command::Mark { target } => {
                for name in migrations.mark(db, target.as_deref()).await? {
                    println!("marked {name}");
                }

                Ok(())
            }
            Command::Unmark { name } => {
                migrations.unmark(db, name).await?;
                println!("forgot {name}");

                Ok(())
            }
            Command::Create { .. } | Command::Help => Ok(()),
        }
    }
}

async fn status(migrations: &Migrations, db: &DatabaseConnection) -> Result<()> {
    let status = migrations.status(db).await?;

    for entry in &status.entries {
        let state = if entry.applied { "applied" } else { "pending" };
        let mut notes = Vec::new();
        if entry.checksum == Checksum::Drift {
            notes.push("edited since it ran");
        }
        if entry.checksum == Checksum::Unrecorded {
            notes.push("applied before this tool");
        }
        if !entry.reversible {
            notes.push("no down");
        }

        match notes.is_empty() {
            true => println!("{state}  {}", entry.name),
            false => println!("{state}  {}  ({})", entry.name, notes.join(", ")),
        }
    }

    if !status.unknown.is_empty() {
        println!("\nrecorded in the database but not carried by this build:");
        for name in &status.unknown {
            println!("  {name}");
        }
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
        println!("every applied migration is the file that ran");
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

        println!("-- {}", migration.name);
        println!("{}", migration.up.trim_end());

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

        println!("-- {}", migration.name);
        println!("{}", down.trim_end());
    }

    Ok(())
}
