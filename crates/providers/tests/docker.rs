//! Explicit disposable integration test. Requires Docker, pg_dump, pg_restore and
//! DBFORK_TEST_POSTGRES pointing at a disposable PostgreSQL 17 database.
use anyhow::{Context, Result};
use dbfork_model::{CloneRequest, DatabaseConfig, Engine, Resource, Strategy};
use sqlx::{Connection, PgConnection};
use std::collections::BTreeMap;

#[tokio::test]
#[ignore = "requires Docker and disposable DBFORK_TEST_POSTGRES"]
async fn postgres_instance_clone_isolated_and_cleanup() -> Result<()> {
    let source_url = std::env::var("DBFORK_TEST_POSTGRES").context("set DBFORK_TEST_POSTGRES to a disposable PostgreSQL URL")?;
    let suffix = uuid::Uuid::new_v4().simple().to_string();
    let table = format!("provider_fixture_{suffix}");
    let mut source_conn = PgConnection::connect(&source_url).await?;
    sqlx::raw_sql(&format!("CREATE TABLE {table}(id INTEGER PRIMARY KEY); INSERT INTO {table} VALUES(1)"))
        .execute(&mut source_conn).await?;
    let dir = tempfile::tempdir()?;
    let mut request = CloneRequest {
        config:DatabaseConfig {
            name:format!("provider-test-{suffix}"),engine:Engine::Postgres,provider:"docker".into(),
            connection_env:"DATABASE_URL".into(),source_env:None,strategy:Strategy::Instance,seed:vec![],
            options:BTreeMap::from([("image".into(), "postgres:17-alpine".into())]),
        },
        source:Resource{database:"source".into(),engine:Engine::Postgres,provider:"local".into(),id:"source".into(),connection_url:source_url.clone(),strategy:Strategy::Full,metadata:BTreeMap::new()},
        target_id:format!("dbf_{suffix}"),data_dir:dir.path().into(),project_dir:dir.path().into(),strategy:Strategy::Instance,
    };
    let mut resources = Vec::new();
    let result = async {
        let resource = dbfork_providers::create(&request).await?;
        resources.push(resource.clone());
        assert_ne!(resource.connection_url, source_url);
        assert_eq!(resource.id, request.target_id);
        assert_eq!(resource.strategy, Strategy::Instance);
        let url = url::Url::parse(&resource.connection_url)?;
        assert_eq!(url.host_str(), Some("127.0.0.1"));
        assert_ne!(url.port(), url::Url::parse(&source_url)?.port());
        dbfork_providers::validate(&resource).await?;
        let mut target = PgConnection::connect(&resource.connection_url).await?;
        let count:i64 = sqlx::query_scalar(&format!("SELECT COUNT(*) FROM {table}")).fetch_one(&mut target).await?;
        assert_eq!(count, 1);
        sqlx::query(&format!("INSERT INTO {table} VALUES(2)")).execute(&mut target).await?;
        let source_count:i64 = sqlx::query_scalar(&format!("SELECT COUNT(*) FROM {table}")).fetch_one(&mut source_conn).await?;
        assert_eq!(source_count, 1);
        target.close().await?;

        let mut forged = resource.clone();
        forged.metadata.insert("docker_owner".into(), "not-owned".into());
        assert!(dbfork_providers::delete(&forged).await.is_err());
        dbfork_providers::validate(&resource).await?;

        request.source = resource;
        request.target_id = format!("dbf_{}",uuid::Uuid::new_v4().simple());
        request.config.options.insert("instance_clone".into(), "schema_only".into());
        let schema = dbfork_providers::create(&request).await?;
        resources.push(schema.clone());
        let mut target = PgConnection::connect(&schema.connection_url).await?;
        let count:i64 = sqlx::query_scalar(&format!("SELECT COUNT(*) FROM {table}")).fetch_one(&mut target).await?;
        assert_eq!(count, 0);
        target.close().await?;
        Ok::<_,anyhow::Error>(())
    }.await;
    let mut cleanup_errors = Vec::new();
    for resource in resources.iter().rev() {
        if let Err(error) = dbfork_providers::delete(resource).await { cleanup_errors.push(error); }
        else { dbfork_providers::delete(resource).await?; }
    }
    sqlx::query(&format!("DROP TABLE {table}")).execute(&mut source_conn).await?;
    assert!(cleanup_errors.is_empty(), "provider test could not clean up owned containers");
    result
}
