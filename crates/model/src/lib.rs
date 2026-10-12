use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, path::PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Engine { Postgres, Mysql, Sqlite, Mongodb, Redis }

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Strategy { Full, SchemaOnly, Seeded, Template, Native, File, Instance, Snapshot }

impl Default for Strategy { fn default() -> Self { Self::Full } }

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DatabaseConfig {
    pub name: String,
    pub engine: Engine,
    pub provider: String,
    pub connection_env: String,
    /// Stable source URL environment variable. Defaults to connection_env at init.
    #[serde(default)] pub source_env: Option<String>,
    #[serde(default)] pub strategy: Strategy,
    #[serde(default)] pub seed: Vec<PathBuf>,
    #[serde(default)] pub options: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Resource {
    pub database: String,
    pub engine: Engine,
    pub provider: String,
    pub id: String,
    pub connection_url: String,
    pub strategy: Strategy,
    #[serde(default)] pub metadata: BTreeMap<String, String>,
}

#[derive(Debug, Clone)]
pub struct CloneRequest {
    pub config: DatabaseConfig,
    pub source: Resource,
    pub target_id: String,
    pub data_dir: PathBuf,
    pub project_dir: PathBuf,
    pub strategy: Strategy,
}

#[derive(Debug, Clone, Serialize)]
pub struct Capabilities {
    pub provider: String,
    pub engines: Vec<Engine>,
    pub strategies: Vec<Strategy>,
    pub notes: String,
}
