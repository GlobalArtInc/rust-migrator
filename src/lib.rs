//! SQL migrations for services that were node services first.
//!
//! The files travel inside the binary, so a service brings its own schema up
//! wherever it is deployed, and the ledger stays the table typeorm wrote - the
//! rows a rewritten service finds in production are the rows it keeps.

use std::future::Future;
use std::path::PathBuf;

use include_dir::Dir;
use sea_orm::{ConnectionTrait, DatabaseConnection, TransactionTrait};

mod discover;
mod error;
mod ledger;
mod scaffold;

pub mod cli;

pub use discover::Migration;
pub use error::{Error, Result};

#[doc(hidden)]
pub mod export {
    pub use include_dir::{self as include_dir_crate, include_dir};
}

/// Points the directory of `.up.sql` / `.down.sql` files at a database.
///
/// ```ignore
/// static MIGRATIONS: Migrations = migrator::embed!("$CARGO_MANIFEST_DIR/../../migrations");
/// ```
pub struct Migrations {
    dir: &'static Dir<'static>,
    source: &'static str,
    manifest: &'static str,
    table: &'static str,
    lock_key: i64,
    lock_wait: &'static str,
}

/// Every service on a database takes the same lock, so replicas starting together
/// migrate once: the first one through does the work, the rest wait and then find
/// nothing left to do.
const LOCK_KEY: i64 = 6_113_251_907_444_282_112;

/// Long enough for the slowest migration somebody else is running, short enough
/// that a lock nobody will release ends the start with a message instead of
/// leaving the service silent behind a port that never opens.
const LOCK_WAIT: &str = "150s";

impl Migrations {
    #[doc(hidden)]
    pub const fn new(dir: &'static Dir<'static>, source: &'static str, manifest: &'static str) -> Self {
        Self {
            dir,
            source,
            manifest,
            table: "migrations",
            lock_key: LOCK_KEY,
            lock_wait: LOCK_WAIT,
        }
    }

    /// The ledger table. `migrations` unless typeorm was told otherwise.
    pub const fn table(mut self, table: &'static str) -> Self {
        self.table = table;
        self
    }

    /// Only worth changing when two schemas share one database and should not
    /// wait on each other.
    pub const fn lock_key(mut self, key: i64) -> Self {
        self.lock_key = key;
        self
    }

    /// How long a start will wait behind another one before giving up.
    pub const fn lock_wait(mut self, wait: &'static str) -> Self {
        self.lock_wait = wait;
        self
    }

    /// Everything the binary carries, oldest first.
    pub fn all(&self) -> Result<Vec<Migration>> {
        discover::all(self.dir)
    }

    /// Where the files live in a checkout. Only meaningful on the machine the
    /// binary was built on, which is where new migrations get written.
    pub fn source(&self) -> PathBuf {
        match self.source.strip_prefix("$CARGO_MANIFEST_DIR") {
            Some(rest) => PathBuf::from(self.manifest).join(rest.trim_start_matches(['/', '\\'])),
            None => PathBuf::from(self.source),
        }
    }

    /// Brings the schema up. This is what a service calls before it serves
    /// anything: a service against a half-built schema answers wrongly rather
    /// than not at all, so a migration that fails takes the start with it.
    pub async fn apply(&self, db: &DatabaseConnection) -> Result<Vec<String>> {
        self.apply_through(db, None).await
    }

    /// The same, stopping after the named migration.
    pub async fn apply_through(&self, db: &DatabaseConnection, target: Option<&str>) -> Result<Vec<String>> {
        self.locked(db, |db| async move {
            let done = ledger::applied(db, self.table).await?;
            let carried = self.all()?;

            if let Some(target) = target {
                if !carried.iter().any(|migration| migration.name == target) {
                    return Err(Error::Unknown {
                        name: target.to_owned(),
                    });
                }
            }

            let mut ran = Vec::new();
            for migration in pending(carried, &done) {
                let last = target == Some(migration.name.as_str());

                self.run(db, &migration, migration.up, true).await?;
                tracing::info!(migration = migration.name, "applied a migration");
                ran.push(migration.name);

                if last {
                    break;
                }
            }

            tracing::info!(applied = ran.len(), "the schema is up to date");

            Ok(ran)
        })
        .await
    }

    /// Takes the last `steps` applied migrations back, newest first.
    pub async fn revert(&self, db: &DatabaseConnection, steps: usize) -> Result<Vec<String>> {
        self.locked(db, |db| async move {
            let mut done = ledger::applied(db, self.table).await?;
            let carried = self.all()?;
            let mut reverted = Vec::new();

            for _ in 0..steps {
                let Some(name) = done.pop() else { break };
                let migration = carried
                    .iter()
                    .find(|migration| migration.name == name)
                    .ok_or(Error::Unknown { name: name.clone() })?;
                let down = migration
                    .down
                    .ok_or_else(|| Error::Irreversible { name: name.clone() })?;

                self.run(db, migration, down, false).await?;
                tracing::info!(migration = name, "reverted a migration");
                reverted.push(name);
            }

            Ok(reverted)
        })
        .await
    }

    /// Records migrations as applied without running them. This is for a schema
    /// that already has the change - a database somebody built by hand, or one
    /// where the history was lost while the rows stayed.
    pub async fn mark(&self, db: &DatabaseConnection, target: Option<&str>) -> Result<Vec<String>> {
        self.locked(db, |db| async move {
            let done = ledger::applied(db, self.table).await?;
            let mut marked = Vec::new();

            for migration in pending(self.all()?, &done) {
                if target.is_some_and(|target| target != migration.name) {
                    continue;
                }

                let txn = db.begin().await.map_err(Error::database("a transaction"))?;
                for statement in ledger::record(
                    txn.get_database_backend(),
                    self.table,
                    migration.stamp,
                    &migration.name,
                    &migration.checksum(),
                ) {
                    txn.execute(statement)
                        .await
                        .map_err(Error::database("recording a migration"))?;
                }
                txn.commit().await.map_err(Error::database("a commit"))?;

                tracing::info!(migration = migration.name, "marked as applied without running it");
                marked.push(migration.name);
            }

            if let Some(target) = target {
                if marked.is_empty() {
                    return Err(Error::Unknown {
                        name: target.to_owned(),
                    });
                }
            }

            Ok(marked)
        })
        .await
    }

    /// Drops the ledger row for a migration without running its down.
    pub async fn unmark(&self, db: &DatabaseConnection, name: &str) -> Result<()> {
        let done = ledger::applied(db, self.table).await?;
        if !done.iter().any(|applied| applied == name) {
            return Err(Error::Unknown { name: name.to_owned() });
        }

        let txn = db.begin().await.map_err(Error::database("a transaction"))?;
        for statement in ledger::forget(txn.get_database_backend(), self.table, name) {
            txn.execute(statement)
                .await
                .map_err(Error::database("forgetting a migration"))?;
        }
        txn.commit().await.map_err(Error::database("a commit"))?;

        Ok(())
    }

    /// Every migration the binary carries, with what the database has to say
    /// about it, and whatever the database has that the binary does not.
    pub async fn status(&self, db: &DatabaseConnection) -> Result<Status> {
        let done = ledger::applied(db, self.table).await?;
        let recorded = ledger::checksums(db, self.table).await?;
        let carried = self.all()?;

        let entries = carried
            .iter()
            .map(|migration| {
                let applied = done.iter().any(|name| name == &migration.name);
                let checksum = match recorded.iter().find(|(name, _)| name == &migration.name) {
                    _ if !applied => Checksum::NotApplied,
                    None => Checksum::Unrecorded,
                    Some((_, recorded)) if recorded == &migration.checksum() => Checksum::Match,
                    Some(_) => Checksum::Drift,
                };

                Entry {
                    name: migration.name.clone(),
                    stamp: migration.stamp,
                    applied,
                    reversible: migration.down.is_some(),
                    checksum,
                }
            })
            .collect();

        let unknown = done
            .into_iter()
            .filter(|name| !carried.iter().any(|migration| &migration.name == name))
            .collect();

        Ok(Status { entries, unknown })
    }

    async fn run(&self, db: &DatabaseConnection, migration: &Migration, sql: &str, forward: bool) -> Result<()> {
        let statements = if forward {
            ledger::record(
                db.get_database_backend(),
                self.table,
                migration.stamp,
                &migration.name,
                &migration.checksum(),
            )
        } else {
            ledger::forget(db.get_database_backend(), self.table, &migration.name)
        };

        // a file that asks for it runs on its own: `CREATE INDEX CONCURRENTLY`
        // and its like refuse to be wrapped, and an index built without the lock
        // is the whole reason to reach for them on a table worth the trouble
        if !migration.transactional() {
            db.execute_unprepared(sql).await.map_err(|source| Error::Failed {
                name: migration.name.clone(),
                source,
            })?;

            for statement in statements {
                db.execute(statement).await.map_err(|source| {
                    tracing::error!(
                        migration = migration.name,
                        "the migration ran but the ledger was not written; record it with `mark`"
                    );
                    Error::database("writing the ledger")(source)
                })?;
            }

            return Ok(());
        }

        // each one stands or falls on its own, the way typeorm ran them
        let txn = db.begin().await.map_err(Error::database("a transaction"))?;
        txn.execute_unprepared(sql).await.map_err(|source| Error::Failed {
            name: migration.name.clone(),
            source,
        })?;
        for statement in statements {
            txn.execute(statement)
                .await
                .map_err(Error::database("writing the ledger"))?;
        }
        txn.commit().await.map_err(Error::database("a commit"))?;

        Ok(())
    }

    /// The lock has to be held by a transaction rather than by the session: the
    /// pool hands each statement to whichever connection is free, so a session
    /// lock would be released on a different connection than it was taken on -
    /// which releases nothing and leaves every other service waiting for good.
    async fn locked<'a, T, F, Fut>(&'a self, db: &'a DatabaseConnection, work: F) -> Result<T>
    where
        F: FnOnce(&'a DatabaseConnection) -> Fut,
        Fut: Future<Output = Result<T>> + 'a,
    {
        ledger::ensure(db, self.table).await?;

        let guard = db.begin().await.map_err(Error::database("a transaction"))?;
        guard
            .execute_unprepared(&format!(
                "SET LOCAL lock_timeout = '{}'; SELECT pg_advisory_xact_lock({})",
                self.lock_wait, self.lock_key
            ))
            .await
            .map_err(Error::database("the migration lock"))?;

        let result = work(db).await;

        guard.commit().await.map_err(Error::database("releasing the lock"))?;

        result
    }

    /// Writes an empty `.up.sql` / `.down.sql` pair into the checkout, stamped
    /// with the time, so the pair is never half-made or out of order.
    pub fn create(&self, slug: &str, stamp: i64) -> Result<Vec<PathBuf>> {
        scaffold::create(&self.source(), slug, stamp)
    }
}

fn pending(carried: Vec<Migration>, done: &[String]) -> Vec<Migration> {
    carried
        .into_iter()
        .filter(|migration| !done.iter().any(|name| name == &migration.name))
        .collect()
}

pub struct Status {
    pub entries: Vec<Entry>,
    /// Recorded in the database, not carried by this build. An older service is
    /// still running, or a migration was deleted after it had been applied.
    pub unknown: Vec<String>,
}

pub struct Entry {
    pub name: String,
    pub stamp: i64,
    pub applied: bool,
    pub reversible: bool,
    pub checksum: Checksum,
}

#[derive(PartialEq, Eq)]
pub enum Checksum {
    Match,
    /// The file was edited after the database ran it, so the schema in front of
    /// you is not the schema this file describes.
    Drift,
    /// Applied before anything wrote checksums - typeorm, or a hand.
    Unrecorded,
    NotApplied,
}

/// Carries a directory of migrations into the binary.
///
/// The path is read the way `include_dir!` reads it, so it starts from
/// `$CARGO_MANIFEST_DIR`.
#[macro_export]
macro_rules! embed {
    ($path:tt) => {{
        use $crate::export::include_dir_crate as include_dir;

        static DIR: include_dir::Dir<'static> = $crate::export::include_dir!($path);

        $crate::Migrations::new(&DIR, $path, env!("CARGO_MANIFEST_DIR"))
    }};
}
