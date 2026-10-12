# Provider adapters

Provider capabilities describe implemented operations, not every feature offered by a hosting vendor. The CLI checks both provider and engine capabilities before provisioning. Unsupported native, instance and snapshot requests fail explicitly.

| Provider names | Engines | Provisioning |
| --- | --- | --- |
| `local` | PostgreSQL, MySQL/MariaDB, SQLite, MongoDB, Redis | Existing servers or SQLite files |
| `docker` | PostgreSQL, MySQL/MariaDB, MongoDB, Redis | Dedicated containers for `instance` |
| `neon` | PostgreSQL | Native copy-on-write branches for `native` and `snapshot` |
| `supabase` | PostgreSQL | Logical cloning when database creation is permitted |
| `aws-rds`, `aws`, `rds`, `aurora` | PostgreSQL, MySQL/MariaDB | Logical cloning on existing instances |
| `google-cloud-sql`, `cloud-sql`, `gcp` | PostgreSQL, MySQL | Logical cloning on existing instances |
| `azure`, `azure-database` | PostgreSQL, MySQL | Logical cloning on existing instances |
| `digitalocean`, `railway` | PostgreSQL, MySQL, MongoDB, Redis | Logical cloning on existing services |
| `render` | PostgreSQL, Redis | Logical cloning on existing services |
| `planetscale` | PostgreSQL, MySQL | Logical cloning where privileges permit |
| `mongodb-atlas`, `atlas` | MongoDB | Logical dump/restore |
| `upstash` | Redis | Logical copy to a separately provisioned target |

Logical cloning requires direct network access, matching client tools, and permission to create and populate the destination database. A provider's managed features, such as users, object storage, functions, cluster settings and service configuration, are not copied. PlanetScale Vitess commonly restricts database creation. Use an existing compatible target with suitable privileges or select an implemented provider strategy. Upstash requires a separate target server because it supports only database zero. Set `options.target_server_env = "TARGET_DATABASE_URL"` and provide the target connection URL through that process environment variable. For Upstash also set `options.target_db = "0"`. The CLI resolves credentials into private state, keeping them out of the TOML configuration. Database creation permission does not grant complete managed-service isolation; choose Docker instance provisioning or Neon native branching when an independent service endpoint is needed.

## Docker

```toml
[[databases]]
name = "primary"
engine = "postgres"
provider = "docker"
connection_env = "DATABASE_URL"
source_env = "SOURCE_DATABASE_URL"
strategy = "instance"

[databases.options]
image = "postgres:17-alpine"
instance_clone = "full"
```

The image option is optional. Defaults are PostgreSQL 17, MySQL 8.4, MongoDB 7 and Redis 7. For MariaDB set `engine = "mysql"` and `options.mysql_flavor = "mariadb"`; its default image is `mariadb:11.4`, and logical cloning uses `mariadb-dump` and `mariadb` client tools. `instance_clone` accepts `full` or `schema_only`. The adapter creates an independent container, publishes its database port only on `127.0.0.1` using an available port, waits for readiness, and invokes the engine adapter to populate the database. Parent-to-child cloning reads the selected parent's connection. Client versions must support both source and target server versions.

Container credentials are generated per instance and passed to Docker in a temporary private environment file. Docker administrators can inspect container environments. The adapter suppresses command output containing secrets and verifies random ownership labels before removing a container and its anonymous volumes. Failed provisioning or cloning attempts remove their container; cleanup failures identify the container for manual recovery. Docker Desktop or a working local Docker daemon is required. A remote Docker context is not supported because published connections use the local loopback address.

## Neon

```toml
[[databases]]
name = "primary"
engine = "postgres"
provider = "neon"
connection_env = "DATABASE_URL"
source_env = "SOURCE_DATABASE_URL"
strategy = "native"

[databases.options]
project_id = "your-project-id"
parent_branch_id = "br-your-source"
api_key_env = "NEON_API_KEY"
```

Set `NEON_API_KEY` in the invoking process environment. The API key is never stored in resource metadata. `role_name` and `database_name` options may be supplied; otherwise the source connection URL provides them. The adapter verifies that the configured parent branch resolves to the source endpoint, role and database. A managed parent branch's saved ID overrides `parent_branch_id` when cloning from another DBFork environment.

Both `native` and `snapshot` create a current copy-on-write branch through the [Neon create-branch API](https://api-docs.neon.tech/reference/createprojectbranch), with a dedicated read-write compute endpoint. Snapshot here means Neon copy-on-write branching, not a downloadable backup or arbitrary filesystem snapshot. Point-in-time restoration is not implemented. The adapter waits for provider operations, [retrieves the new connection URI](https://api-docs.neon.tech/reference/getconnectionuri), and executes a PostgreSQL validation query before returning the resource.

Deletion verifies the saved branch ID and deterministic provisioning-intent name. Root, default and protected branches are refused. Neon also prevents deleting parents with child branches; delete children first. The [delete-branch API](https://api-docs.neon.tech/reference/deleteprojectbranch) supplies operation completion status. Provisioning intents are recorded before creation. Recovery locates Docker containers by their intent label or Neon branches by their exact deterministic name, verifies ownership and parent identity, and returns the resource for cleanup. If a transport failure leaves creation's outcome unknown, retry cleanup through the saved intent; the adapter does not blindly retry mutating API calls. Live cloud tests require a disposable project and credentials. Unit tests use local HTTP servers to exercise error redaction, parent selection and failed-create rollback.

For the successful native create, validate and delete contract test, set `DBFORK_TEST_POSTGRES_URL` to a disposable PostgreSQL database, then run `cargo test -p dbfork-providers native_create_validate_delete_with_real_postgres -- --ignored`. The test mocks the provider API and uses real PostgreSQL connections for creation and health validation. It verifies completed provider operations, branch ownership, saved parent metadata, and that deletion targets only the child branch.
