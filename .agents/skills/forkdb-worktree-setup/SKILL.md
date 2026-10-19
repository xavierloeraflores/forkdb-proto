---
name: forkdb-worktree-setup
description: Provision and activate isolated databases when creating, entering, or resuming a new Git worktree in a DBFork-configured project, before running the app or database commands.
---

# Set up a worktree database

Run DBFork in the new worktree before migrations, seeds, tests, or a dev server can write to its databases. The executable in this repository is `dbfork-proto`.

## Establish the target

- Use the actual worktree path returned by Git or the worktree creation tool. Run `git -C "$worktree" rev-parse --show-toplevel` and `git -C "$worktree" branch --show-current`.
- Read the project's `.dbfork.toml`, including its default branch, databases, and environment-file paths. A linked worktree can use configuration from another worktree in the same repository. Do not run `init` merely because the new checkout lacks the file. If the project has no configuration, obtain the intended source and engine before initializing; do not silently create the default SQLite demo.
- Resolve `dbfork-proto` on PATH. When developing the CLI itself, build with `cargo build --locked -p dbfork-proto` from the CLI repository and use its absolute `target/debug/dbfork-proto` path if needed. Keep `-C` pointed at the application worktree.
- Inspect `dbfork-proto -C "$worktree" --json status` and `dbfork-proto -C "$worktree" --json list`. JSON omits connection credentials. Keep environment files and private state out of tool output and version control.

## Provision and verify

For an attached feature branch, run:

```sh
dbfork-proto -C "$worktree" --json sync
dbfork-proto -C "$worktree" --json status --check
```

`sync` creates or reuses the branch's environment and writes every configured environment file in that worktree. It clones the configured source by default. Use `create "$environment" --from "$parent"` instead only when the task calls for cloning an existing DBFork environment. Do not use `--no-switch` when preparing an app to run.

Handle these cases explicitly:

- **Detached HEAD:** `sync` skips it. Reuse a ready, isolated environment already bound to this worktree by running `switch "$environment"`. Otherwise choose a unique name based on the task or worktree, such as `worktree/auth-<unique-suffix>`, verify it is absent from `list`, and run `create "$environment"`. Record the name and path for cleanup. Creating an environment does not create or check out a Git branch.
- **Default or protected branch:** The default branch points at source databases; it is not isolated. If this is a new worktree that needs isolation, create a uniquely named environment as above without changing the Git branch. Do not treat a successful switch to the default branch as a safe target for destructive tests.
- **Existing shared environment:** If `list` associates the intended environment with another live worktree, preserve it. Create a unique environment for this worktree when independent data is required.
- **Failure or incomplete environment:** Inspect `list` and the error. Do not delete and recreate an existing database just to make setup pass. Preserve partial state for recovery and report the blocker before running database-dependent work.

Check the JSON `ok` field and command exit status, then verify that `status.environment` matches the intended name. Every configured database should appear in `health` with `healthy: true`. A successful command with an empty health array or an unhealthy entry does not prove isolation or connectivity. `list` should show the environment as `ready` and bound to the target path.

Restart only relevant app processes after connection files change; existing processes may retain old environment values. Report the worktree, environment name, and health without URLs or passwords.

## Automatic checkout setup

If the task includes enabling Git automation, run `dbfork-proto -C "$project" hooks install` using a durable installed binary. It preserves existing hooks and respects `core.hooksPath`. The hook runs `sync` after checkout or worktree creation, but skips detached HEADs and reports failures without failing Git checkout. Always perform the verification above. Remove the hook with `hooks uninstall` when requested.

This skill guides the agent when it creates or starts work in a worktree. The skill file alone does not register a background event handler.
