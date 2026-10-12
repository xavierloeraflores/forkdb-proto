use serde_json::Value;
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};

fn command(root: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_dbfork-proto"))
        .arg("--json")
        .arg("-C")
        .arg(root)
        .args(args)
        .env_remove("DATABASE_URL")
        .output()
        .unwrap()
}
fn success(root: &Path, args: &[&str]) -> Value {
    let output = command(root, args);
    assert!(
        output.status.success(),
        "{args:?}: {} {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice::<Value>(&output.stdout).unwrap()["result"].clone()
}
fn failure(root: &Path, args: &[&str]) -> Output {
    let output = command(root, args);
    assert!(!output.status.success(), "unexpected success: {args:?}");
    assert_eq!(
        serde_json::from_slice::<Value>(&output.stdout).unwrap()["ok"],
        false
    );
    output
}
fn git(root: &Path, args: &[&str]) {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}
fn fixture() -> (tempfile::TempDir, PathBuf) {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    (directory, root)
}

#[test]
fn missing_config_does_not_allow_init_to_erase_existing_state() {
    let (_directory, root) = fixture();
    success(&root, &["init"]);
    success(&root, &["create", "feat/data"]);
    let state_path = root.join(".dbfork/state.json");
    let state = fs::read(&state_path).unwrap();
    let env = fs::read(root.join(".env")).unwrap();
    fs::remove_file(root.join(".dbfork.toml")).unwrap();
    failure(&root, &["init"]);
    assert_eq!(fs::read(state_path).unwrap(), state);
    assert_eq!(fs::read(root.join(".env")).unwrap(), env);
}

#[test]
fn moved_worktree_still_protects_its_active_database_from_deletion() {
    let (_directory, base) = fixture();
    let root = base.join("main");
    fs::create_dir(&root).unwrap();
    git(&root, &["init", "-b", "main"]);
    success(&root, &["init"]);
    git(&root, &["add", ".dbfork.toml"]);
    git(
        &root,
        &[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.com",
            "commit",
            "-m",
            "config",
        ],
    );
    let old = base.join("old tree");
    let moved = base.join("moved tree");
    git(
        &root,
        &["worktree", "add", "-b", "feat/moved", old.to_str().unwrap()],
    );
    success(&old, &["create", "feat/moved"]);
    let state_path = root.join(".git/dbfork-proto/state.json");
    let state: Value = serde_json::from_slice(&fs::read(&state_path).unwrap()).unwrap();
    let database = state["environments"]["feat/moved"]["resources"][0]["connection_url"]
        .as_str()
        .unwrap()
        .strip_prefix("sqlite://")
        .unwrap();
    git(
        &root,
        &[
            "worktree",
            "move",
            old.to_str().unwrap(),
            moved.to_str().unwrap(),
        ],
    );
    failure(&root, &["delete", "feat/moved", "--yes"]);
    assert!(Path::new(database).exists());
    success(&moved, &["switch", "main"]);
    success(&root, &["delete", "feat/moved", "--yes"]);
    assert!(!Path::new(database).exists());
}

#[test]
fn ignore_negations_are_rejected_before_creating_secret_file() {
    let (_directory, root) = fixture();
    git(&root, &["init", "-b", "main"]);
    fs::write(root.join(".gitignore"), "!.env\n").unwrap();
    failure(&root, &["init"]);
    assert!(!root.join(".env").exists());
    assert!(!root.join(".dbfork.toml").exists());
}

#[test]
fn invalid_arguments_return_json_without_echoing_credentials() {
    let (_directory, root) = fixture();
    let output = failure(
        &root,
        &[
            "init",
            "--engine",
            "postgres://user:FAKE_AUDIT_PASSWORD@localhost/db",
        ],
    );
    assert_eq!(output.status.code(), Some(2));
    assert!(!String::from_utf8_lossy(&output.stdout).contains("FAKE_AUDIT_PASSWORD"));
    assert!(!String::from_utf8_lossy(&output.stderr).contains("FAKE_AUDIT_PASSWORD"));
}

#[test]
fn master_repository_sync_activates_the_source_environment() {
    let (_directory, root) = fixture();
    git(&root, &["init", "-b", "master"]);
    success(&root, &["init"]);
    success(&root, &["sync"]);
    let status = success(&root, &["status"]);
    assert_eq!(status["git_branch"], "master");
    assert_eq!(status["environment"], "master");
    assert_eq!(status["environments"], 0);
    assert!(fs::read_to_string(root.join(".dbfork.toml"))
        .unwrap()
        .contains("default_branch = \"master\""));
}

#[test]
fn interrupted_sqlite_clone_is_recovered_and_can_be_recreated() {
    use std::{
        process::Stdio,
        time::{Duration, Instant},
    };
    let (_directory, root) = fixture();
    success(&root, &["init"]);
    let source = root.join(".dbfork/source.sqlite");
    {
        let connection = rusqlite::Connection::open(&source).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE payload(data BLOB); INSERT INTO payload VALUES(zeroblob(67108864));",
            )
            .unwrap();
    }
    let mut child = Command::new(env!("CARGO_BIN_EXE_dbfork-proto"))
        .arg("--json")
        .arg("-C")
        .arg(&root)
        .args(["create", "feat/interrupted", "--no-switch"])
        .env_remove("DATABASE_URL")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut allocated = None;
    while Instant::now() < deadline {
        if child.try_wait().unwrap().is_some() {
            break;
        }
        let state: Value =
            serde_json::from_slice(&fs::read(root.join(".dbfork/state.json")).unwrap()).unwrap();
        if let Some(id) =
            state["environments"]["feat/interrupted"]["pending"][0]["target_id"].as_str()
        {
            let path = root
                .join(".dbfork/databases/sqlite")
                .join(id)
                .join(format!("{id}.sqlite3"));
            if fs::metadata(&path).is_ok_and(|metadata| metadata.len() > 0) {
                allocated = Some(path);
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    // Always reap our child, including an unexpected early completion/timeout.
    let _ = child.kill();
    let output = child.wait_with_output().unwrap();
    let allocated = allocated.expect(
        "clone did not expose an allocated database with a durable pending intent before deadline",
    );
    assert!(
        !output.status.success(),
        "clone completed before the interruption test could stop it"
    );
    let state: Value =
        serde_json::from_slice(&fs::read(root.join(".dbfork/state.json")).unwrap()).unwrap();
    assert!(!state["environments"]["feat/interrupted"]["pending"]
        .as_array()
        .unwrap()
        .is_empty());
    success(&root, &["delete", "feat/interrupted", "--yes"]);
    assert!(
        !allocated.parent().unwrap().exists(),
        "partial SQLite files or ownership directory survived recovery"
    );
    success(&root, &["create", "feat/interrupted", "--no-switch"]);
    let source_bytes: i64 = rusqlite::Connection::open(&source)
        .unwrap()
        .query_row("SELECT length(data) FROM payload", [], |row| row.get(0))
        .unwrap();
    assert_eq!(source_bytes, 67108864);
}

#[test]
fn installed_hook_provisions_a_new_git_worktree_automatically() {
    let (_directory, base) = fixture();
    let root = base.join("main");
    fs::create_dir(&root).unwrap();
    git(&root, &["init", "-b", "main"]);
    success(&root, &["init"]);
    git(&root, &["add", ".dbfork.toml"]);
    git(
        &root,
        &[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.com",
            "commit",
            "-m",
            "config",
        ],
    );
    success(&root, &["hooks", "install"]);
    let linked = base.join(if cfg!(unix) {
        "linked tree\nnewline"
    } else {
        "linked tree"
    });
    git(
        &root,
        &[
            "worktree",
            "add",
            "-b",
            "feat/automatic",
            linked.to_str().unwrap(),
        ],
    );
    let status = success(&linked, &["status", "--check"]);
    assert_eq!(status["environment"], "feat/automatic");
    assert_eq!(status["health"][0]["healthy"], true);
    assert_ne!(
        fs::read(root.join(".env")).unwrap(),
        fs::read(linked.join(".env")).unwrap()
    );
    success(&root, &["hooks", "uninstall"]);
    success(&root, &["hooks", "uninstall"]);
}
