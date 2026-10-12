use super::*;
use sqlx::{Connection, Executor, MySqlConnection, PgConnection};
use std::{fs, io::Write};
fn safe_error(label: &'static str) -> impl FnOnce(sqlx::Error) -> anyhow::Error {
    move |_| anyhow::anyhow!(label)
}
async fn pg(url: &str) -> Result<PgConnection> {
    PgConnection::connect(url)
        .await
        .map_err(safe_error("PostgreSQL connection failed"))
}
async fn mysql(url: &str) -> Result<MySqlConnection> {
    MySqlConnection::connect(url)
        .await
        .map_err(safe_error("MySQL connection failed"))
}
fn admin(url: &str, engine: Engine) -> Result<String> {
    let mut u = parse_url(url)?;
    u.set_path(if engine == Engine::Postgres {
        "/postgres"
    } else {
        "/mysql"
    });
    Ok(u.to_string())
}
fn quoted(name: &str, engine: Engine) -> Result<String> {
    ensure!(!name.contains('\0'), "invalid database identifier");
    Ok(if engine == Engine::Postgres {
        format!("\"{}\"", name.replace('"', "\"\""))
    } else {
        format!("`{}`", name.replace('`', "``"))
    })
}
fn pg_command(program: &str, url: &str) -> Result<Command> {
    let mut u = parse_url(url)?;
    let password = u.password().map(|p| {
        percent_encoding::percent_decode_str(p)
            .decode_utf8_lossy()
            .into_owned()
    });
    u.set_password(None)
        .map_err(|_| anyhow::anyhow!("invalid PostgreSQL URL"))?;
    let mut c = Command::new(program);
    c.arg("--dbname").arg(u.as_str()).stdin(Stdio::null());
    if let Some(password) = password {
        c.env("PGPASSWORD", password);
    }
    Ok(c)
}
fn mysql_options(url: &str) -> Result<tempfile::NamedTempFile> {
    let u = parse_url(url)?;
    let decode = |s: &str| {
        percent_encoding::percent_decode_str(s)
            .decode_utf8_lossy()
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('\n', "\\n")
            .replace('\r', "\\r")
    };
    let mut f = tempfile::NamedTempFile::new()?;
    writeln!(
        f,
        "[client]\nhost=\"{}\"\nport={}\nuser=\"{}\"\npassword=\"{}\"",
        decode(u.host_str().context("MySQL URL needs a host")?),
        u.port().unwrap_or(3306),
        decode(u.username()),
        decode(u.password().unwrap_or(""))
    )?;
    // Retain TLS verification requirements in external client connections.
    if u.query_pairs()
        .any(|(k, v)| k == "ssl-mode" && v != "DISABLED")
    {
        writeln!(f, "ssl-mode=VERIFY_IDENTITY")?;
    }
    Ok(f)
}
pub(crate) async fn create(request: &CloneRequest) -> Result<Resource> {
    seed_files(request)?;
    let target = destination(request)?;
    let source = parse_url(&request.source.connection_url)?;
    let source_name = database_name(&source)?;
    ensure!(
        source_name != request.target_id,
        "destination database name must differ from source"
    );
    let r = resource(request, target.to_string());
    let engine = r.engine;
    let target_name = quoted(&r.id, engine)?;
    let admin_url = admin(&r.connection_url, engine)?;
    if engine == Engine::Postgres {
        let mut con = pg(&admin_url).await?;
        let query = if request.strategy == Strategy::Template {
            ensure!(
                !request.config.options.contains_key("target_server_url"),
                "template strategy requires the source server"
            );
            format!(
                "CREATE DATABASE {target_name} TEMPLATE {}",
                quoted(&source_name, engine)?
            )
        } else {
            format!("CREATE DATABASE {target_name} TEMPLATE template0")
        };
        con.execute(query.as_str()).await.map_err(safe_error(
            "PostgreSQL CREATE DATABASE failed; check privileges and active template connections",
        ))?;
    } else {
        mysql(&admin_url)
            .await?
            .execute(format!("CREATE DATABASE {target_name}").as_str())
            .await
            .map_err(safe_error("MySQL CREATE DATABASE failed; check privileges"))?;
    }
    let populated: Result<()> = async {
        if request.strategy != Strategy::Template {
            let dump = tempfile::NamedTempFile::new()?;
            if engine == Engine::Postgres {
                let mut dump_cmd = pg_command("pg_dump", source.as_str())?;
                dump_cmd.args(["--format=custom", "--no-owner", "--no-privileges"]).arg("--file").arg(dump.path());
                if request.strategy != Strategy::Full { dump_cmd.arg("--schema-only"); }
                run(&mut dump_cmd, "pg_dump").await?;
                let mut restore = pg_command("pg_restore", target.as_str())?;
                restore.args(["--no-owner", "--no-privileges", "--exit-on-error"]).arg(dump.path());
                run(&mut restore, "pg_restore").await?;
            } else {
                let source_options = mysql_options(source.as_str())?;
                let mut dump_cmd = Command::new(request.config.options.get("mysql_dump_command").map(String::as_str).unwrap_or("mysqldump"));
                dump_cmd.arg(format!("--defaults-extra-file={}", source_options.path().display())).args(["--single-transaction", "--skip-lock-tables", "--routines", "--triggers", "--events", "--no-tablespaces"]);
                if request.strategy != Strategy::Full { dump_cmd.arg("--no-data"); }
                dump_cmd.arg(&source_name).stdin(Stdio::null()).stdout(fs::File::create(dump.path())?);
                run_to_file(&mut dump_cmd, "mysqldump", fs::File::create(dump.path())?).await?;
                let target_options = mysql_options(target.as_str())?;
                let mut restore = Command::new(request.config.options.get("mysql_command").map(String::as_str).unwrap_or("mysql"));
                restore.arg(format!("--defaults-extra-file={}", target_options.path().display())).arg(&r.id).stdin(fs::File::open(dump.path())?);
                run(&mut restore, "mysql restore").await?;
            }
        }
        if engine == Engine::Postgres {
            let mut con = pg(&r.connection_url).await?;
            if request.strategy == Strategy::Seeded {
                for p in seed_files(request)? { sqlx::raw_sql(&fs::read_to_string(p).context("cannot read seed script")?).execute(&mut con).await.map_err(safe_error("PostgreSQL seed script failed"))?; }
            }
            sqlx::raw_sql("DROP TABLE IF EXISTS public._dbfork_owner; CREATE TABLE public._dbfork_owner (id TEXT PRIMARY KEY NOT NULL)").execute(&mut con).await.map_err(safe_error("cannot write PostgreSQL ownership marker"))?;
            sqlx::query("INSERT INTO public._dbfork_owner VALUES ($1)").bind(&r.id).execute(&mut con).await.map_err(safe_error("cannot write PostgreSQL ownership marker"))?;
        } else {
            let mut con = mysql(&r.connection_url).await?;
            if request.strategy == Strategy::Seeded {
                for p in seed_files(request)? { sqlx::raw_sql(&fs::read_to_string(p).context("cannot read seed script")?).execute(&mut con).await.map_err(safe_error("MySQL seed script failed"))?; }
            }
            sqlx::raw_sql("DROP TABLE IF EXISTS _dbfork_owner; CREATE TABLE _dbfork_owner (id VARCHAR(63) PRIMARY KEY NOT NULL)").execute(&mut con).await.map_err(safe_error("cannot write MySQL ownership marker"))?;
            sqlx::query("INSERT INTO _dbfork_owner VALUES (?)").bind(&r.id).execute(&mut con).await.map_err(safe_error("cannot write MySQL ownership marker"))?;
        }
        Ok(())
    }.await;
    if let Err(err) = populated {
        if drop_created(&r).await.is_err() {
            bail!("{err}; cleanup failed for owned database {}; delete it manually after verifying ownership", r.id);
        }
        return Err(err);
    }
    Ok(r)
}
async fn drop_created(r: &Resource) -> Result<()> {
    let query = format!("DROP DATABASE IF EXISTS {}", quoted(&r.id, r.engine)?);
    let url = admin(&r.connection_url, r.engine)?;
    if r.engine == Engine::Postgres {
        pg(&url)
            .await?
            .execute(query.as_str())
            .await
            .map_err(safe_error("PostgreSQL cleanup failed"))?;
    } else {
        mysql(&url)
            .await?
            .execute(query.as_str())
            .await
            .map_err(safe_error("MySQL cleanup failed"))?;
    }
    Ok(())
}
pub(crate) async fn validate(r: &Resource) -> Result<()> {
    ensure!(
        database_name(&parse_url(&r.connection_url)?)? == r.id,
        "connection database does not match owned resource"
    );
    let owner: String = if r.engine == Engine::Postgres {
        sqlx::query_scalar("SELECT id FROM public._dbfork_owner")
            .fetch_one(&mut pg(&r.connection_url).await?)
            .await
            .map_err(safe_error("PostgreSQL ownership marker missing"))?
    } else {
        sqlx::query_scalar("SELECT id FROM _dbfork_owner")
            .fetch_one(&mut mysql(&r.connection_url).await?)
            .await
            .map_err(safe_error("MySQL ownership marker missing"))?
    };
    ensure!(owner == r.id, "database ownership marker mismatch");
    Ok(())
}
pub(crate) async fn delete(r: &Resource) -> Result<()> {
    ensure!(
        database_name(&parse_url(&r.connection_url)?)? == r.id,
        "connection database does not match owned resource"
    );
    if !exists(r).await? {
        return Ok(());
    }
    validate(r).await?;
    drop_created(r).await
}

pub(crate) async fn recover(request: &CloneRequest) -> Result<Option<Resource>> {
    let r = resource(request, destination(request)?.to_string());
    if !exists(&r).await? {
        return Ok(None);
    }
    validate(&r).await.context(
        "pending database exists but ownership is not proven; inspect it manually before cleanup",
    )?;
    Ok(Some(r))
}
async fn exists(r: &Resource) -> Result<bool> {
    let admin_url = admin(&r.connection_url, r.engine)?;
    let exists = if r.engine == Engine::Postgres {
        sqlx::query_scalar::<_, bool>("SELECT EXISTS(SELECT 1 FROM pg_database WHERE datname=$1)")
            .bind(&r.id)
            .fetch_one(&mut pg(&admin_url).await?)
            .await
            .map_err(safe_error("cannot inspect pending PostgreSQL database"))?
    } else {
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM information_schema.schemata WHERE schema_name=?",
        )
        .bind(&r.id)
        .fetch_one(&mut mysql(&admin_url).await?)
        .await
        .map_err(safe_error("cannot inspect pending MySQL database"))?
            > 0
    };
    Ok(exists)
}
