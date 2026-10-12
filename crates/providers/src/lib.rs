//! Provider capabilities and provisioning. Logical cloning lives in dbfork-engines.
use anyhow::{bail, Result};
use dbfork_model::{Capabilities, CloneRequest, Engine, Resource, Strategy};

mod docker;
mod neon;

pub fn capabilities(provider: &str) -> Result<Capabilities> {
    use Engine::*;
    use Strategy::*;
    let logical = vec![Full, SchemaOnly, Seeded];
    let (engines, strategies, notes) = match provider {
        "local" => (vec![Postgres, Mysql, Sqlite, Mongodb, Redis], vec![Full, SchemaOnly, Seeded, Template, File], "Existing server or SQLite file; engine-specific privileges and client tools are required."),
        "docker" => (vec![Postgres, Mysql, Mongodb, Redis], vec![Full, SchemaOnly, Seeded, Template, Instance], "Instance provisions a dedicated container on a random localhost port. Logical strategies require an existing source server and create another database there."),
        "neon" => (vec![Postgres], vec![Full, SchemaOnly, Seeded, Native, Snapshot], "Native and snapshot create a copy-on-write Neon branch with dedicated compute. Logical clones require CREATE DATABASE permission."),
        "supabase" => (vec![Postgres], logical, "PostgreSQL logical cloning where CREATE DATABASE and direct connections are permitted. Supabase Auth, Storage, edge functions, and project configuration are not cloned. Native project branching is not implemented."),
        "aws" | "aws-rds" | "rds" | "aurora" => (vec![Postgres, Mysql], logical, "Logical database cloning on an existing RDS/Aurora instance with sufficient privileges. Instance provisioning and snapshots are not implemented."),
        "google-cloud-sql" | "cloud-sql" | "gcp" => (vec![Postgres, Mysql], logical, "Logical database cloning through an accessible Cloud SQL connection with CREATE DATABASE permission. Cloud SQL instance cloning is not implemented."),
        "azure" | "azure-database" => (vec![Postgres, Mysql], logical, "Logical database cloning through a direct accessible Azure Database connection with CREATE DATABASE permission. Managed instance provisioning is not implemented."),
        "digitalocean" => (vec![Postgres, Mysql, Mongodb, Redis], logical, "Logical cloning when the service account can create databases. Redis needs a distinct writable target database or separately provisioned target_server_url. Cluster provisioning is not implemented."),
        "railway" => (vec![Postgres, Mysql, Mongodb, Redis], logical, "Logical clones use an accessible existing service with database creation privileges. Service provisioning is not implemented."),
        "render" => (vec![Postgres, Redis], logical, "Logical clones require permissions on the existing service. Redis requires a distinct writable database or target_server_url. Service provisioning is not implemented."),
        "planetscale" => (vec![Mysql, Postgres], logical, "Logical cloning only when CREATE DATABASE and dump/restore permissions are available, including a separately provisioned target_server_url. Vitess plans may prohibit CREATE DATABASE. Native branches are not implemented."),
        "mongodb-atlas" | "atlas" => (vec![Mongodb], logical, "Logical MongoDB dump/restore requires database creation and restore privileges. Indexes and collection options are preserved; cluster settings and users are not cloned."),
        "upstash" => (vec![Redis], logical, "Requires a separately provisioned writable Redis target_server_url and target_db=0; Upstash does not support multiple logical database indexes. Native provisioning is not implemented."),
        _ => bail!("unknown provider '{provider}'; run providers to see supported providers"),
    };
    Ok(Capabilities {
        provider: provider.into(),
        engines,
        strategies,
        notes: notes.into(),
    })
}

pub async fn create(request: &CloneRequest) -> Result<Resource> {
    let caps = capabilities(&request.config.provider)?;
    if !caps.engines.contains(&request.config.engine)
        || !caps.strategies.contains(&request.strategy)
    {
        bail!(
            "provider '{}' does not support {:?} for {:?}. {}",
            request.config.provider,
            request.strategy,
            request.config.engine,
            caps.notes
        );
    }
    match (request.config.provider.as_str(), request.strategy) {
        ("docker", Strategy::Instance) => docker::create(request).await,
        ("neon", Strategy::Native | Strategy::Snapshot) => neon::create(request).await,
        _ => bail!("provider provisioning is unavailable for this strategy; logical cloning must use the engine adapter"),
    }
}

pub async fn delete(resource: &Resource) -> Result<()> {
    match (resource.provider.as_str(), resource.strategy) {
        ("docker", Strategy::Instance) => docker::delete(resource).await,
        ("neon", Strategy::Native | Strategy::Snapshot) => neon::delete(resource).await,
        _ => bail!("provider cannot delete this resource strategy"),
    }
}

pub async fn validate(resource: &Resource) -> Result<()> {
    match (resource.provider.as_str(), resource.strategy) {
        ("docker", Strategy::Instance) => docker::validate(resource).await,
        ("neon", Strategy::Native | Strategy::Snapshot) => neon::validate(resource).await,
        _ => bail!("provider cannot validate this resource strategy"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn provider_matrix_does_not_invent_native_support() {
        for provider in [
            "local",
            "docker",
            "supabase",
            "aws-rds",
            "google-cloud-sql",
            "azure",
            "digitalocean",
            "railway",
            "render",
            "planetscale",
            "mongodb-atlas",
            "upstash",
        ] {
            let caps = capabilities(provider).unwrap();
            assert!(!caps.strategies.contains(&Strategy::Native), "{provider}");
            assert!(!caps.strategies.contains(&Strategy::Snapshot), "{provider}");
        }
        assert!(capabilities("neon")
            .unwrap()
            .strategies
            .contains(&Strategy::Snapshot));
        assert!(capabilities("invalid").is_err());
    }
}
