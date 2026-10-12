//! Explicit disposable integration test. Requires Docker, pg_dump, pg_restore and
//! DBFORK_TEST_POSTGRES pointing at a disposable PostgreSQL 17 database.
use anyhow::{Context, Result};
use dbfork_model::{CloneRequest, DatabaseConfig, Engine, Resource, Strategy};
use sqlx::{Connection, PgConnection};
use std::collections::BTreeMap;

#[tokio::test]
#[ignore = "requires Docker and disposable DBFORK_TEST_POSTGRES"]
async fn postgres_instance_clone_isolated_and_cleanup() -> Result<()> {
    let source_url = std::env::var("DBFORK_TEST_POSTGRES")
        .context("set DBFORK_TEST_POSTGRES to a disposable PostgreSQL URL")?;
    let suffix = uuid::Uuid::new_v4().simple().to_string();
    let table = format!("provider_fixture_{suffix}");
    let mut source_conn = PgConnection::connect(&source_url).await?;
    sqlx::raw_sql(&format!(
        "CREATE TABLE {table}(id INTEGER PRIMARY KEY); INSERT INTO {table} VALUES(1)"
    ))
    .execute(&mut source_conn)
    .await?;
    let dir = tempfile::tempdir()?;
    let mut request = CloneRequest {
        config: DatabaseConfig {
            name: format!("provider-test-{suffix}"),
            engine: Engine::Postgres,
            provider: "docker".into(),
            connection_env: "DATABASE_URL".into(),
            source_env: None,
            strategy: Strategy::Instance,
            seed: vec![],
            options: BTreeMap::from([("image".into(), "postgres:17-alpine".into())]),
        },
        source: Resource {
            database: "source".into(),
            engine: Engine::Postgres,
            provider: "local".into(),
            id: "source".into(),
            connection_url: source_url.clone(),
            strategy: Strategy::Full,
            metadata: BTreeMap::new(),
        },
        target_id: format!("dbf_{suffix}"),
        data_dir: dir.path().into(),
        project_dir: dir.path().into(),
        strategy: Strategy::Instance,
    };
    let mut resources = Vec::new();
    let result = async {
        let resource = dbfork_providers::create(&request).await?;
        resources.push(resource.clone());
        let recovered = dbfork_providers::recover(&request)
            .await?
            .context("Docker intent was not recoverable")?;
        assert_eq!(recovered.id, resource.id);
        assert_eq!(recovered.metadata, resource.metadata);
        assert!(
            dbfork_providers::create(&request).await.is_err(),
            "duplicate intent must not provision another container"
        );
        assert_ne!(resource.connection_url, source_url);
        assert_eq!(resource.id, request.target_id);
        assert_eq!(resource.strategy, Strategy::Instance);
        let url = url::Url::parse(&resource.connection_url)?;
        assert_eq!(url.host_str(), Some("127.0.0.1"));
        assert_ne!(url.port(), url::Url::parse(&source_url)?.port());
        dbfork_providers::validate(&resource).await?;
        let mut target = PgConnection::connect(&resource.connection_url).await?;
        let count: i64 = sqlx::query_scalar(&format!("SELECT COUNT(*) FROM {table}"))
            .fetch_one(&mut target)
            .await?;
        assert_eq!(count, 1);
        sqlx::query(&format!("INSERT INTO {table} VALUES(2)"))
            .execute(&mut target)
            .await?;
        let source_count: i64 = sqlx::query_scalar(&format!("SELECT COUNT(*) FROM {table}"))
            .fetch_one(&mut source_conn)
            .await?;
        assert_eq!(source_count, 1);
        target.close().await?;

        let mut forged = resource.clone();
        forged
            .metadata
            .insert("docker_owner".into(), "not-owned".into());
        assert!(dbfork_providers::delete(&forged).await.is_err());
        dbfork_providers::validate(&resource).await?;

        request.source = resource;
        request.target_id = format!("dbf_{}", uuid::Uuid::new_v4().simple());
        request
            .config
            .options
            .insert("instance_clone".into(), "schema_only".into());
        let schema = dbfork_providers::create(&request).await?;
        resources.push(schema.clone());
        let mut target = PgConnection::connect(&schema.connection_url).await?;
        let count: i64 = sqlx::query_scalar(&format!("SELECT COUNT(*) FROM {table}"))
            .fetch_one(&mut target)
            .await?;
        assert_eq!(count, 0);
        target.close().await?;
        Ok::<_, anyhow::Error>(())
    }
    .await;
    let mut cleanup_errors = Vec::new();
    for resource in resources.iter().rev() {
        if let Err(error) = dbfork_providers::delete(resource).await {
            cleanup_errors.push(error);
        } else {
            dbfork_providers::delete(resource).await?;
        }
    }
    // Dump failure happens after provisioning and must remove the new container.
    request.target_id = format!("dbf_{}", uuid::Uuid::new_v4().simple());
    request.source.id = "source".into();
    let mut missing_source = url::Url::parse(&source_url)?;
    missing_source.set_path(&format!("/missing_{suffix}"));
    request.source.connection_url = missing_source.into();
    assert!(dbfork_providers::create(&request).await.is_err());
    let remaining = tokio::process::Command::new("docker")
        .args([
            "ps",
            "--all",
            "--filter",
            &format!("label=dbfork-proto.database={}", request.config.name),
            "--format",
            "{{.ID}}",
        ])
        .output()
        .await?;
    assert!(remaining.status.success());
    assert!(
        remaining.stdout.is_empty(),
        "failed clone left a provisioned container"
    );
    sqlx::query(&format!("DROP TABLE {table}"))
        .execute(&mut source_conn)
        .await?;
    assert!(
        cleanup_errors.is_empty(),
        "provider test could not clean up owned containers"
    );
    result
}

async fn smoke(engine: Engine, env: &str, image: &str) -> Result<()> {
    let source = std::env::var(env).with_context(|| format!("set {env} to a disposable source"))?;
    let dir = tempfile::tempdir()?;
    let mut request = CloneRequest {
        config: DatabaseConfig {
            name: "docker-smoke".into(),
            engine,
            provider: "docker".into(),
            connection_env: "DATABASE_URL".into(),
            source_env: None,
            strategy: Strategy::Instance,
            seed: vec![],
            options: BTreeMap::from([("image".into(), image.into())]),
        },
        source: Resource {
            database: "source".into(),
            engine,
            provider: "local".into(),
            id: "source".into(),
            connection_url: source,
            strategy: Strategy::Full,
            metadata: BTreeMap::new(),
        },
        target_id: format!("dbf_{}", uuid::Uuid::new_v4().simple()),
        data_dir: dir.path().into(),
        project_dir: dir.path().into(),
        strategy: Strategy::Instance,
    };
    if image.starts_with("mariadb:") {
        request
            .config
            .options
            .insert("mysql_flavor".into(), "mariadb".into());
    }
    let mut mysql_source = if engine == Engine::Mysql {
        Some(sqlx::MySqlConnection::connect(&request.source.connection_url).await?)
    } else {
        None
    };
    let table = format!("provider_{}", uuid::Uuid::new_v4().simple());
    if let Some(source) = &mut mysql_source {
        sqlx::raw_sql(&format!(
            "CREATE TABLE {table}(id INTEGER PRIMARY KEY); INSERT INTO {table} VALUES(1)"
        ))
        .execute(source)
        .await?;
    }
    let created = dbfork_providers::create(&request).await;
    let result = match created {
        Ok(resource) => {
            let verified = async {
                dbfork_providers::validate(&resource).await?;
                if let Some(source) = &mut mysql_source {
                    let mut target =
                        sqlx::MySqlConnection::connect(&resource.connection_url).await?;
                    let count: i64 = sqlx::query_scalar(&format!("SELECT COUNT(*) FROM {table}"))
                        .fetch_one(&mut target)
                        .await?;
                    anyhow::ensure!(count == 1, "instance clone did not copy source data");
                    sqlx::query(&format!("INSERT INTO {table} VALUES(2)"))
                        .execute(&mut target)
                        .await?;
                    let count: i64 = sqlx::query_scalar(&format!("SELECT COUNT(*) FROM {table}"))
                        .fetch_one(source)
                        .await?;
                    anyhow::ensure!(count == 1, "instance writes modified source data");
                    target.close().await?;
                }
                Ok::<_, anyhow::Error>(())
            }
            .await;
            dbfork_providers::delete(&resource).await?;
            verified
        }
        Err(error) => Err(error),
    };
    if let Some(source) = &mut mysql_source {
        sqlx::query(&format!("DROP TABLE {table}"))
            .execute(source)
            .await?;
    }
    result
}

#[tokio::test]
#[ignore = "requires Docker, MongoDB tools and disposable DBFORK_TEST_MONGODB"]
async fn mongodb_instance() -> Result<()> {
    smoke(Engine::Mongodb, "DBFORK_TEST_MONGODB", "mongo:7").await
}

#[tokio::test]
#[ignore = "requires Docker, MySQL tools and disposable DBFORK_TEST_MYSQL"]
async fn mysql_instance() -> Result<()> {
    smoke(Engine::Mysql, "DBFORK_TEST_MYSQL", "mysql:8.4").await
}

#[tokio::test]
#[ignore = "requires Docker, MariaDB tools and disposable DBFORK_TEST_MARIADB"]
async fn mariadb_instance() -> Result<()> {
    smoke(Engine::Mysql, "DBFORK_TEST_MARIADB", "mariadb:11.4").await
}

#[tokio::test]
#[ignore = "requires Docker and disposable DBFORK_TEST_REDIS"]
async fn redis_instance() -> Result<()> {
    smoke(Engine::Redis, "DBFORK_TEST_REDIS", "redis:7-alpine").await
}
