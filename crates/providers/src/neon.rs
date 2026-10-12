//! Neon API contract: https://api-docs.neon.tech/reference/createprojectbranch
use anyhow::{bail, ensure, Context, Result};
use dbfork_model::{CloneRequest, Engine, Resource};
use percent_encoding::percent_decode_str;
use reqwest::{Client, Method, StatusCode};
use serde_json::{json, Value};
use sqlx::Connection;
use std::{collections::BTreeMap, time::Duration};
use url::Url;

const API: &str = "https://console.neon.tech/api/v2";

struct Neon {
    client: Client,
    base: String,
    key: String,
}

impl Neon {
    fn from_env(key_env: &str) -> Result<Self> {
        let key =
            std::env::var(key_env).with_context(|| format!("set {key_env} to a Neon API key"))?;
        ensure!(!key.is_empty(), "Neon API key is empty");
        Ok(Self {
            client: Client::builder()
                .timeout(Duration::from_secs(30))
                .redirect(reqwest::redirect::Policy::none())
                .build()?,
            base: API.into(),
            key,
        })
    }

    async fn request(
        &self,
        method: Method,
        path: &str,
        body: Option<&Value>,
        query: &[(&str, &str)],
    ) -> Result<Option<Value>> {
        let mut req = self
            .client
            .request(method.clone(), format!("{}{path}", self.base))
            .bearer_auth(&self.key)
            .query(query);
        if let Some(body) = body {
            req = req.json(body);
        }
        // Never print response bodies or request URLs; both can contain credentials.
        let response = req.send().await.map_err(|_| anyhow::anyhow!("Neon API transport failed; a mutating request may have completed remotely, inspect the project before retrying"))?;
        if response.status() == StatusCode::NOT_FOUND && method == Method::GET {
            return Ok(None);
        }
        if response.status() == StatusCode::NO_CONTENT {
            return Ok(None);
        }
        ensure!(
            response.status().is_success(),
            "Neon API returned HTTP {}",
            response.status().as_u16()
        );
        Ok(Some(
            response
                .json()
                .await
                .context("Neon API returned an invalid JSON response")?,
        ))
    }

    async fn branch(&self, project: &str, branch: &str) -> Result<Option<Value>> {
        self.request(
            Method::GET,
            &format!("/projects/{project}/branches/{branch}"),
            None,
            &[],
        )
        .await
    }

    async fn uri(&self, project: &str, branch: &str, database: &str, role: &str) -> Result<String> {
        let response = self
            .request(
                Method::GET,
                &format!("/projects/{project}/connection_uri"),
                None,
                &[
                    ("branch_id", branch),
                    ("database_name", database),
                    ("role_name", role),
                    ("pooled", "false"),
                ],
            )
            .await?
            .context("Neon connection URI does not exist")?;
        Ok(response
            .get("uri")
            .and_then(Value::as_str)
            .context("Neon response has no connection URI")?
            .into())
    }

    async fn wait_operations(&self, project: &str, response: &Value) -> Result<()> {
        let Some(operations) = response.get("operations").and_then(Value::as_array) else {
            return Ok(());
        };
        let wait = async {
            for operation in operations {
                let id = operation
                    .get("id")
                    .and_then(Value::as_str)
                    .context("Neon operation has no ID")?;
                identifier(id)?;
                loop {
                    let response = self
                        .request(
                            Method::GET,
                            &format!("/projects/{project}/operations/{id}"),
                            None,
                            &[],
                        )
                        .await?
                        .context("Neon operation disappeared")?;
                    match response
                        .pointer("/operation/status")
                        .and_then(Value::as_str)
                    {
                        Some("finished" | "skipped") => break,
                        Some("failed" | "cancelled") => {
                            bail!("Neon operation did not complete successfully")
                        }
                        Some("scheduling" | "running" | "cancelling") => {
                            tokio::time::sleep(Duration::from_millis(500)).await
                        }
                        _ => bail!("Neon operation returned an unknown status"),
                    }
                }
            }
            Ok(())
        };
        tokio::time::timeout(Duration::from_secs(180), wait)
            .await
            .context("Neon provisioning did not finish within 180 seconds")?
    }

    async fn delete_owned(&self, resource: &Resource) -> Result<()> {
        let (project, id, expected_name) = identity(resource)?;
        let Some(response) = self.branch(project, id).await? else {
            return Ok(());
        };
        let branch = response
            .get("branch")
            .context("Neon response has no branch")?;
        check_branch(branch, id, expected_name)?;
        let response = self
            .request(
                Method::DELETE,
                &format!("/projects/{project}/branches/{id}"),
                None,
                &[],
            )
            .await?;
        if let Some(response) = response {
            self.wait_operations(project, &response).await?;
        }
        Ok(())
    }
}

fn identifier(value: &str) -> Result<()> {
    ensure!(
        !value.is_empty()
            && value.len() <= 128
            && value
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-'),
        "invalid Neon API resource identifier"
    );
    Ok(())
}

fn identity(resource: &Resource) -> Result<(&str, &str, &str)> {
    let project = resource
        .metadata
        .get("neon_project_id")
        .context("missing Neon project ownership")?;
    let id = resource
        .metadata
        .get("neon_branch_id")
        .context("missing Neon branch ownership")?;
    let name = resource
        .metadata
        .get("neon_branch_name")
        .context("missing Neon branch ownership name")?;
    identifier(project)?;
    identifier(id)?;
    ensure!(
        name.starts_with("dbfork-"),
        "refusing to access an unowned Neon branch"
    );
    ensure!(
        resource.metadata.get("neon_parent_id") != Some(id),
        "refusing to operate on a source Neon branch"
    );
    Ok((project, id, name))
}

fn check_branch(branch: &Value, id: &str, name: &str) -> Result<()> {
    ensure!(
        branch.get("id").and_then(Value::as_str) == Some(id),
        "Neon branch ID mismatch"
    );
    ensure!(
        branch.get("name").and_then(Value::as_str) == Some(name),
        "Neon branch ownership name mismatch"
    );
    ensure!(
        branch.get("parent_id").and_then(Value::as_str).is_some(),
        "refusing to operate on a root Neon branch"
    );
    ensure!(
        branch.get("default").and_then(Value::as_bool) == Some(false),
        "refusing to operate on a default Neon branch"
    );
    ensure!(
        branch.get("protected").and_then(Value::as_bool) != Some(true),
        "refusing to operate on a protected Neon branch"
    );
    Ok(())
}

fn same_database(left: &str, right: &str) -> Result<bool> {
    let a = Url::parse(left).context("invalid source PostgreSQL URL")?;
    let b = Url::parse(right).context("invalid Neon PostgreSQL URL")?;
    let host = |url: &Url| url.host_str().unwrap_or_default().replace("-pooler.", ".");
    Ok(host(&a) == host(&b)
        && a.port_or_known_default() == b.port_or_known_default()
        && a.path() == b.path()
        && a.username() == b.username())
}

pub async fn create(request: &CloneRequest) -> Result<Resource> {
    let key_env = request
        .config
        .options
        .get("api_key_env")
        .map(String::as_str)
        .unwrap_or("NEON_API_KEY");
    let api = Neon::from_env(key_env)?;
    create_with_api(request, &api).await
}

async fn create_with_api(request: &CloneRequest, api: &Neon) -> Result<Resource> {
    ensure!(
        request.config.engine == Engine::Postgres,
        "Neon supports PostgreSQL"
    );
    let options = &request.config.options;
    let project = options
        .get("project_id")
        .context("Neon native branching requires options.project_id")?;
    identifier(project)?;
    if let Some(source_project) = request.source.metadata.get("neon_project_id") {
        ensure!(
            source_project == project,
            "source branch belongs to another Neon project"
        );
    }
    // Managed parent metadata always wins over the configured root branch.
    let parent = request
        .source
        .metadata
        .get("neon_branch_id")
        .or_else(|| options.get("parent_branch_id"))
        .context("Neon native branching requires options.parent_branch_id for the source branch")?;
    identifier(parent)?;
    let key_env = options
        .get("api_key_env")
        .map(String::as_str)
        .unwrap_or("NEON_API_KEY");
    let source_url =
        Url::parse(&request.source.connection_url).context("invalid PostgreSQL source URL")?;
    let database = options.get("database_name").cloned().unwrap_or_else(|| {
        percent_decode_str(source_url.path().trim_start_matches('/'))
            .decode_utf8_lossy()
            .into_owned()
    });
    let role = options.get("role_name").cloned().unwrap_or_else(|| {
        percent_decode_str(source_url.username())
            .decode_utf8_lossy()
            .into_owned()
    });
    ensure!(
        !database.is_empty() && !role.is_empty(),
        "source URL or options must specify a Neon database_name and role_name"
    );
    let parent_uri = api.uri(project, parent, &database, &role).await?;
    ensure!(
        same_database(&request.source.connection_url, &parent_uri)?,
        "configured Neon parent does not match the source database endpoint, role, or database"
    );
    let name = format!("dbfork-{}", uuid::Uuid::new_v4().simple());
    let body =
        json!({"branch":{"name":name,"parent_id":parent},"endpoints":[{"type":"read_write"}]});
    let response = api.request(Method::POST, &format!("/projects/{project}/branches"), Some(&body), &[]).await
        .with_context(|| format!("creating Neon branch {name}; if outcome is unknown, inspect this branch name before retrying"))?.context("Neon create response was empty")?;
    let id = response
        .pointer("/branch/id")
        .and_then(Value::as_str)
        .with_context(|| {
            format!("Neon response has no branch ID; inspect branch {name} before retrying")
        })?;
    identifier(id)?;
    ensure!(
        id != parent,
        "Neon API returned the source branch instead of a new branch"
    );
    let mut resource = Resource {
        database: request.config.name.clone(),
        engine: Engine::Postgres,
        provider: "neon".into(),
        id: request.target_id.clone(),
        connection_url: String::new(),
        strategy: request.strategy,
        metadata: BTreeMap::from([
            ("neon_project_id".into(), project.clone()),
            ("neon_branch_id".into(), id.into()),
            ("neon_parent_id".into(), parent.clone()),
            ("neon_branch_name".into(), name),
            ("neon_api_key_env".into(), key_env.into()),
        ]),
    };
    let finish = async {
        api.wait_operations(project, &response).await?;
        let uri = api.uri(project, id, &database, &role).await?;
        ensure!(
            !same_database(&uri, &request.source.connection_url)?,
            "Neon returned the source endpoint for the new branch"
        );
        resource.connection_url = uri;
        check_connection(&resource.connection_url).await?;
        Ok(())
    }
    .await;
    if let Err(error) = finish {
        if let Err(cleanup) = api.delete_owned(&resource).await {
            bail!("{error}; Neon rollback failed for branch {id}: {cleanup}");
        }
        return Err(error);
    }
    Ok(resource)
}

async fn check_connection(uri: &str) -> Result<()> {
    tokio::time::timeout(Duration::from_secs(30), async {
        let mut conn = sqlx::PgConnection::connect(uri)
            .await
            .map_err(|_| anyhow::anyhow!("cannot connect to the Neon branch"))?;
        sqlx::query("SELECT 1")
            .execute(&mut conn)
            .await
            .map_err(|_| anyhow::anyhow!("Neon branch validation query failed"))?;
        conn.close()
            .await
            .map_err(|_| anyhow::anyhow!("closing Neon validation connection failed"))?;
        Ok(())
    })
    .await
    .context("Neon branch connection timed out")?
}

pub async fn delete(resource: &Resource) -> Result<()> {
    let key_env = resource
        .metadata
        .get("neon_api_key_env")
        .context("missing Neon API key environment variable name")?;
    Neon::from_env(key_env)?.delete_owned(resource).await
}

pub async fn validate(resource: &Resource) -> Result<()> {
    let (project, id, name) = identity(resource)?;
    let key_env = resource
        .metadata
        .get("neon_api_key_env")
        .context("missing Neon API key environment variable name")?;
    let api = Neon::from_env(key_env)?;
    let response = api
        .branch(project, id)
        .await?
        .context("tracked Neon branch no longer exists")?;
    check_branch(
        response
            .get("branch")
            .context("Neon response has no branch")?,
        id,
        name,
    )?;
    check_connection(&resource.connection_url).await
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parent_matching_supports_pooled_connections() {
        assert!(same_database(
            "postgres://user:a@ep-test-pooler.example/db?sslmode=require",
            "postgres://user:b@ep-test.example/db"
        )
        .unwrap());
        assert!(!same_database(
            "postgres://user:a@ep-other.example/db",
            "postgres://user:b@ep-test.example/db"
        )
        .unwrap());
    }
    #[test]
    fn rejects_source_default_and_foreign_branches() {
        let mut branch =
            json!({"id":"br-test","name":"dbfork-owned","parent_id":"br-parent","default":false});
        assert!(check_branch(&branch, "br-test", "dbfork-owned").is_ok());
        branch["default"] = json!(true);
        assert!(check_branch(&branch, "br-test", "dbfork-owned").is_err());
        branch["default"] = json!(false);
        branch["parent_id"] = Value::Null;
        assert!(check_branch(&branch, "br-test", "dbfork-owned").is_err());
        assert!(identifier("../other-project").is_err());
    }
    #[tokio::test]
    async fn api_http_failures_never_expose_secrets() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buffer = vec![0; 4096];
            let n = socket.read(&mut buffer).await.unwrap();
            let request = String::from_utf8_lossy(&buffer[..n]);
            assert!(request.contains("authorization: Bearer test-key"));
            assert!(request.starts_with("POST /projects/test/branches"));
            let body = "secret-server-password";
            socket.write_all(format!("HTTP/1.1 403 Forbidden\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",body.len(),body).as_bytes()).await.unwrap();
        });
        let api = Neon {
            client: Client::new(),
            base: format!("http://{address}"),
            key: "test-key".into(),
        };
        let err = api
            .request(
                Method::POST,
                "/projects/test/branches",
                Some(&json!({})),
                &[],
            )
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("403"));
        assert!(!err.contains("secret-server-password"));
        assert!(!err.contains("test-key"));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn create_uses_selected_parent_and_rolls_back_failed_operations() {
        use dbfork_model::{DatabaseConfig, Strategy};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let mut branch_name = String::new();
            for step in 0..5 {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut bytes = Vec::new();
                loop {
                    let mut chunk = [0; 4096];
                    let n = socket.read(&mut chunk).await.unwrap();
                    assert!(n > 0);
                    bytes.extend_from_slice(&chunk[..n]);
                    if let Some(split) = bytes.windows(4).position(|s| s == b"\r\n\r\n") {
                        let header = String::from_utf8_lossy(&bytes[..split]).to_lowercase();
                        let length = header
                            .lines()
                            .find_map(|l| l.strip_prefix("content-length: "))
                            .map(|s| s.parse::<usize>().unwrap())
                            .unwrap_or(0);
                        if bytes.len() >= split + 4 + length {
                            break;
                        }
                    }
                }
                let request = String::from_utf8(bytes).unwrap();
                let response = match step {
                    0 => {
                        assert!(request.starts_with(
                            "GET /projects/project-test/connection_uri?branch_id=br-feature&"
                        ));
                        json!({"uri":"postgres://user:secret@ep-parent.example/app"})
                    }
                    1 => {
                        assert!(request.starts_with("POST /projects/project-test/branches"));
                        let body: Value =
                            serde_json::from_str(request.split_once("\r\n\r\n").unwrap().1)
                                .unwrap();
                        assert_eq!(body.pointer("/branch/parent_id").unwrap(), "br-feature");
                        assert_eq!(body.pointer("/endpoints/0/type").unwrap(), "read_write");
                        branch_name = body
                            .pointer("/branch/name")
                            .unwrap()
                            .as_str()
                            .unwrap()
                            .into();
                        json!({"branch":{"id":"br-new"},"operations":[{"id":"op-test"}]})
                    }
                    2 => {
                        assert!(
                            request.starts_with("GET /projects/project-test/operations/op-test")
                        );
                        json!({"operation":{"status":"failed"}})
                    }
                    3 => {
                        assert!(request.starts_with("GET /projects/project-test/branches/br-new"));
                        json!({"branch":{"id":"br-new","name":branch_name,"parent_id":"br-feature","default":false}})
                    }
                    4 => {
                        assert!(
                            request.starts_with("DELETE /projects/project-test/branches/br-new")
                        );
                        json!({"operations":[]})
                    }
                    _ => unreachable!(),
                };
                let body = response.to_string();
                socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",body.len(),body).as_bytes()).await.unwrap();
            }
        });
        let request = CloneRequest {
            config: DatabaseConfig {
                name: "primary".into(),
                engine: Engine::Postgres,
                provider: "neon".into(),
                connection_env: "DATABASE_URL".into(),
                source_env: None,
                strategy: Strategy::Native,
                seed: vec![],
                options: BTreeMap::from([
                    ("project_id".into(), "project-test".into()),
                    ("parent_branch_id".into(), "br-root".into()),
                ]),
            },
            source: Resource {
                database: "primary".into(),
                engine: Engine::Postgres,
                provider: "neon".into(),
                id: "source".into(),
                connection_url: "postgres://user:secret@ep-parent.example/app".into(),
                strategy: Strategy::Native,
                metadata: BTreeMap::from([("neon_branch_id".into(), "br-feature".into())]),
            },
            target_id: "dbf_test".into(),
            data_dir: ".".into(),
            project_dir: ".".into(),
            strategy: Strategy::Native,
        };
        let api = Neon {
            client: Client::new(),
            base: format!("http://{address}"),
            key: "test-key".into(),
        };
        let result = tokio::time::timeout(Duration::from_secs(10), create_with_api(&request, &api))
            .await
            .unwrap();
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("operation did not complete"));
        server.await.unwrap();
    }
}
