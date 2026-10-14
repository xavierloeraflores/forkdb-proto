---
name: forkdb-diagnose
description: Diagnose failed DBFork provisioning, unhealthy worktree databases, incorrect active connections, or refused cleanup. Use for a DBFork failure rather than routine setup or unrelated database debugging.
---

# Diagnose a DBFork environment

Start with the affected application worktree and the actual `dbfork-proto` executable. Keep connection URLs, environment-file contents, and private registry credentials out of logs and responses.

```sh
git -C "$worktree" branch --show-current
git -C "$project" worktree list --porcelain -z
dbfork-proto -C "$worktree" --json status --check
dbfork-proto -C "$project" --json list
```

Read `.dbfork.toml` for configured databases, providers, strategies, and environment-file destinations. A linked worktree can use configuration from another checkout. Compare the Git branch, `status.environment`, readiness, health entries, and recorded worktree bindings. Check `ok` and the exit code; unhealthy health entries can accompany a successful status command.

Choose the repair that matches the evidence:

- A detached worktree is skipped by `sync`. Activate its known environment with `switch`, or provision a uniquely named environment when setup is authorized.
- A wrong or missing active binding can be repaired with `switch "$environment"` for a known ready environment. Use `sync` for branch-based setup only after confirming that the branch is the intended database target. Switching to the configured default branch activates source databases.
- A `ready` environment with failed health needs connectivity or ownership investigation. Check the configured engine's client tools, server access, or Docker availability. Use `capabilities "$provider"` for provider support. Do not replace the source or reinitialize the project to conceal a connection failure.
- Tracked or symlinked environment destinations are rejected. Fix the intended ignored, regular destination within the task's scope. Preserve unrelated variables and do not copy an entire source `.env` over a child environment.
- An interrupted create can retain a provisioning intent or partial resources. Inspect its status and bindings. When disposal of that failed environment is already authorized, use `delete "$environment" --yes` and verify it is absent before retrying creation. Otherwise get approval to discard its data. Do not retry an unchanged failure in a loop.
- A deletion refusal may identify a live binding or environment-file reference, including a moved worktree. Retain the environment unless those worktrees are also part of the authorized cleanup. Never edit state or ownership metadata to force deletion.
- An operation lock timeout means another operation may be running. Check that operation before retrying; the lock normally waits up to 30 seconds. Do not remove private state or kill unrelated processes.

After a repair, rerun `status --check` for an active environment or `list` for deletion. Verify all configured databases are healthy before restarting database-dependent work. Empty health output for the default source does not establish source health. Report the observed failure, the repair, and any remaining blocker without credentials.
