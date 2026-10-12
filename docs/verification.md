# Prototype verification

Verification began on October 11, 2026. Commands and fixtures are in the repository so these checks can be repeated.

| Requirement | Implementation and evidence |
| --- | --- |
| Rust, Clap, Tokio, SQLx, TOML, Serde | Workspace manifests, compiled debug and release binaries, strict Clippy |
| Init/create/switch/list/status/delete/sync | CLI subprocess lifecycle tests and release-binary smoke test |
| Parent branches, schema-only, seeds | CLI parent-data assertions; SQLite and network engine integration tests |
| PostgreSQL | Full/schema/seed/template, isolation, failed-seed rollback, recovery and repeated deletion against disposable servers |
| MySQL and MariaDB | Full/schema/seed, data/view isolation, failed-seed rollback, recovery and repeated deletion against MySQL 8.4 and MariaDB 11.4 |
| SQLite | Backup includes WAL data; schema/index/view/trigger preservation; seeds; ownership refusal; filename encoding; actual killed-process recovery |
| MongoDB | Full/schema/seed, collection options and indexes, rollback, recovery and repeated deletion against disposable MongoDB |
| Redis | DUMP/RESTORE values and TTLs, dedicated database ownership, source preservation, rollback/repeated deletion |
| Independent instances | Real Docker PostgreSQL, MySQL, MariaDB, MongoDB and Redis provisioning, connectivity and cleanup tests |
| Native branches and copy-on-write | Neon API adapter; HTTP contracts cover parent selection, operation polling, recovery, errors and ownership; positive create/validate/delete uses a real PostgreSQL validation endpoint |
| All named cloud providers | Capability declarations and compatible engine routing; per-provider permissions and unimplemented native operations documented in the provider README |
| Worktrees and automatic lifecycle | Shared common-directory registry; actual post-checkout hook test creates an isolated database during git worktree add |
| Multiple databases and env files | Successful two-database/two-file CLI test plus rollback when the second database fails |
| Source and production protection | Protected branch checks, engine ownership markers, equivalent-URL source checks, moved-worktree reference checks |
| Safe environment integration | Comment/variable preservation, atomic replacements, effective ignore verification, tracked-file and symlink refusal |
| Concurrent agents, no daemon | Two concurrent CLI create processes produce one owned allocation; shared filesystem lock |
| Machine-readable use | JSON success/errors, idempotent operations, explicit confirmation exit code, secret-argument redaction |
| Interrupted allocations | Durable pending intents; real 64 MiB SQLite copy interrupted by killing the CLI, recovered and recreated successfully |
| Open-source distribution | MIT license and local Cargo installation instructions |

## Runs completed

- `cargo test --workspace --locked` passed on macOS ARM64 with Rust 1.99.
- The same portable suite passed in an official `rust:1.89-bookworm` Linux ARM64 container, including 46 tests and actual CLI subprocesses. This also checked the declared Rust 1.89 minimum. External-service tests were excluded from that portable run and exercised separately.
- `bash tests/integration/docker.sh` passed the PostgreSQL, MySQL, MongoDB, Redis and additional MariaDB engine runs.
- Docker provider integration tests passed for PostgreSQL, MySQL, MariaDB, MongoDB and Redis. Provider-created containers and disposable source servers were removed.
- The positive Neon API lifecycle test passed against local HTTP fixtures and a disposable PostgreSQL 17 endpoint, including real connection validation and child-only deletion.
- Strict workspace Clippy passed with warnings denied.
- The optimized release binary completed init/create/status/switch/delete in a path containing literal `%20`, spaces and `#`.
- The GitHub Actions portable matrix passed on Linux x64/ARM64, macOS x64/ARM64, and Windows x64 at commit `7fe9be3`. Its Windows job used Rust 1.99 MSVC and passed formatting, strict Clippy, 46 portable tests, and the release build. The Windows release CLI lifecycle is also exercised by `tests/integration/windows.ps1` in the CI workflow.

## Verification limits

The connected Windows x64 host has Rust 1.99 and a native GNU linker, but Windows Smart App Control blocks execution of unsigned Cargo build scripts there. The hosted Windows CI runner provides executable verification; this host did not run the CLI. Linux ARM64 and macOS ARM64 also ran locally.

Live cloud accounts were not used. Neon contract tests use local HTTP fixtures; the other cloud providers route to engine adapters and require service-specific database privileges. The implementation does not provision managed AWS, Google Cloud, Azure or other paid instances. Unsupported native branching and snapshots return explicit errors.

Redis SCAN cloning is not a global point-in-time snapshot. Local Redis needs a dedicated empty database index; use Docker instances for multiple independent branches. SQLite virtual tables require full/file cloning. An interrupted server-side allocation without a provable ownership marker is retained for inspection rather than deleted speculatively.
