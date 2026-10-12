use dbfork_engines::{create, delete, supported, validate};
use dbfork_model::{CloneRequest, DatabaseConfig, Engine, Resource, Strategy};
use rusqlite::Connection;
use std::collections::BTreeMap;

fn request(root: &std::path::Path, strategy: Strategy) -> CloneRequest {
    let source = root.join("source.sqlite");
    let db = Connection::open(&source).unwrap();
    db.execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE users(id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT NOT NULL UNIQUE); CREATE TABLE audit(msg TEXT); CREATE INDEX by_name ON users(name); CREATE VIEW names AS SELECT name FROM users; CREATE TRIGGER record_user AFTER INSERT ON users BEGIN INSERT INTO audit(msg) VALUES (new.name); END; INSERT INTO users(name) VALUES ('original');").unwrap();
    CloneRequest {
        config: DatabaseConfig {
            name: "primary".into(),
            engine: Engine::Sqlite,
            provider: "local".into(),
            connection_env: "DATABASE_URL".into(),
            source_env: None,
            strategy,
            seed: vec![],
            options: BTreeMap::new(),
        },
        source: Resource {
            database: "primary".into(),
            engine: Engine::Sqlite,
            provider: "local".into(),
            id: "source".into(),
            connection_url: format!("sqlite://{}", source.display()),
            strategy: Strategy::Full,
            metadata: BTreeMap::new(),
        },
        target_id: "dbf_test_clone".into(),
        data_dir: root.join("data"),
        project_dir: root.into(),
        strategy,
    }
}
fn db(r: &Resource) -> Connection {
    Connection::open(r.connection_url.strip_prefix("sqlite://").unwrap()).unwrap()
}
fn count(db: &Connection, table: &str) -> i64 {
    db.query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
        .unwrap()
}

#[tokio::test]
async fn full_clone_includes_uncheckpointed_wal_and_is_independent() {
    let root = tempfile::tempdir().unwrap();
    let req = request(root.path(), Strategy::Full);
    let source = db(&req.source);
    source
        .execute_batch("PRAGMA wal_autocheckpoint=0; INSERT INTO users(name) VALUES ('wal');")
        .unwrap();
    let result = create(&req).await.unwrap();
    validate(&result).await.unwrap();
    let target = db(&result);
    assert_eq!(count(&target, "users"), 2);
    target
        .execute("INSERT INTO users(name) VALUES ('clone')", [])
        .unwrap();
    assert_eq!(count(&source, "users"), 2);
    drop(target);
    delete(&result).await.unwrap();
    assert_eq!(count(&source, "users"), 2);
    assert!(
        !std::path::Path::new(result.connection_url.strip_prefix("sqlite://").unwrap()).exists()
    );
}
#[tokio::test]
async fn schema_preserves_indexes_views_and_triggers_without_rows() {
    let root = tempfile::tempdir().unwrap();
    let req = request(root.path(), Strategy::SchemaOnly);
    let result = create(&req).await.unwrap();
    let target = db(&result);
    assert_eq!(count(&target, "users"), 0);
    assert_eq!(count(&target, "audit"), 0);
    target
        .execute("INSERT INTO users(name) VALUES ('new')", [])
        .unwrap();
    assert_eq!(count(&target, "names"), 1);
    assert_eq!(count(&target, "audit"), 1);
    assert!(target
        .execute("INSERT INTO users(name) VALUES ('new')", [])
        .is_err());
    let index: i64 = target
        .query_row(
            "SELECT count(*) FROM sqlite_schema WHERE type='index' AND name='by_name'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(index, 1);
    assert_eq!(count(&db(&req.source), "users"), 1);
}
#[tokio::test]
async fn seed_runs_on_empty_schema() {
    let root = tempfile::tempdir().unwrap();
    let mut req = request(root.path(), Strategy::Seeded);
    std::fs::write(
        root.path().join("seed.sql"),
        "INSERT INTO users(name) VALUES ('seeded');",
    )
    .unwrap();
    req.config.seed.push("seed.sql".into());
    let result = create(&req).await.unwrap();
    let name: String = db(&result)
        .query_row("SELECT name FROM users", [], |r| r.get(0))
        .unwrap();
    assert_eq!(name, "seeded");
}
#[tokio::test]
async fn failure_removes_partial_destination_and_keeps_source() {
    let root = tempfile::tempdir().unwrap();
    let mut req = request(root.path(), Strategy::Seeded);
    std::fs::write(
        root.path().join("bad.sql"),
        "INSERT INTO missing VALUES ('password-secret');",
    )
    .unwrap();
    req.config.seed.push("bad.sql".into());
    let err = create(&req).await.unwrap_err();
    assert!(!format!("{err:#}").contains("password-secret"));
    assert!(!req
        .data_dir
        .join("sqlite/dbf_test_clone/dbf_test_clone.sqlite3")
        .exists());
    assert_eq!(count(&db(&req.source), "users"), 1);
}
#[tokio::test]
async fn ownership_and_existing_destination_prevent_destructive_operations() {
    let root = tempfile::tempdir().unwrap();
    let req = request(root.path(), Strategy::File);
    let result = create(&req).await.unwrap();
    assert!(create(&req).await.is_err());
    let mut forged = result.clone();
    forged.connection_url = req.source.connection_url.clone();
    assert!(delete(&forged).await.is_err());
    db(&result)
        .execute("UPDATE _dbfork_owner SET id='someone_else'", [])
        .unwrap();
    assert!(delete(&result).await.is_err());
    assert_eq!(count(&db(&req.source), "users"), 1);
}
#[test]
fn unsupported_strategies_are_explicit() {
    assert!(!supported(Engine::Sqlite, Strategy::Template));
    assert!(!supported(Engine::Redis, Strategy::Seeded));
    assert!(!supported(Engine::Mongodb, Strategy::Snapshot));
}

#[tokio::test]
async fn virtual_tables_copy_in_full_and_reject_schema_only_before_partial_copy() {
    let root = tempfile::tempdir().unwrap();
    let mut req = request(root.path(), Strategy::Full);
    db(&req.source)
        .execute_batch(
            "CREATE VIRTUAL TABLE search USING fts5(body); INSERT INTO search VALUES ('hello');",
        )
        .unwrap();
    let result = create(&req).await.unwrap();
    assert_eq!(count(&db(&result), "search"), 1);
    delete(&result).await.unwrap();
    req.strategy = Strategy::SchemaOnly;
    let error = create(&req).await.unwrap_err();
    assert!(error.to_string().contains("virtual tables"));
    assert!(!req
        .data_dir
        .join("sqlite/dbf_test_clone/dbf_test_clone.sqlite3")
        .exists());
}

#[tokio::test]
async fn recover_interrupted_backup_requires_directory_ownership_and_cleans_partial_file() {
    let root = tempfile::tempdir().unwrap();
    let req = request(root.path(), Strategy::Full);
    assert!(dbfork_engines::recover(&req).await.unwrap().is_none());
    let dir = req.data_dir.join("sqlite").join(&req.target_id);
    std::fs::create_dir_all(&dir).unwrap();
    assert!(dbfork_engines::recover(&req).await.unwrap().is_none());
    assert!(!dir.exists());
    std::fs::create_dir(&dir).unwrap();
    let file = dir.join(format!("{}.sqlite3", req.target_id));
    std::fs::write(&file, b"partial invalid backup").unwrap();
    std::fs::write(
        format!("{}-journal", file.display()),
        b"interrupted journal",
    )
    .unwrap();
    assert!(dbfork_engines::recover(&req).await.is_err());
    assert!(file.exists());
    std::fs::write(dir.join(".owner"), &req.target_id).unwrap();
    let recovered = dbfork_engines::recover(&req).await.unwrap().unwrap();
    delete(&recovered).await.unwrap();
    assert!(!dir.exists());
    let r = create(&req).await.unwrap();
    validate(&r).await.unwrap();
    delete(&r).await.unwrap();
    assert_eq!(count(&db(&req.source), "users"), 1);
}
