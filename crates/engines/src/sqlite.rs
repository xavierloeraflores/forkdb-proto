use super::*;
use rusqlite::{backup::Backup, Connection, OpenFlags};
use std::{fs, path::{Path, PathBuf}, time::Duration};

pub(crate) fn path(value: &str) -> Result<PathBuf> {
    let raw = value.strip_prefix("sqlite://").or_else(|| value.strip_prefix("sqlite:")).unwrap_or(value);
    ensure!(!raw.is_empty() && raw != ":memory:" && !raw.contains('?') && !raw.contains('#'), "SQLite requires an on-disk path without URL query parameters");
    Ok(PathBuf::from(percent_encoding::percent_decode_str(raw).decode_utf8()?.as_ref()))
}
fn open(path: &Path, readonly: bool) -> Result<Connection> {
    Connection::open_with_flags(path, if readonly { OpenFlags::SQLITE_OPEN_READ_ONLY } else { OpenFlags::SQLITE_OPEN_READ_WRITE })
        .context("could not open SQLite database")
}
pub(crate) fn create(request: &CloneRequest) -> Result<Resource> {
    let source_path = path(&request.source.connection_url)?;
    let source_path = if source_path.is_absolute() { source_path } else { request.project_dir.join(source_path) };
    let source = open(&source_path, true)?;
    let dir = request.data_dir.join("sqlite");
    fs::create_dir_all(&dir)?;
    let target = dir.join(format!("{}.sqlite3", request.target_id));
    // create_new prevents overwriting a source, existing clone, or symlink.
    fs::OpenOptions::new().write(true).create_new(true).open(&target).context("SQLite destination already exists or cannot be created")?;
    let result = (|| -> Result<Resource> {
        let mut dest = open(&target, false)?;
        dest.busy_timeout(Duration::from_secs(10))?;
        if matches!(request.strategy, Strategy::Full | Strategy::File) {
            let backup = Backup::new(&source, &mut dest)?;
            let started = std::time::Instant::now();
            loop {
                if matches!(backup.step(256)?, rusqlite::backup::StepResult::Done) { break; }
                ensure!(started.elapsed() < Duration::from_secs(120), "SQLite backup timed out");
                std::thread::sleep(Duration::from_millis(5));
            }
        } else {
            let virtual_count: i64 = source.query_row("SELECT count(*) FROM sqlite_schema WHERE type='table' AND upper(sql) LIKE 'CREATE VIRTUAL TABLE%'", [], |row| row.get(0))?;
            ensure!(virtual_count == 0, "SQLite virtual tables require the full or file strategy; schema-only virtual table copying is unsupported");
            let mut statement = source.prepare("SELECT sql FROM sqlite_schema WHERE sql IS NOT NULL AND name NOT LIKE 'sqlite_%' AND name != '_dbfork_owner' ORDER BY CASE type WHEN 'table' THEN 0 WHEN 'index' THEN 1 WHEN 'view' THEN 2 ELSE 3 END, rowid")?;
            let definitions = statement.query_map([], |row| row.get::<_, String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
            dest.execute_batch("BEGIN;")?;
            for definition in definitions { dest.execute_batch(&definition).map_err(|_| anyhow::anyhow!("cannot reproduce SQLite schema"))?; }
            dest.execute_batch("COMMIT;")?;
        }
        if request.strategy == Strategy::Seeded {
            for file in seed_files(request)? { dest.execute_batch(&fs::read_to_string(file).context("cannot read seed script")?).map_err(|_| anyhow::anyhow!("SQLite seed script failed"))?; }
        }
        dest.execute_batch("DROP TABLE IF EXISTS _dbfork_owner; CREATE TABLE _dbfork_owner (id TEXT PRIMARY KEY NOT NULL);")?;
        dest.execute("INSERT INTO _dbfork_owner(id) VALUES (?1)", [&request.target_id])?;
        let url = format!("sqlite://{}", target.canonicalize()?.display());
        Ok(resource(request, url))
    })();
    if result.is_err() { remove_files(&target)?; }
    result
}
pub(crate) fn validate(r: &Resource) -> Result<()> {
    let p = path(&r.connection_url)?;
    ensure!(p.file_name().and_then(|s| s.to_str()) == Some(&format!("{}.sqlite3", r.id)), "SQLite destination path does not match owned resource");
    ensure!(!fs::symlink_metadata(&p)?.file_type().is_symlink(), "refusing SQLite symlink");
    let db = open(&p, true)?;
    let owner: String = db.query_row("SELECT id FROM _dbfork_owner", [], |row| row.get(0)).context("SQLite ownership marker missing")?;
    ensure!(owner == r.id, "SQLite ownership marker mismatch");
    let check: String = db.query_row("PRAGMA quick_check", [], |row| row.get(0))?;
    ensure!(check == "ok", "SQLite integrity check failed");
    Ok(())
}
fn remove_files(p: &Path) -> Result<()> {
    for path in [p.to_path_buf(), PathBuf::from(format!("{}-wal", p.display())), PathBuf::from(format!("{}-shm", p.display()))] {
        match fs::remove_file(path) { Ok(()) => {}, Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}, Err(e) => return Err(e.into()) }
    }
    Ok(())
}
pub(crate) fn delete(r: &Resource) -> Result<()> {
    if !path(&r.connection_url)?.exists() { return Ok(()); }
    validate(r)?;
    remove_files(&path(&r.connection_url)?)
}
