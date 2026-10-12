use anyhow::{bail, ensure, Context, Result};
use dbfork_model::{CloneRequest, Engine, Resource, Strategy};
use serde_json::Value;
use std::{collections::BTreeMap, io::Write, process::Output, time::Duration};
use tokio::process::Command;
use uuid::Uuid;

async fn docker(args: &[&str]) -> Result<Output> {
    let mut command = Command::new("docker");
    command.args(args).kill_on_drop(true);
    tokio::time::timeout(Duration::from_secs(300), command.output())
        .await
        .map_err(|_| {
            if args.first() == Some(&"run") {
                anyhow::Error::new(crate::OutcomeUnknown {
                    resource_hint: args
                        .windows(2)
                        .find(|a| a[0] == "--name")
                        .map(|a| a[1])
                        .unwrap_or("Docker container")
                        .into(),
                })
            } else {
                anyhow::anyhow!("Docker command timed out")
            }
        })?
        .context("could not run docker; install Docker and start its daemon")
}

fn engine_settings(engine: Engine) -> Result<(&'static str, u16, &'static str)> {
    Ok(match engine {
        Engine::Postgres => ("postgres:17", 5432, "postgres"),
        Engine::Mysql => ("mysql:8.4", 3306, "mysql"),
        Engine::Mongodb => ("mongo:7", 27017, "mongodb"),
        Engine::Redis => ("redis:7-alpine", 6379, "redis"),
        Engine::Sqlite => bail!("SQLite uses file cloning, not Docker instances"),
    })
}

pub async fn create(request: &CloneRequest) -> Result<Resource> {
    ensure_local_daemon().await?;
    ensure!(recover(request).await?.is_none(), "a Docker container already exists for this provisioning intent; recover it before retrying");
    let (default_image, inner_port, scheme) = engine_settings(request.config.engine)?;
    let default_image = if request.config.engine == Engine::Mysql
        && request
            .config
            .options
            .get("mysql_flavor")
            .is_some_and(|flavor| flavor == "mariadb")
    {
        "mariadb:11.4"
    } else {
        default_image
    };
    let mode = match request
        .config
        .options
        .get("instance_clone")
        .map(String::as_str)
    {
        None | Some("full") => Strategy::Full,
        Some("schema_only") => Strategy::SchemaOnly,
        _ => bail!("instance_clone must be full or schema_only"),
    };
    let token = Uuid::new_v4().simple().to_string();
    let name = format!("dbfork-{token}");
    let password = Uuid::new_v4().simple().to_string();
    // Secrets are passed in a private file, never in process arguments or diagnostics.
    let mut env_file = tempfile::NamedTempFile::new().context("create Docker environment file")?;
    match request.config.engine {
        Engine::Postgres => writeln!(
            env_file,
            "POSTGRES_PASSWORD={password}\nPOSTGRES_USER=postgres\nPOSTGRES_DB=postgres"
        )?,
        Engine::Mysql => writeln!(
            env_file,
            "MYSQL_ROOT_PASSWORD={password}\nMYSQL_ROOT_HOST=%"
        )?,
        Engine::Mongodb => writeln!(
            env_file,
            "MONGO_INITDB_ROOT_USERNAME=dbfork\nMONGO_INITDB_ROOT_PASSWORD={password}"
        )?,
        Engine::Redis => writeln!(env_file, "DBFORK_PASSWORD={password}")?,
        Engine::Sqlite => unreachable!(),
    }
    env_file.flush()?;
    let owner_label = format!("dbfork-proto.owner={token}");
    let db_label = format!("dbfork-proto.database={}", request.config.name);
    let intent_label = format!("dbfork-proto.intent={}", request.target_id);
    let publish = format!("127.0.0.1::{inner_port}");
    let env_path = env_file
        .path()
        .to_str()
        .context("Docker environment path is not UTF-8")?;
    let image = request
        .config
        .options
        .get("image")
        .map(String::as_str)
        .unwrap_or(default_image);
    ensure!(
        !image.starts_with('-') && !image.is_empty(),
        "invalid Docker image"
    );
    let mut args = vec![
        "run",
        "--detach",
        "--name",
        &name,
        "--label",
        &owner_label,
        "--label",
        &db_label,
        "--label",
        &intent_label,
        "--publish",
        &publish,
        "--env-file",
        env_path,
        image,
    ];
    if request.config.engine == Engine::Redis {
        args.extend([
            "sh",
            "-c",
            "printf 'requirepass %s\\n' \"$DBFORK_PASSWORD\" | exec redis-server -",
        ]);
    }
    let output = docker(&args).await;
    let mut shell = Resource {
        database: request.config.name.clone(),
        engine: request.config.engine,
        provider: "docker".into(),
        id: request.target_id.clone(),
        connection_url: String::new(),
        strategy: Strategy::Instance,
        metadata: BTreeMap::from([
            ("docker_container".into(), name.clone()),
            ("docker_owner".into(), token),
        ]),
    };
    let result = async {
        let output = output?;
        ensure!(
            output.status.success(),
            "Docker provisioning failed; check daemon access and image availability"
        );
        // Capture immutable ID rather than later relying on a reusable container name.
        let container = String::from_utf8(output.stdout)?.trim().to_owned();
        ensure!(
            container.len() == 64 && container.bytes().all(|b| b.is_ascii_hexdigit()),
            "Docker returned an invalid container ID"
        );
        shell
            .metadata
            .insert("docker_container".into(), container.clone());
        let port_spec = format!("{inner_port}/tcp");
        let port_output = docker(&["port", &container, &port_spec]).await?;
        ensure!(
            port_output.status.success(),
            "Docker did not publish the database port"
        );
        let port_text = String::from_utf8(port_output.stdout)?;
        let port = port_text
            .trim()
            .strip_prefix("127.0.0.1:")
            .context("Docker port is not bound exclusively to localhost")?
            .parse::<u16>()?;
        let target_url = match request.config.engine {
            Engine::Postgres => format!("{scheme}://postgres:{password}@127.0.0.1:{port}/postgres"),
            Engine::Mysql => format!("{scheme}://root:{password}@127.0.0.1:{port}/mysql"),
            Engine::Mongodb => {
                format!("{scheme}://dbfork:{password}@127.0.0.1:{port}/admin?authSource=admin")
            }
            Engine::Redis => format!("{scheme}://:{password}@127.0.0.1:{port}/0"),
            Engine::Sqlite => unreachable!(),
        };
        wait_ready(&container, request.config.engine).await?;
        let mut clone = request.clone();
        clone.strategy = mode;
        clone
            .config
            .options
            .insert("target_server_url".into(), target_url);
        if request.config.engine == Engine::Redis {
            clone.config.options.insert("target_db".into(), "0".into());
        }
        let mut resource = dbfork_engines::create(&clone).await?;
        resource.provider = "docker".into();
        resource.strategy = Strategy::Instance;
        resource.metadata.extend(shell.metadata.clone());
        Ok(resource)
    }
    .await;
    match result {
        Ok(resource) => Ok(resource),
        Err(error) => {
            if error.downcast_ref::<crate::OutcomeUnknown>().is_some() {
                return Err(error);
            }
            if let Err(cleanup) = delete(&shell).await {
                bail!("{error}; rollback failed for container {name}: {cleanup}");
            }
            Err(error)
        }
    }
}

pub async fn recover(request: &CloneRequest) -> Result<Option<Resource>> {
    let filter = format!("label=dbfork-proto.intent={}", request.target_id);
    let found = docker(&[
        "ps",
        "--all",
        "--no-trunc",
        "--filter",
        &filter,
        "--format",
        "{{.ID}}",
    ])
    .await?;
    ensure!(
        found.status.success(),
        "Docker could not search for an interrupted provisioning intent"
    );
    let text = String::from_utf8(found.stdout)?;
    let ids = text.lines().filter(|s| !s.is_empty()).collect::<Vec<_>>();
    ensure!(
        ids.len() <= 1,
        "multiple containers match the provisioning intent; manual recovery is required"
    );
    let Some(id) = ids.first() else {
        return Ok(None);
    };
    ensure!(
        id.len() == 64 && id.bytes().all(|b| b.is_ascii_hexdigit()),
        "Docker returned an invalid recovery identity"
    );
    let inspected = docker(&[
        "inspect",
        "--type",
        "container",
        "--format",
        "{{json .Config.Labels}}",
        id,
    ])
    .await?;
    ensure!(
        inspected.status.success(),
        "Docker recovery could not inspect the tracked container"
    );
    let labels: Value = serde_json::from_slice(&inspected.stdout)?;
    let owner = labels
        .get("dbfork-proto.owner")
        .and_then(Value::as_str)
        .context("recovered Docker container has no ownership label")?;
    ensure!(
        owner.len() == 32 && owner.bytes().all(|b| b.is_ascii_hexdigit()),
        "invalid recovered Docker ownership token"
    );
    ensure!(
        labels.get("dbfork-proto.intent").and_then(Value::as_str) == Some(&request.target_id),
        "Docker recovery intent mismatch"
    );
    ensure!(
        labels.get("dbfork-proto.database").and_then(Value::as_str) == Some(&request.config.name),
        "Docker recovery database mismatch"
    );
    Ok(Some(Resource {
        database: request.config.name.clone(),
        engine: request.config.engine,
        provider: "docker".into(),
        id: request.target_id.clone(),
        connection_url: String::new(),
        strategy: Strategy::Instance,
        metadata: BTreeMap::from([
            ("docker_container".into(), (*id).into()),
            ("docker_owner".into(), owner.into()),
        ]),
    }))
}

async fn ensure_local_daemon() -> Result<()> {
    let host = if let Ok(host) = std::env::var("DOCKER_HOST") {
        host
    } else {
        let output = docker(&[
            "context",
            "inspect",
            "--format",
            "{{json .Endpoints.docker.Host}}",
        ])
        .await?;
        ensure!(
            output.status.success(),
            "cannot inspect the active Docker context"
        );
        serde_json::from_slice::<String>(&output.stdout)
            .context("invalid Docker context endpoint")?
    };
    ensure!(host.starts_with("unix://") || host.starts_with("npipe://"), "Docker instance provisioning requires a local Docker context using a Unix socket or Windows named pipe");
    Ok(())
}

async fn wait_ready(container: &str, engine: Engine) -> Result<()> {
    let health = match engine {
        Engine::Postgres => "pg_isready -h 127.0.0.1 -U postgres -d postgres",
        Engine::Mysql => "client=$(command -v mysql || command -v mariadb) && MYSQL_PWD=\"$MYSQL_ROOT_PASSWORD\" \"$client\" --protocol=tcp -h 127.0.0.1 -u root -Nse 'SELECT 1'",
        Engine::Mongodb => "mongosh --quiet --host 127.0.0.1 --eval 'if(!db.getSiblingDB(\"admin\").auth(process.env.MONGO_INITDB_ROOT_USERNAME,process.env.MONGO_INITDB_ROOT_PASSWORD)){quit(1)}; if(!db.adminCommand({ping:1}).ok){quit(1)}'",
        Engine::Redis => "REDISCLI_AUTH=\"$DBFORK_PASSWORD\" redis-cli ping | grep -q PONG",
        Engine::Sqlite => unreachable!(),
    };
    let ready = async {
        loop {
            if docker(&["exec", container, "sh", "-c", health])
                .await?
                .status
                .success()
            {
                return Ok(());
            }
            let state = docker(&["inspect", "--format", "{{.State.Running}}", container]).await?;
            ensure!(state.status.success() && state.stdout == b"true\n", "database container exited before becoming ready; check image and host compatibility");
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    };
    tokio::time::timeout(Duration::from_secs(120), ready)
        .await
        .context("database container did not become ready within 120 seconds")?
}

async fn owned_container(resource: &Resource) -> Result<Option<String>> {
    let id = resource
        .metadata
        .get("docker_container")
        .context("resource is missing Docker container ownership")?;
    ensure!(
        !id.starts_with('-') && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'),
        "invalid Docker container ID"
    );
    let owner = resource
        .metadata
        .get("docker_owner")
        .context("resource is missing Docker ownership token")?;
    let output = docker(&[
        "inspect",
        "--type",
        "container",
        "--format",
        "{{json .Config.Labels}}",
        id,
    ])
    .await?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.contains("No such object:") || stderr.contains("No such container:") {
            return Ok(None);
        }
        bail!("Docker could not inspect the tracked container; ensure its daemon is available");
    }
    let labels: Value = serde_json::from_slice(&output.stdout).context("invalid Docker labels")?;
    ensure!(
        labels.get("dbfork-proto.owner").and_then(Value::as_str) == Some(owner),
        "refusing to access a Docker container without matching ownership"
    );
    ensure!(
        labels.get("dbfork-proto.database").and_then(Value::as_str) == Some(&resource.database),
        "Docker database ownership mismatch"
    );
    Ok(Some(id.clone()))
}

pub async fn delete(resource: &Resource) -> Result<()> {
    if let Some(id) = owned_container(resource).await? {
        let output = docker(&["rm", "--force", "--volumes", &id]).await?;
        ensure!(
            output.status.success(),
            "Docker could not remove the owned container"
        );
    }
    Ok(())
}

pub async fn validate(resource: &Resource) -> Result<()> {
    let id = owned_container(resource)
        .await?
        .context("tracked Docker container no longer exists")?;
    let output = docker(&["inspect", "--format", "{{.State.Running}}", &id]).await?;
    ensure!(
        output.status.success() && output.stdout == b"true\n",
        "tracked Docker container is stopped"
    );
    dbfork_engines::validate(resource).await
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn sqlite_cannot_allocate_a_container() {
        assert!(engine_settings(Engine::Sqlite).is_err());
    }
}
