use rusqlite::Connection;
use serde_json::Value;
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};

fn command(root: &Path, args: &[&str]) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_dbfork-proto"));
    c.arg("-C").arg(root).arg("--json").args(args);
    c.env_remove("DATABASE_URL");
    c
}
fn run(root: &Path, args: &[&str]) -> Value {
    let o = command(root, args).output().unwrap();
    assert!(
        o.status.success(),
        "{:?}: {} {}",
        args,
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    );
    serde_json::from_slice::<Value>(&o.stdout).unwrap()["result"].clone()
}
fn fail(root: &Path, args: &[&str]) -> Output {
    let o = command(root, args).output().unwrap();
    assert!(!o.status.success(), "{args:?}");
    assert_eq!(
        serde_json::from_slice::<Value>(&o.stdout).unwrap()["ok"],
        false
    );
    o
}
fn state(root: &Path) -> Value {
    let dir = if root.join(".git").is_dir() {
        root.join(".git/dbfork-proto")
    } else {
        root.join(".dbfork")
    };
    serde_json::from_slice(&fs::read(dir.join("state.json")).unwrap()).unwrap()
}
fn db_path(r: &Value) -> PathBuf {
    dbfork_engines::sqlite_path(r["connection_url"].as_str().unwrap()).unwrap()
}
fn fixture() -> (tempfile::TempDir, PathBuf, PathBuf) {
    let t = tempfile::tempdir().unwrap();
    let root = t.path().canonicalize().unwrap();
    run(&root, &["init"]);
    let source = db_path(&state(&root)["sources"]["primary"]);
    Connection::open(&source).unwrap().execute_batch("CREATE TABLE items(id INTEGER PRIMARY KEY, value TEXT); INSERT INTO items VALUES(1,'source');").unwrap();
    (t, root, source)
}
#[test]
fn complete_lifecycle_and_source_protection() {
    let (_t, root, source) = fixture();
    fs::write(
        root.join(".env"),
        format!(
            "# retained\nOTHER=value\nDATABASE_URL='sqlite://{}'\n",
            source.display()
        ),
    )
    .unwrap();
    let created = run(&root, &["create", "feat/auth"]);
    assert_eq!(created["created"], true);
    assert!(!created.to_string().contains("connection_url"));
    let child = db_path(&state(&root)["environments"]["feat/auth"]["resources"][0]);
    Connection::open(&child)
        .unwrap()
        .execute("UPDATE items SET value='child'", [])
        .unwrap();
    let value: String = Connection::open(&source)
        .unwrap()
        .query_row("SELECT value FROM items", [], |r| r.get(0))
        .unwrap();
    assert_eq!(value, "source");
    assert!(fs::read_to_string(root.join(".env"))
        .unwrap()
        .contains("# retained\nOTHER=value"));
    assert_eq!(run(&root, &["create", "feat/auth"])["created"], false);
    assert_eq!(
        run(&root, &["status", "--check"])["health"][0]["healthy"],
        true
    );
    assert_eq!(fail(&root, &["delete", "feat/auth"]).status.code(), Some(3));
    fail(&root, &["delete", "feat/auth", "--yes"]);
    fail(&root, &["create", "main"]);
    fail(&root, &["delete", "main", "--yes"]);
    run(
        &root,
        &["create", "child", "--from", "feat/auth", "--no-switch"],
    );
    let grandchild = db_path(&state(&root)["environments"]["child"]["resources"][0]);
    let value: String = Connection::open(grandchild)
        .unwrap()
        .query_row("SELECT value FROM items", [], |r| r.get(0))
        .unwrap();
    assert_eq!(value, "child");
    run(&root, &["switch", "main"]);
    run(&root, &["delete", "feat/auth", "--yes"]);
    assert!(!child.exists());
    assert!(source.exists());
    assert_eq!(
        run(&root, &["delete", "feat/auth", "--yes"])["deleted"],
        false
    );
}
#[test]
fn schema_only_and_seeded() {
    let (_t, root, _) = fixture();
    run(&root, &["create", "schema", "--schema-only", "--no-switch"]);
    let child = db_path(&state(&root)["environments"]["schema"]["resources"][0]);
    let count: i64 = Connection::open(child)
        .unwrap()
        .query_row("SELECT count(*) FROM items", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count, 0);
    let cfg = fs::read_to_string(root.join(".dbfork.toml"))
        .unwrap()
        .replace("seed = []", "seed = [\"seed.sql\"]");
    fs::write(root.join(".dbfork.toml"), cfg).unwrap();
    fs::write(
        root.join("seed.sql"),
        "INSERT INTO items VALUES(2,'seeded');",
    )
    .unwrap();
    run(
        &root,
        &["create", "seeded", "--strategy", "seeded", "--no-switch"],
    );
    let child = db_path(&state(&root)["environments"]["seeded"]["resources"][0]);
    let value: String = Connection::open(child)
        .unwrap()
        .query_row("SELECT value FROM items", [], |r| r.get(0))
        .unwrap();
    assert_eq!(value, "seeded");
}
#[test]
fn multiple_database_failure_rolls_back_every_clone() {
    let (_t, root, source) = fixture();
    let cfg=fs::read_to_string(root.join(".dbfork.toml")).unwrap()+"\n[[databases]]\nname='second'\nengine='sqlite'\nprovider='local'\nconnection_env='SECOND_URL'\nsource_env='DATABASE_URL'\nstrategy='seeded'\nseed=['missing.sql']\n";
    fs::write(root.join(".dbfork.toml"), cfg).unwrap();
    let old = fs::read(root.join(".env")).unwrap();
    fail(&root, &["create", "bad"]);
    assert_eq!(state(&root)["environments"].as_object().unwrap().len(), 0);
    assert_eq!(
        fs::read_dir(root.join(".dbfork/databases/sqlite"))
            .unwrap()
            .count(),
        0
    );
    assert_eq!(fs::read(root.join(".env")).unwrap(), old);
    assert!(source.exists());
}
#[test]
fn concurrent_idempotent_creates_share_one_resource() {
    let (_t, root, _) = fixture();
    let first = command(&root, &["create", "same", "--no-switch"])
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let second = command(&root, &["create", "same", "--no-switch"])
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    for child in [first, second] {
        let o = child.wait_with_output().unwrap();
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stdout));
    }
    assert_eq!(state(&root)["environments"].as_object().unwrap().len(), 1);
    assert_eq!(
        fs::read_dir(root.join(".dbfork/databases/sqlite"))
            .unwrap()
            .count(),
        1
    );
}

#[test]
fn multiple_databases_update_every_configured_env_file() {
    let (_t, root, _) = fixture();
    let config = fs::read_to_string(root.join(".dbfork.toml")).unwrap()
        .replace("env_files = [\".env\"]", "env_files = [\".env\", \"app/.env.local\"]")
        + "\n[[databases]]\nname='second'\nengine='sqlite'\nprovider='local'\nconnection_env='SECOND_URL'\nsource_env='DATABASE_URL'\nstrategy='schema_only'\n";
    fs::write(root.join(".dbfork.toml"), config).unwrap();
    fs::create_dir(root.join("app")).unwrap();
    fs::write(root.join("app/.env.local"), "KEEP=unchanged\n").unwrap();
    run(&root, &["create", "multi"]);
    let state = state(&root);
    let resources = state["environments"]["multi"]["resources"]
        .as_array()
        .unwrap();
    assert_eq!(resources.len(), 2);
    assert_ne!(
        resources[0]["connection_url"],
        resources[1]["connection_url"]
    );
    for (index, resource) in resources.iter().enumerate() {
        let count: i64 = Connection::open(db_path(resource))
            .unwrap()
            .query_row("SELECT count(*) FROM items", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, if index == 0 { 1 } else { 0 });
    }
    let app_env = fs::read_to_string(root.join("app/.env.local")).unwrap();
    assert!(app_env.contains("KEEP=unchanged"));
    assert!(app_env.contains("DATABASE_URL="));
    assert!(app_env.contains("SECOND_URL="));
    run(&root, &["switch", "main"]);
    run(&root, &["delete", "multi", "--yes"]);
}
fn git(root: &Path, args: &[&str]) {
    let o = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .unwrap();
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
}
#[test]
fn git_worktrees_share_registry_and_have_separate_env_files() {
    let t = tempfile::tempdir().unwrap();
    let root = t.path().canonicalize().unwrap().join("project");
    fs::create_dir(&root).unwrap();
    git(&root, &["init", "-b", "main"]);
    git(
        &root,
        &[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.com",
            "commit",
            "--allow-empty",
            "-m",
            "initial",
        ],
    );
    run(&root, &["init"]);
    let source = db_path(&state(&root)["sources"]["primary"]);
    Connection::open(&source)
        .unwrap()
        .execute_batch("CREATE TABLE items(x); INSERT INTO items VALUES(42);")
        .unwrap();
    let child = root.parent().unwrap().join("linked tree");
    git(
        &root,
        &[
            "worktree",
            "add",
            "-b",
            "feat/worktree",
            child.to_str().unwrap(),
        ],
    );
    run(&child, &["sync"]);
    assert_eq!(
        run(&root, &["sync", "--all"])["synced"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert_eq!(run(&child, &["status"])["environment"], "feat/worktree");
    assert_ne!(
        fs::read(root.join(".env")).unwrap(),
        fs::read(child.join(".env")).unwrap()
    );
    assert_eq!(
        run(&root, &["list"])["environments"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    fail(&root, &["delete", "feat/worktree", "--yes"]);
    run(&child, &["switch", "main"]);
    run(&root, &["delete", "feat/worktree", "--yes"]);
    assert_eq!(
        Command::new("git")
            .arg("-C")
            .arg(&child)
            .args(["check-ignore", ".env"])
            .status()
            .unwrap()
            .code(),
        Some(0)
    );
}
