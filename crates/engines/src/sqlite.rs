use super::*;
use rusqlite::{backup::Backup, Connection, OpenFlags};
use std::{
    fs,
    path::{Path, PathBuf},
    time::Duration,
};

pub(crate) fn path(value: &str) -> Result<PathBuf> {
    let encoded = value
        .strip_prefix("sqlite://")
        .or_else(|| value.strip_prefix("sqlite:"));
    let raw = encoded.unwrap_or(value);
    ensure!(
        !raw.is_empty() && raw != ":memory:",
        "SQLite requires an on-disk database path"
    );
    if encoded.is_some() {
        ensure!(
            !raw.contains('?') && !raw.contains('#'),
            "SQLite URLs must percent-encode question marks and hash characters in filenames"
        );
        Ok(PathBuf::from(
            percent_encoding::percent_decode_str(raw)
                .decode_utf8()?
                .as_ref(),
        ))
    } else {
        Ok(PathBuf::from(raw))
    }
}
fn open(path: &Path, readonly: bool) -> Result<Connection> {
    Connection::open_with_flags(
        path,
        if readonly {
            OpenFlags::SQLITE_OPEN_READ_ONLY
        } else {
            OpenFlags::SQLITE_OPEN_READ_WRITE
        },
    )
    .context("could not open SQLite database")
}
pub(crate) fn create(request: &CloneRequest) -> Result<Resource> {
    let source_path = path(&request.source.connection_url)?;
    let source_path = if source_path.is_absolute() {
        source_path
    } else {
        request.project_dir.join(source_path)
    };
    let source = open(&source_path, true)?;
    let parent = request.data_dir.join("sqlite");
    fs::create_dir_all(&parent)?;
    let dir = parent.join(&request.target_id);
    fs::create_dir(&dir).context("SQLite destination directory already exists")?;
    let target = dir.join(format!("{}.sqlite3", request.target_id));
    let result = (|| -> Result<Resource> {
        use std::io::Write;
        let mut marker = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(dir.join(".owner"))?;
        marker.write_all(request.target_id.as_bytes())?;
        marker.sync_all()?;
        // create_new prevents overwriting a source, existing clone, or symlink.
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&target)
            .context("SQLite destination already exists or cannot be created")?;
        let mut dest = open(&target, false)?;
        dest.busy_timeout(Duration::from_secs(10))?;
        if matches!(request.strategy, Strategy::Full | Strategy::File) {
            let backup = Backup::new(&source, &mut dest)?;
            let started = std::time::Instant::now();
            loop {
                if matches!(backup.step(256)?, rusqlite::backup::StepResult::Done) {
                    break;
                }
                ensure!(
                    started.elapsed() < Duration::from_secs(120),
                    "SQLite backup timed out"
                );
                std::thread::sleep(Duration::from_millis(5));
            }
        } else {
            let virtual_count: i64 = source.query_row("SELECT count(*) FROM sqlite_schema WHERE type='table' AND upper(sql) LIKE 'CREATE VIRTUAL TABLE%'", [], |row| row.get(0))?;
            ensure!(virtual_count == 0, "SQLite virtual tables require the full or file strategy; schema-only virtual table copying is unsupported");
            let mut statement = source.prepare("SELECT sql FROM sqlite_schema WHERE sql IS NOT NULL AND name NOT LIKE 'sqlite_%' AND name != '_dbfork_owner' ORDER BY CASE type WHEN 'table' THEN 0 WHEN 'index' THEN 1 WHEN 'view' THEN 2 ELSE 3 END, rowid")?;
            let definitions = statement
                .query_map([], |row| row.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            dest.execute_batch("BEGIN;")?;
            for definition in definitions {
                dest.execute_batch(&definition)
                    .map_err(|_| anyhow::anyhow!("cannot reproduce SQLite schema"))?;
            }
            dest.execute_batch("COMMIT;")?;
        }
        if request.strategy == Strategy::Seeded {
            for file in seed_files(request)? {
                dest.execute_batch(&fs::read_to_string(file).context("cannot read seed script")?)
                    .map_err(|_| anyhow::anyhow!("SQLite seed script failed"))?;
            }
        }
        dest.execute_batch("DROP TABLE IF EXISTS _dbfork_owner; CREATE TABLE _dbfork_owner (id TEXT PRIMARY KEY NOT NULL);")?;
        dest.execute(
            "INSERT INTO _dbfork_owner(id) VALUES (?1)",
            [&request.target_id],
        )?;
        let url = sqlite_url(&target.canonicalize()?)?;
        Ok(resource(request, url))
    })();
    if result.is_err() {
        remove_files(&target)?;
        let _ = fs::remove_file(dir.join(".owner"));
        fs::remove_dir(&dir)?;
    }
    result
}
pub(crate) fn validate(r: &Resource) -> Result<()> {
    let p = path(&r.connection_url)?;
    check_directory(r, &p)?;
    ensure!(
        p.file_name().and_then(|s| s.to_str()) == Some(&format!("{}.sqlite3", r.id)),
        "SQLite destination path does not match owned resource"
    );
    ensure!(
        !fs::symlink_metadata(&p)?.file_type().is_symlink(),
        "refusing SQLite symlink"
    );
    let db = open(&p, true)?;
    let owner: String = db
        .query_row("SELECT id FROM _dbfork_owner", [], |row| row.get(0))
        .context("SQLite ownership marker missing")?;
    ensure!(owner == r.id, "SQLite ownership marker mismatch");
    let check: String = db.query_row("PRAGMA quick_check", [], |row| row.get(0))?;
    ensure!(check == "ok", "SQLite integrity check failed");
    Ok(())
}
fn remove_files(p: &Path) -> Result<()> {
    for path in [
        p.to_path_buf(),
        PathBuf::from(format!("{}-wal", p.display())),
        PathBuf::from(format!("{}-shm", p.display())),
        PathBuf::from(format!("{}-journal", p.display())),
    ] {
        match fs::remove_file(path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}
fn check_directory(r: &Resource, p: &Path) -> Result<()> {
    let dir = p.parent().context("SQLite owned directory missing")?;
    ensure!(
        dir.file_name().and_then(|s| s.to_str()) == Some(&r.id),
        "SQLite owned directory name mismatch"
    );
    ensure!(
        !fs::symlink_metadata(dir)?.file_type().is_symlink(),
        "refusing SQLite directory symlink"
    );
    let marker = dir.join(".owner");
    ensure!(
        !fs::symlink_metadata(&marker)?.file_type().is_symlink(),
        "refusing SQLite ownership symlink"
    );
    ensure!(
        fs::read_to_string(marker)? == r.id,
        "SQLite directory ownership mismatch"
    );
    Ok(())
}
pub(crate) fn delete(r: &Resource) -> Result<()> {
    let p = path(&r.connection_url)?;
    let dir = p.parent().context("SQLite owned directory missing")?;
    if !dir.exists() {
        return Ok(());
    }
    check_directory(r, &p)?;
    if p.exists() && !r.metadata.contains_key("sqlite_pending") {
        validate(r)?;
    }
    remove_files(&p)?;
    for entry in fs::read_dir(dir)? {
        ensure!(
            entry?.file_name() == ".owner",
            "unexpected file in SQLite owned directory; ownership marker retained for recovery"
        );
    }
    fs::remove_file(dir.join(".owner"))?;
    fs::remove_dir(dir)?;
    Ok(())
}
pub(crate) fn recover(request: &CloneRequest) -> Result<Option<Resource>> {
    let dir = request.data_dir.join("sqlite").join(&request.target_id);
    if !dir.exists() {
        return Ok(None);
    }
    ensure!(
        !fs::symlink_metadata(&dir)?.file_type().is_symlink(),
        "refusing SQLite recovery directory symlink"
    );
    if fs::read_dir(&dir)?.next().is_none() {
        fs::remove_dir(&dir)?;
        return Ok(None);
    }
    let p = dir.join(format!("{}.sqlite3", request.target_id));
    let mut r = resource(
        request,
        sqlite_url(&dir.canonicalize()?.join(p.file_name().unwrap()))?,
    );
    check_directory(&r, &p)?;
    r.metadata.insert("sqlite_pending".into(), "true".into());
    Ok(Some(r))
}
