//! Runs against a real postgres, because the whole point is what the ledger holds
//! afterwards. Set `MIGRATOR_TEST_DATABASE_URL` to a database it may own outright:
//!
//! ```sh
//! docker run --rm -d -p 5433:5432 -e POSTGRES_PASSWORD=postgres --name migrator-test postgres:17
//! MIGRATOR_TEST_DATABASE_URL=postgres://postgres:postgres@localhost:5433/postgres cargo test
//! ```

use migrator::{Checksum, Migrations};
use sea_orm::{ConnectionTrait, Database, DatabaseConnection, Statement};

static MIGRATIONS: Migrations = migrator::embed!("$CARGO_MANIFEST_DIR/tests/migrations");

async fn database() -> Option<DatabaseConnection> {
    let url = std::env::var("MIGRATOR_TEST_DATABASE_URL").ok()?;
    let db = Database::connect(url).await.expect("the test database is reachable");

    db.execute_unprepared(
        r#"DROP TABLE IF EXISTS "widget", "migrations", "migrations_checksum", "mine", "mine_checksum""#,
    )
    .await
    .expect("the test database can be emptied");

    Some(db)
}

async fn columns(db: &DatabaseConnection, table: &str) -> Vec<String> {
    db.query_all(Statement::from_string(
        db.get_database_backend(),
        format!(
            "SELECT column_name FROM information_schema.columns \
             WHERE table_name = '{table}' ORDER BY column_name"
        ),
    ))
    .await
    .expect("the columns are readable")
    .into_iter()
    .map(|row| row.try_get::<String>("", "column_name").expect("a column name"))
    .collect()
}

#[tokio::test]
async fn a_schema_goes_up_and_comes_back_down() {
    let Some(db) = database().await else { return };

    let applied = MIGRATIONS.apply(&db).await.expect("the schema goes up");
    assert_eq!(applied.len(), 3);
    assert_eq!(columns(&db, "widget").await, ["id", "name"]);

    // the second run finds nothing left, which is what every replica after the
    // first one does on the way up
    assert!(MIGRATIONS.apply(&db).await.expect("a second run is quiet").is_empty());

    let reverted = MIGRATIONS.revert(&db, 2).await.expect("two come back down");
    assert_eq!(reverted, ["WidgetNameIndex1000000000002", "AddWidgetName1000000000001"]);
    assert_eq!(columns(&db, "widget").await, ["id"]);
}

#[tokio::test]
async fn the_ledger_is_the_table_typeorm_wrote() {
    let Some(db) = database().await else { return };

    MIGRATIONS.apply(&db).await.expect("the schema goes up");

    let rows = db
        .query_all(Statement::from_string(
            db.get_database_backend(),
            r#"SELECT "id", "timestamp", "name" FROM "migrations" ORDER BY "timestamp""#,
        ))
        .await
        .expect("the ledger is readable");

    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0].try_get::<i64>("", "timestamp").unwrap(), 1000000000000);
    assert_eq!(rows[0].try_get::<String>("", "name").unwrap(), "Init1000000000000");
    assert!(
        rows[0].try_get::<i32>("", "id").is_ok(),
        "the id is the serial typeorm had"
    );
}

#[tokio::test]
async fn a_schema_that_already_has_the_change_is_marked_rather_than_run() {
    let Some(db) = database().await else { return };

    // the change is here already, put in by a hand that left no ledger row
    db.execute_unprepared(r#"CREATE TABLE "widget" ("id" SERIAL PRIMARY KEY)"#)
        .await
        .expect("the table is created by hand");

    let marked = MIGRATIONS
        .mark(&db, Some("Init1000000000000"))
        .await
        .expect("the first is marked");
    assert_eq!(marked, ["Init1000000000000"]);

    // and the rest go up over it without tripping on the table that is there
    let applied = MIGRATIONS.apply(&db).await.expect("the rest go up");
    assert_eq!(applied.len(), 2);
}

#[tokio::test]
async fn an_edited_file_is_reported_and_a_missing_one_is_too() {
    let Some(db) = database().await else { return };

    MIGRATIONS.apply(&db).await.expect("the schema goes up");

    let status = MIGRATIONS.status(&db).await.expect("the status is readable");
    assert!(status.entries.iter().all(|entry| entry.checksum == Checksum::Match));
    assert!(status.unknown.is_empty());

    db.execute_unprepared(
        r#"UPDATE "migrations_checksum" SET "checksum" = repeat('0', 64) WHERE "name" = 'Init1000000000000';
           INSERT INTO "migrations" ("timestamp", "name") VALUES (900000000000, 'FromAnOlderBuild900000000000')"#,
    )
    .await
    .expect("the ledger can be disturbed");

    let status = MIGRATIONS.status(&db).await.expect("the status is readable");
    let init = &status.entries[0];
    assert_eq!(init.name, "Init1000000000000");
    assert!(init.checksum == Checksum::Drift, "an edited file has to be visible");
    assert_eq!(status.unknown, ["FromAnOlderBuild900000000000"]);
}

#[tokio::test]
async fn the_ledger_can_be_told_to_live_somewhere_else() {
    let Some(db) = database().await else { return };

    let mine = migrator::embed!("$CARGO_MANIFEST_DIR/tests/migrations").table("mine");

    mine.apply(&db).await.expect("the schema goes up");

    let rows = db
        .query_all(Statement::from_string(
            db.get_database_backend(),
            r#"SELECT "name" FROM "mine""#,
        ))
        .await
        .expect("the named ledger is readable");
    assert_eq!(rows.len(), 3);
}
