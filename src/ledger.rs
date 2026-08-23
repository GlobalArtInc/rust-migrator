use sea_orm::{ConnectionTrait, DatabaseConnection, Statement, Value};

use crate::error::{Error, Result};

/// The table typeorm wrote, kept exactly as it was: these services were node
/// services first, and their databases still hold the rows it left.
pub(crate) fn create(table: &str) -> String {
    format!(
        r#"CREATE TABLE IF NOT EXISTS "{table}" (
            "id" SERIAL NOT NULL,
            "timestamp" bigint NOT NULL,
            "name" character varying NOT NULL,
            CONSTRAINT "PK_8c82d7f526340ab734260ea46be" PRIMARY KEY ("id")
        );
        CREATE TABLE IF NOT EXISTS "{table}_checksum" (
            "name" character varying NOT NULL,
            "checksum" character(64) NOT NULL,
            "recorded_at" timestamptz NOT NULL DEFAULT now(),
            CONSTRAINT "PK_{table}_checksum" PRIMARY KEY ("name")
        )"#
    )
}

pub(crate) async fn ensure(db: &DatabaseConnection, table: &str) -> Result<()> {
    db.execute_unprepared(&create(table))
        .await
        .map_err(Error::database("the migrations table"))?;

    Ok(())
}

/// The names the database says it has, oldest first.
pub(crate) async fn applied(db: &DatabaseConnection, table: &str) -> Result<Vec<String>> {
    ensure(db, table).await?;

    let rows = db
        .query_all(Statement::from_string(
            db.get_database_backend(),
            format!(r#"SELECT "name" FROM "{table}" ORDER BY "timestamp", "id""#),
        ))
        .await
        .map_err(Error::database("reading the applied migrations"))?;

    rows.into_iter()
        .map(|row| {
            row.try_get::<String>("", "name")
                .map_err(Error::database("reading a migration name"))
        })
        .collect()
}

pub(crate) async fn checksums(db: &DatabaseConnection, table: &str) -> Result<Vec<(String, String)>> {
    ensure(db, table).await?;

    let rows = db
        .query_all(Statement::from_string(
            db.get_database_backend(),
            format!(r#"SELECT "name", "checksum" FROM "{table}_checksum""#),
        ))
        .await
        .map_err(Error::database("reading the recorded checksums"))?;

    rows.into_iter()
        .map(|row| {
            Ok((
                row.try_get::<String>("", "name")
                    .map_err(Error::database("reading a checksum name"))?,
                row.try_get::<String>("", "checksum")
                    .map_err(Error::database("reading a checksum"))?,
            ))
        })
        .collect()
}

pub(crate) fn record(
    backend: sea_orm::DatabaseBackend,
    table: &str,
    stamp: i64,
    name: &str,
    checksum: &str,
) -> Vec<Statement> {
    vec![
        Statement::from_sql_and_values(
            backend,
            format!(r#"INSERT INTO "{table}" ("timestamp", "name") VALUES ($1, $2)"#),
            [Value::from(stamp), Value::from(name)],
        ),
        Statement::from_sql_and_values(
            backend,
            format!(
                r#"INSERT INTO "{table}_checksum" ("name", "checksum") VALUES ($1, $2)
                   ON CONFLICT ("name") DO UPDATE SET "checksum" = EXCLUDED."checksum", "recorded_at" = now()"#
            ),
            [Value::from(name), Value::from(checksum)],
        ),
    ]
}

pub(crate) fn forget(backend: sea_orm::DatabaseBackend, table: &str, name: &str) -> Vec<Statement> {
    vec![
        Statement::from_sql_and_values(
            backend,
            format!(r#"DELETE FROM "{table}" WHERE "name" = $1"#),
            [Value::from(name)],
        ),
        Statement::from_sql_and_values(
            backend,
            format!(r#"DELETE FROM "{table}_checksum" WHERE "name" = $1"#),
            [Value::from(name)],
        ),
    ]
}
