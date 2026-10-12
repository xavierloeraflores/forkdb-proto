//! Run only against disposable source databases. These tests create fixture tables/keys.
use anyhow::Result;
use dbfork_engines::{create, delete, validate};
use dbfork_model::{CloneRequest, DatabaseConfig, Engine, Resource, Strategy};
use sqlx::{Connection, Executor};
use std::collections::BTreeMap;

fn request(engine: Engine, env: &str, root: &std::path::Path) -> CloneRequest {
    let source_url =
        std::env::var(env).unwrap_or_else(|_| panic!("set {env} to a disposable database"));
    CloneRequest {
        config: DatabaseConfig {
            name: "test".into(),
            engine,
            provider: "local".into(),
            connection_env: "DATABASE_URL".into(),
            source_env: None,
            strategy: Strategy::Full,
            seed: vec![],
            options: BTreeMap::new(),
        },
        source: Resource {
            database: "test".into(),
            engine,
            provider: "local".into(),
            id: "source".into(),
            connection_url: source_url,
            strategy: Strategy::Full,
            metadata: BTreeMap::new(),
        },
        target_id: format!("dbf_engine_test_{}", std::process::id()),
        data_dir: root.join("data"),
        project_dir: root.into(),
        strategy: Strategy::Full,
    }
}
#[tokio::test]
#[ignore = "requires disposable PostgreSQL and pg_dump/pg_restore"]
async fn postgres_full_schema_seed_and_rollback() -> Result<()> {
    let root = tempfile::tempdir()?;
    let mut req = request(Engine::Postgres, "DBFORK_TEST_POSTGRES", root.path());
    let mut source = sqlx::PgConnection::connect(&req.source.connection_url).await?;
    source.execute("DROP TABLE IF EXISTS engine_fixture; CREATE TABLE engine_fixture (id INTEGER PRIMARY KEY, note TEXT); INSERT INTO engine_fixture VALUES (1, 'source')").await?;
    for strategy in [Strategy::Full, Strategy::SchemaOnly, Strategy::Seeded] {
        req.strategy = strategy;
        std::fs::write(
            root.path().join("seed.sql"),
            "INSERT INTO engine_fixture VALUES (2, 'seed');",
        )?;
        req.config.seed = vec!["seed.sql".into()];
        let r = create(&req).await?;
        validate(&r).await?;
        assert!(dbfork_engines::recover(&req).await?.is_some());
        let mut target = sqlx::PgConnection::connect(&r.connection_url).await?;
        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM engine_fixture")
            .fetch_one(&mut target)
            .await?;
        assert_eq!(
            count,
            if strategy == Strategy::SchemaOnly {
                0
            } else {
                1
            }
        );
        target
            .execute("INSERT INTO engine_fixture VALUES (3, 'clone')")
            .await?;
        target.close().await?;
        delete(&r).await?;
        delete(&r).await?;
        assert!(dbfork_engines::recover(&req).await?.is_none());
        let source_count: i64 = sqlx::query_scalar("SELECT count(*) FROM engine_fixture")
            .fetch_one(&mut source)
            .await?;
        assert_eq!(source_count, 1);
    }
    std::fs::write(
        root.path().join("seed.sql"),
        "INVALID SQL secret-never-print;",
    )?;
    assert!(create(&req).await.is_err());
    let leaked: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_database WHERE datname=$1)")
            .bind(&req.target_id)
            .fetch_one(&mut source)
            .await?;
    assert!(!leaked);
    // Close the source client so PostgreSQL can copy the template database.
    source.close().await?;
    req.strategy = Strategy::Template;
    let r = create(&req).await?;
    validate(&r).await?;
    delete(&r).await?;
    delete(&r).await?;
    assert!(dbfork_engines::recover(&req).await?.is_none());
    Ok(())
}
#[tokio::test]
#[ignore = "requires disposable MySQL and mysql/mysqldump"]
async fn mysql_full_schema_seed_and_rollback() -> Result<()> {
    let root = tempfile::tempdir()?;
    let mut req = request(Engine::Mysql, "DBFORK_TEST_MYSQL", root.path());
    let mut source = sqlx::MySqlConnection::connect(&req.source.connection_url).await?;
    sqlx::raw_sql("DROP TABLE IF EXISTS engine_fixture; CREATE TABLE engine_fixture (id INTEGER PRIMARY KEY, note TEXT); INSERT INTO engine_fixture VALUES (1, 'source')").execute(&mut source).await?;
    for strategy in [Strategy::Full, Strategy::SchemaOnly, Strategy::Seeded] {
        req.strategy = strategy;
        std::fs::write(
            root.path().join("seed.sql"),
            "INSERT INTO engine_fixture VALUES (2, 'seed');",
        )?;
        req.config.seed = vec!["seed.sql".into()];
        let r = create(&req).await?;
        validate(&r).await?;
        assert!(dbfork_engines::recover(&req).await?.is_some());
        let mut target = sqlx::MySqlConnection::connect(&r.connection_url).await?;
        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM engine_fixture")
            .fetch_one(&mut target)
            .await?;
        assert_eq!(
            count,
            if strategy == Strategy::SchemaOnly {
                0
            } else {
                1
            }
        );
        target
            .execute("INSERT INTO engine_fixture VALUES (3, 'clone')")
            .await?;
        target.close().await?;
        delete(&r).await?;
        delete(&r).await?;
        assert!(dbfork_engines::recover(&req).await?.is_none());
        let source_count: i64 = sqlx::query_scalar("SELECT count(*) FROM engine_fixture")
            .fetch_one(&mut source)
            .await?;
        assert_eq!(source_count, 1);
    }
    std::fs::write(
        root.path().join("seed.sql"),
        "INVALID SQL secret-never-print;",
    )?;
    assert!(create(&req).await.is_err());
    let leaked: i64 =
        sqlx::query_scalar("SELECT count(*) FROM information_schema.schemata WHERE schema_name=?")
            .bind(&req.target_id)
            .fetch_one(&mut source)
            .await?;
    assert_eq!(leaked, 0);
    Ok(())
}
async fn mongo(url: &str, script: &str) -> Result<()> {
    let status = tokio::process::Command::new("mongosh")
        .args([
            "--nodb",
            "--quiet",
            "--eval",
            &format!("const db=connect(process.env.DBFORK_TEST_MONGO_URL); {script}"),
        ])
        .env("DBFORK_TEST_MONGO_URL", url)
        .status()
        .await?;
    anyhow::ensure!(status.success(), "MongoDB fixture assertion failed");
    Ok(())
}
#[tokio::test]
#[ignore = "requires disposable MongoDB and mongosh/mongodump/mongorestore"]
async fn mongodb_full_schema_seed_and_rollback() -> Result<()> {
    let root = tempfile::tempdir()?;
    let mut req = request(Engine::Mongodb, "DBFORK_TEST_MONGODB", root.path());
    mongo(&req.source.connection_url,"db.engine_fixture.drop(); db.engine_fixture.insertOne({_id:1,note:'source'}); db.engine_fixture.createIndex({note:1});").await?;
    for strategy in [Strategy::Full, Strategy::SchemaOnly, Strategy::Seeded] {
        req.strategy = strategy;
        std::fs::write(
            root.path().join("seed.js"),
            "db.engine_fixture.insertOne({_id:2,note:'seed'});",
        )?;
        req.config.seed = vec!["seed.js".into()];
        let r = create(&req).await?;
        validate(&r).await?;
        assert!(dbfork_engines::recover(&req).await?.is_some());
        let expected = if strategy == Strategy::SchemaOnly {
            0
        } else {
            1
        };
        mongo(&r.connection_url,&format!("if(db.engine_fixture.countDocuments({{}})!=={expected}) throw Error('count'); if(db.engine_fixture.getIndexes().length!==2) throw Error('index'); db.engine_fixture.insertOne({{_id:3}});")).await?;
        delete(&r).await?;
        delete(&r).await?;
        assert!(dbfork_engines::recover(&req).await?.is_none());
        mongo(
            &req.source.connection_url,
            "if(db.engine_fixture.countDocuments({})!==1) throw Error('source changed');",
        )
        .await?;
    }
    std::fs::write(
        root.path().join("seed.js"),
        "throw Error('secret-never-print');",
    )?;
    assert!(create(&req).await.is_err());
    let mut target = url::Url::parse(&req.source.connection_url)?;
    target.set_path(&format!("/{}", req.target_id));
    mongo(
        target.as_str(),
        "if(db.getCollectionNames().length!==0) throw Error('leaked clone');",
    )
    .await?;
    Ok(())
}
#[tokio::test]
#[ignore = "requires disposable Redis logical databases 0 and 1"]
async fn redis_isolated_clone_ttl_and_ownership() -> Result<()> {
    let root = tempfile::tempdir()?;
    let mut req = request(Engine::Redis, "DBFORK_TEST_REDIS", root.path());
    req.config.options.insert("target_db".into(), "1".into());
    let mut source = redis::Client::open(req.source.connection_url.as_str())?
        .get_multiplexed_async_connection()
        .await?;
    let _: () = redis::cmd("SET")
        .arg("engine_fixture")
        .arg("source")
        .query_async(&mut source)
        .await?;
    let _: () = redis::cmd("SET")
        .arg("engine_expiring")
        .arg("ttl")
        .arg("PX")
        .arg(60000)
        .query_async(&mut source)
        .await?;
    let r = create(&req).await?;
    validate(&r).await?;
    assert!(create(&req).await.is_err());
    let mut target = redis::Client::open(r.connection_url.as_str())?
        .get_multiplexed_async_connection()
        .await?;
    let value: String = redis::cmd("GET")
        .arg("engine_fixture")
        .query_async(&mut target)
        .await?;
    assert_eq!(value, "source");
    let ttl: i64 = redis::cmd("PTTL")
        .arg("engine_expiring")
        .query_async(&mut target)
        .await?;
    assert!(ttl > 0 && ttl <= 60000);
    let _: () = redis::cmd("SET")
        .arg("engine_fixture")
        .arg("clone")
        .query_async(&mut target)
        .await?;
    delete(&r).await?;
    delete(&r).await?;
    assert!(dbfork_engines::recover(&req).await?.is_none());
    let value: String = redis::cmd("GET")
        .arg("engine_fixture")
        .query_async(&mut source)
        .await?;
    assert_eq!(value, "source");
    let size: i64 = redis::cmd("DBSIZE").query_async(&mut target).await?;
    assert_eq!(size, 0);
    req.strategy = Strategy::SchemaOnly;
    let empty = create(&req).await?;
    let keys: i64 = redis::cmd("DBSIZE").query_async(&mut target).await?;
    assert_eq!(keys, 1); // ownership marker only
    delete(&empty).await?;
    Ok(())
}
