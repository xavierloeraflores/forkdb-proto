# DBFork Proto

A Rust CLI that gives Git branches and worktrees their own databases. It clones a configured source, records the resources it owns, and writes connection URLs into that worktree's environment files. It also works without Git.

## Install and try it

Install current stable Rust, then run:

```sh
cargo install --path crates/cli --locked
mkdir dbfork-demo
cd dbfork-demo
dbfork-proto init
dbfork-proto create feat/auth
dbfork-proto status --check
dbfork-proto create feat/auth-child --from feat/auth
dbfork-proto list
dbfork-proto switch main
dbfork-proto delete feat/auth-child --yes
dbfork-proto delete feat/auth --yes
```

The default configuration uses bundled SQLite and creates an empty source database. Add tables and rows through the source connection in `.env` before cloning to try a populated example.

`create` activates the new connections in the current worktree. `--no-switch` only provisions them. `switch` changes connections without checking out a Git branch. `main` refers to the configured source databases, which DBFork never deletes.

## Existing PostgreSQL project

Set `SOURCE_DATABASE_URL` in your shell or an ignored `.env` file, then run:

```sh
dbfork-proto init --engine postgres --source-env SOURCE_DATABASE_URL
dbfork-proto create feat/auth
dbfork-proto create feat/schema --schema-only
dbfork-proto create feat/child --from feat/auth --json
```

The role needs permission to create databases. Install `pg_dump` and `pg_restore` compatible with the server. Use a development source suitable for copying. Managed services may require a direct connection instead of a transaction pooler.

For a sample application, see [forkdb-proto-test-case](https://github.com/xavierloeraflores/forkdb-proto-test-case), a Next.js task CRUD app using Prisma and PostgreSQL. Its 15 example tasks and documented database baseline provide a starting point for trying separate database forks in Git worktrees.

## Configuration

Commit `.dbfork.toml`. Keep connection URLs and API keys outside it.

```toml
[project]
name = "my-app"
default_branch = "main"
env_files = [".env", "apps/web/.env.local"]
protected_branches = ["main", "master", "production", "prod"]

[[databases]]
name = "primary"
engine = "postgres"
provider = "local"
connection_env = "DATABASE_URL"
source_env = "SOURCE_DATABASE_URL"
strategy = "full"

[[databases]]
name = "cache"
engine = "redis"
provider = "docker"
connection_env = "REDIS_URL"
source_env = "SOURCE_REDIS_URL"
strategy = "instance"
```

Sources are captured in private state before cloning. Later `.env` edits cannot silently redirect the parent. Use a new project for a different source.

`seeded` clones schema and executes `seed = ["seeds/development.sql"]`. Seed scripts are trusted code and must target the child. MongoDB seeds are JavaScript for `mongosh`. Redis has no seed-script strategy; `schema_only` creates an empty owned destination.

## Engines and providers

| Engine | Cloning strategies | Client tools |
| --- | --- | --- |
| SQLite | Full/file backup, schema-only, seeded | None |
| PostgreSQL | Full, schema-only, seeded, database template | `pg_dump`, `pg_restore`; templates use SQLx |
| MySQL / MariaDB | Full, schema-only, seeded | MySQL: `mysqldump`, `mysql`; MariaDB: `mariadb-dump`, `mariadb` |
| MongoDB | Full, schema-only with indexes/options, seeded | `mongodump`, `mongorestore`, `mongosh` |
| Redis | Full values and TTLs, empty destination | Native Redis connection |

For MariaDB use `engine = "mysql"` with `options.mysql_flavor = "mariadb"`. This selects MariaDB client tools and compatible dump flags.

SQLite backups include committed WAL data. Virtual tables require full/file cloning; schema-only fails explicitly. PostgreSQL templates require a same-server source with no active sessions. Redis logical cloning requires an empty dedicated destination database. Redis SCAN and DUMP/RESTORE do not form a single point-in-time snapshot under concurrent writes; quiesce the source when that consistency is required.

Docker `instance` provisions PostgreSQL, MySQL, MongoDB, or Redis, then clones into the container. Ports bind only to localhost, passwords are generated, and deletion checks ownership labels. Docker must run locally. Set `options.instance_clone = "schema_only"` for a schema-only instance, and `options.image` to choose an image.

Neon `native` creates a copy-on-write branch and compute endpoint through its API. `snapshot` uses the same copy-on-write operation. Other cloud providers use compatible logical cloning when permissions allow. Unsupported native branching or snapshots fail explicitly.

See [provider configuration and limits](crates/providers/README.md) for Neon, Supabase, AWS RDS/Aurora, Google Cloud SQL, Azure Database, DigitalOcean, Railway, Render, PlanetScale, MongoDB Atlas, and Upstash. `dbfork-proto capabilities <provider>` returns capabilities; engine restrictions also apply.

## Git worktrees

```sh
dbfork-proto hooks install
git worktree add ../my-app-auth -b feat/auth
cd ../my-app-auth
dbfork-proto status
```

The opt-in post-checkout hook runs `sync` after branch checkout or worktree creation. It preserves existing hooks, respects `core.hooksPath`, and reports database failures without failing Git checkout. Remove it with `dbfork-proto hooks uninstall`.

Without hooks, use `sync` in a worktree or `sync --all` for every worktree. Detached HEADs are skipped. Sync never deletes databases. Removed worktrees leave environments available for explicit deletion.

Worktrees share state and an exclusive operation lock in Git's common directory, with separate environment files in each worktree. The lock waits up to 30 seconds before returning an operation error. No persistent daemon is needed.

## Agent use and cleanup

```sh
dbfork-proto --json create feat/agent --no-switch
dbfork-proto --json list
dbfork-proto --json delete feat/agent --yes
```

Success is `{"ok":true,"result":...}`. Failure is `{"ok":false,"error":{"code":...,"message":...}}`. Output omits connection URLs and credentials. Exit codes are `0` success, `1` operation failure, `2` invalid arguments, and `3` missing or cancelled deletion confirmation. JSON mode never prompts. Repeated create/delete operations are idempotent when state is compatible.

Deletion refuses protected sources and resources still referenced by live worktrees. Switch those worktrees to the default branch first. Internal ownership tables, MongoDB collections, Redis keys, and provider metadata must stay intact.

Provisioning intent is saved before resource allocation. After an interruption, inspect `list` and use `delete <branch> --yes` before retrying. Failed cleanup retains its state. If ownership cannot be proved, DBFork refuses deletion and keeps the intent for inspection.

Environment updates preserve unrelated variables and comments, reject tracked or symlinked destinations, and replace files atomically. A later environment-file failure restores earlier updates. Registry files and new environment files are private on Unix; Windows uses the user's directory ACLs. State contains connection credentials and must stay outside version control.

## Development

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --locked
cargo build --release --locked
```

The workspace separates shared models, configuration/state, Git, environment files, engines, providers, orchestration, and the Clap CLI. Tests invoke the compiled CLI with disposable SQLite databases. [Testing instructions](docs/testing.md) cover disposable network databases and Docker.

CI is configured for macOS ARM64/x64, Windows x64, and Linux ARM64/x64. Cloud API tests use local mocks; live cloud verification requires account credentials.
