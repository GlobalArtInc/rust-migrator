//! The names a build hands the ledger are the names typeorm wrote, and the file
//! that asks to run outside a transaction is the only one that does.

static MIGRATIONS: migrator::Migrations = migrator::embed!("$CARGO_MANIFEST_DIR/tests/migrations");

#[test]
fn the_pairs_are_read_in_order_of_their_stamp() {
    let carried = MIGRATIONS.all().expect("the migrations are readable");
    let names: Vec<&str> = carried.iter().map(|migration| migration.name.as_str()).collect();

    assert_eq!(
        names,
        [
            "Init1000000000000",
            "AddWidgetName1000000000001",
            "WidgetNameIndex1000000000002",
            "Db101000000000003",
            "Db91000000000003",
        ]
    );
}

#[test]
fn files_that_share_a_stamp_keep_one_order_between_them() {
    // typeorm ordered by the stamp alone and let the directory listing settle the
    // ties; three of the wfs migrations were stamped in the same millisecond
    let carried = MIGRATIONS.all().expect("the migrations are readable");
    let tied: Vec<&str> = carried
        .iter()
        .filter(|migration| migration.stamp == 1000000000003)
        .map(|migration| migration.name.as_str())
        .collect();

    assert_eq!(tied, ["Db101000000000003", "Db91000000000003"]);
}

#[test]
fn every_pair_can_be_taken_back() {
    for migration in MIGRATIONS.all().expect("the migrations are readable") {
        assert!(migration.down.is_some(), "{} has no down", migration.name);
    }
}

#[test]
fn only_the_file_that_asks_runs_outside_a_transaction() {
    let carried = MIGRATIONS.all().expect("the migrations are readable");

    assert!(carried[0].transactional());
    assert!(carried[1].transactional());
    assert!(!carried[2].transactional(), "the directive was not seen");
}

#[test]
fn the_checksum_follows_the_file_it_was_taken_from() {
    let carried = MIGRATIONS.all().expect("the migrations are readable");

    assert_eq!(carried[0].checksum().len(), 64);
    assert_ne!(carried[0].checksum(), carried[1].checksum());
}

#[test]
fn the_source_points_back_into_the_checkout() {
    assert!(
        MIGRATIONS.source().join("1000000000000-init.up.sql").is_file(),
        "create writes next to the files the build carries"
    );
}
