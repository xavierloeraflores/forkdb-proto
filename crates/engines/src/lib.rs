//! Engine adapters own only destinations bearing a DBFork ownership marker.
use anyhow::{bail, ensure, Context, Result};
use dbfork_model::{CloneRequest, Engine, Resource, Strategy};
use std::{collections::BTreeMap, path::PathBuf, process::Stdio};
use tokio::process::Command;
use url::Url;
mod sqlite;
mod sql;
mod mongo;
mod redis_db;

pub fn supported(engine: Engine, strategy: Strategy) -> bool {
    match engine {
        Engine::Sqlite => matches!(strategy, Strategy::Full | Strategy::SchemaOnly | Strategy::Seeded | Strategy::File),
        Engine::Postgres => matches!(strategy, Strategy::Full | Strategy::SchemaOnly | Strategy::Seeded | Strategy::Template),
        Engine::Mysql | Engine::Mongodb => matches!(strategy, Strategy::Full | Strategy::SchemaOnly | Strategy::Seeded),
        Engine::Redis => matches!(strategy, Strategy::Full | Strategy::SchemaOnly),
    }
}

pub async fn create(request: &CloneRequest) -> Result<Resource> {
    owned_id(&request.target_id)?;
    ensure!(supported(request.config.engine, request.strategy), "strategy {:?} is unsupported for {:?}", request.strategy, request.config.engine);
    ensure!(request.config.engine == request.source.engine, "source engine does not match target");
    ensure!(request.target_id != request.source.id, "target must differ from source");
    match request.config.engine {
        Engine::Sqlite => { let r = request.clone(); tokio::task::spawn_blocking(move || sqlite::create(&r)).await? }
        Engine::Postgres | Engine::Mysql => sql::create(request).await,
        Engine::Mongodb => mongo::create(request).await,
        Engine::Redis => redis_db::create(request).await,
    }
}
pub async fn validate(resource: &Resource) -> Result<()> {
    owned_id(&resource.id)?;
    match resource.engine {
        Engine::Sqlite => { let r = resource.clone(); tokio::task::spawn_blocking(move || sqlite::validate(&r)).await? }
        Engine::Postgres | Engine::Mysql => sql::validate(resource).await,
        Engine::Mongodb => mongo::validate(resource).await,
        Engine::Redis => redis_db::validate(resource).await,
    }
}
pub async fn delete(resource: &Resource) -> Result<()> {
    owned_id(&resource.id)?;
    match resource.engine {
        Engine::Sqlite => { let r = resource.clone(); tokio::task::spawn_blocking(move || sqlite::delete(&r)).await? }
        Engine::Postgres | Engine::Mysql => sql::delete(resource).await,
        Engine::Mongodb => mongo::delete(resource).await,
        Engine::Redis => redis_db::delete(resource).await,
    }
}
fn owned_id(id: &str) -> Result<()> {
    ensure!(id.starts_with("dbf_") && id.len() > 4 && id.len() <= 63 && id.bytes().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'_'), "invalid DBFork resource identifier");
    Ok(())
}
fn parse_url(value: &str) -> Result<Url> { Url::parse(value).map_err(|_| anyhow::anyhow!("invalid database connection URL")) }
fn database_name(url: &Url) -> Result<String> {
    let name = percent_encoding::percent_decode_str(url.path().trim_start_matches('/')).decode_utf8().context("invalid database name encoding")?.into_owned();
    ensure!(!name.is_empty() && !name.contains('/'), "URL must name a database");
    Ok(name)
}
fn resource(request: &CloneRequest, connection_url: String) -> Resource {
    Resource { database: request.config.name.clone(), engine: request.config.engine, provider: request.config.provider.clone(), id: request.target_id.clone(), connection_url, strategy: request.strategy, metadata: BTreeMap::new() }
}
fn destination(request: &CloneRequest) -> Result<Url> {
    let mut url = parse_url(request.config.options.get("target_server_url").unwrap_or(&request.source.connection_url))?;
    url.set_path(&format!("/{}", request.target_id));
    // PostgreSQL libpq URLs can select a database again through query parameters.
    ensure!(!url.query_pairs().any(|(k, _)| matches!(k.as_ref(), "dbname" | "database")), "database override in URL query is unsupported");
    Ok(url)
}
fn seed_files(request: &CloneRequest) -> Result<Vec<PathBuf>> {
    ensure!(request.strategy != Strategy::Seeded || !request.config.seed.is_empty(), "seeded strategy requires at least one seed script");
    Ok(request.config.seed.iter().map(|p| request.project_dir.join(p)).collect())
}
async fn run(command: &mut Command, description: &str) -> Result<()> {
    command.stderr(Stdio::piped()).kill_on_drop(true);
    let result = tokio::time::timeout(std::time::Duration::from_secs(1800), command.output()).await
        .with_context(|| format!("{description} timed out"))?
        .with_context(|| format!("could not run {description}; install its database client tools"))?;
    // Client stderr can contain credentials or seed contents. Never forward it.
    if !result.status.success() { bail!("{description} failed with status {}; check connectivity, permissions and client/server version compatibility", result.status); }
    Ok(())
}
