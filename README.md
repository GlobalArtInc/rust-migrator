# rust-migrator

SQL migrations for services that were node services first.

The `.up.sql` / `.down.sql` files travel inside the binary, so a service brings
its own schema up wherever it is deployed, and the ledger stays the table
TypeORM wrote — the rows a rewritten service finds in production are the rows it
keeps.

```toml
[dependencies]
migrator = { package = "sqlmig", version = "0.1" }
```

The crate is `sqlmig` on crates.io; `migrator` is what it is called at the call
site, which is what the examples below use. Straight from here works too:

```toml
migrator = { package = "sqlmig", git = "https://github.com/GlobalArtInc/rust-migrator" }
```

## In a service

```rust
static MIGRATIONS: migrator::Migrations = migrator::embed!("$CARGO_MANIFEST_DIR/../../migrations");

MIGRATIONS.apply(&db).await?;
```

Replicas start together and each of them applies what is missing. They take one
advisory lock, so the first one through does the work and the rest wait and then
find nothing left to do. A migration that fails takes the start with it: a
service against a half-built schema answers wrongly rather than not at all.

## As a binary

```rust
static MIGRATIONS: migrator::Migrations = migrator::embed!("$CARGO_MANIFEST_DIR/../../migrations");

#[tokio::main]
async fn main() -> std::process::ExitCode {
    telemetry::init("migrator", DeployEnv::from_env());

    migrator::cli::report(run().await)
}

async fn run() -> anyhow::Result<()> {
    let command = migrator::cli::command()?;
    let db = match command.needs_database() {
        true => Some(database::connect(&DatabaseConfig::from_env()?).await?),
        false => None,
    };

    Ok(command.run(&MIGRATIONS, db.as_ref()).await?)
}
```

Everything the binary has to say goes through `tracing`, including what went
wrong — a job's output is read by a machine, and one plain line in the middle of
a stream of json is a line nobody sees. `report` turns the outcome into an exit
code and logs the error through the same subscriber. Only `help` writes plainly,
because nothing but a person ever asks for it.

```
migrator status                  what the binary carries and what the database has
migrator up [--to NAME]          apply everything pending, or stop after NAME
migrator down [--steps N]        take the last migration back, or the last N
migrator mark [NAME]             record as applied without running it
migrator unmark NAME             drop the ledger row without running the down
migrator verify                  report files edited after the database ran them
migrator create SLUG             write a new pair into the checkout

migrator up --dry-run            print the sql instead of running it
```

## As an image

`globalartltd/sqlmig` carries no schema of its own: the files come off a mount,
so one tag runs every database. This is what a Flux job uses.

```yaml
jobs:
  migrate:
    image: globalartltd/sqlmig:latest
    args: ["up"]
    env:
      MIGRATIONS_DIR: /migrations       # where the pairs are mounted
      MIGRATIONS_TABLE: migrations      # the ledger
    envFrom:
    - secret: app-env                   # DB_HOST, DB_USER, DB_PASS, DB_NAME, ...
```

`DATABASE_URL` is read whole if it is set. Otherwise the pieces: `DB_HOST`,
`DB_PORT` (5432), `DB_USER`, `DB_PASS` or `DB_PASSWORD`, `DB_NAME` or
`DB_DATABASE`, and optionally `DB_SCHEMA` and `DB_SSL_MODE` — both spellings of
the two that differ between our services are read, so the same job definition
works against any of them.

The migrations reach `/migrations` however the deployment likes: a ConfigMap for
a small history, or an init container built from the repository's own SQL for one
that has outgrown a ConfigMap's megabyte.

## The files

```
migrations/
  1748693000572-init.up.sql
  1748693000572-init.down.sql
  1787184000000-AddBillingPlan.up.sql
  1787184000000-AddBillingPlan.down.sql
```

`<timestamp>-<slug>`, applied oldest first. The ledger holds the name of the
class TypeORM generated, so the slug is read back into one: every dash-separated
part gets a capital and nothing else is touched. `workspace-member` and
`WorkspaceMember` both become `WorkspaceMember1748693000572`, which is the name
already sitting in the table.

Two files stamped in the same millisecond keep one order between them, by name,
which is the order the directory listing gave TypeORM. Two files that resolve to
the same name are refused: the ledger could not tell them apart.

`migrator create add-billing-plan` writes the pair, stamped in milliseconds the
way TypeORM stamped them, so the two histories sort into one.

### Outside a transaction

`CREATE INDEX CONCURRENTLY` and its like refuse to be wrapped, and an index built
without the lock is the whole reason to reach for them on a table worth the
trouble. A file may ask to run on its own:

```sql
-- migrator:no-transaction
CREATE INDEX CONCURRENTLY IF NOT EXISTS "IDX_player_stat_player" ON "player_stat" ("player_id");
```

Everything else runs in a transaction of its own, the way TypeORM ran them.

## A schema that already has the change

`mark` records a migration as applied without running it. This is for a database
somebody built by hand, or one where the history was lost while the rows stayed —
a rewritten service usually meets one of the two.

```sh
migrator status                        # see what the database thinks it has
migrator mark Init1748693000572        # one of them
migrator mark --to Db211726134867498   # everything up to and including this one
migrator mark                          # all of them
migrator up                            # and run whatever is left
```

`status` also lists what the ledger holds that the build does not, which is how an
older service still running, or a migration deleted after it had been applied,
becomes visible.

## Drift

Every migration this tool applies is checksummed into `<table>_checksum`, and
`verify` reports files edited after the database ran them: the schema in front of
you is then not the schema the file describes. Migrations applied before this
tool existed carry no checksum and are reported as such, not as a failure.

## Configuration

```rust
migrator::embed!("$CARGO_MANIFEST_DIR/../../migrations")
    .table("migrations")     // the ledger, unless typeorm was told otherwise
    .lock_key(6113251907444282112)
    .lock_wait("150s")       // how long a start waits behind another one
```

## Tests

The live tests need a database they may own outright:

```sh
docker run --rm -d -p 5433:5432 -e POSTGRES_PASSWORD=postgres --name migrator-test postgres:17
MIGRATOR_TEST_DATABASE_URL=postgres://postgres:postgres@localhost:5433/postgres cargo test
```

Without the variable they pass without touching anything.
