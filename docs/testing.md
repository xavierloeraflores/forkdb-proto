# Testing

## Portable checks

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --locked
cargo build --release --locked
```

The CLI integration suites run real subprocesses against disposable SQLite databases and Git repositories. They cover source protection, parent selection, schema-only and seeded clones, worktree isolation, multi-database rollback, simultaneous create commands, missing-config reinitialization, moved worktrees, environment ignore rules, and JSON errors. Engine tests also cover WAL data, virtual-table restrictions, ownership tampering, and interrupted allocation recovery.

Git hook tests preserve existing hook behavior and exercise paths with spaces and quotes. Environment tests cover comments, duplicate variables, multiline values, permissions, symlink rejection, and tracked files. Neon tests use a local HTTP server to check request payloads, parent selection, operation polling, cleanup, ownership, and credential redaction.

## Disposable Docker databases

With a local Docker daemon, Python 3, and Rust installed:

```sh
bash tests/integration/docker.sh
```

The script creates throwaway PostgreSQL, MySQL, MariaDB, MongoDB, and Redis source containers on random localhost ports. It runs client tools inside those containers, streams dump files across the boundary, executes the ignored engine integration tests, and removes the containers on exit. No production connection is read. The CI database job runs this script.

Docker Desktop credential helpers can block anonymous image downloads. If pulls stall, diagnose Docker independently. A temporary empty Docker client configuration with an explicit local Docker socket can test public pulls without changing the user's saved configuration.

## Existing disposable servers

The ignored engine tests accept these process environment variables:

- `DBFORK_TEST_POSTGRES`: a disposable PostgreSQL database.
- `DBFORK_TEST_MYSQL`: a disposable MySQL database, or MariaDB with `DBFORK_TEST_MYSQL_FLAVOR=mariadb`.
- `DBFORK_TEST_MARIADB`: the separate disposable MariaDB source for provider instance tests.
- `DBFORK_TEST_MONGODB`: a disposable MongoDB database.
- `DBFORK_TEST_REDIS`: a disposable Redis database zero, with database one available.

These tests create, populate, and remove fixture tables and child databases. Use only disposable servers. Install the matching client tools listed in the README, then run:

```sh
cargo test -p dbfork-engines --test network -- --ignored --test-threads=1
```

The provider integration suite creates dedicated Docker containers, validates source independence and ownership checks, and removes its children:

```sh
cargo test -p dbfork-providers --test docker -- --ignored --test-threads=1
```

Provider tests need native client tools capable of connecting to both source and generated destination endpoints. The engine-only Docker client bridge is restricted to its source fixtures and is not a substitute for those tools.

## Platform and cloud scope

The GitHub Actions matrix is configured for Linux x64/ARM64, macOS x64/ARM64, and Windows x64. SQLite is bundled. No cloud credentials are required by the default test suite. A configured CI matrix is not evidence of a completed remote run; inspect Actions results when publishing.

Neon API behavior is tested against local contract fixtures. Live verification requires a disposable Neon project and API key. Other cloud providers use tested engine adapters over their supplied database endpoints. Availability and permissions depend on the service plan; unsupported native operations return explicit errors. The test suite does not provision AWS, Google Cloud, Azure, or other paid managed instances.
