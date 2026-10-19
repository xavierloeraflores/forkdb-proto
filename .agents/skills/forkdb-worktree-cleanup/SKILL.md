---
name: forkdb-worktree-cleanup
description: Delete corresponding DBFork databases when removing, clearing, or archiving Git worktrees, including cleanup after a worktree was already removed. Preserve sources and environments still in use.
---

# Clean up worktree databases

Treat database cleanup as part of the requested worktree cleanup unless the user asks to retain the data. Scope deletion to environments linked to the worktrees being cleared. The executable is `dbfork-proto`.

## Capture the mapping before removal

From a surviving checkout of the same repository, inspect:

```sh
git -C "$project" worktree list --porcelain -z
dbfork-proto -C "$project" --json list
```

For each target path that still exists, also run:

```sh
dbfork-proto -C "$worktree" --json status
```

Record the canonical worktree path, active DBFork environment, and any matching `list.environments[].worktrees` entries. Parse Git's NUL-delimited output without splitting paths on whitespace. Use DBFork environment names, not directory names or assumptions about the checked-out branch. A detached checkout can have a named database environment; `switch` can also make the database name differ from the Git branch.

Read `.dbfork.toml` for the default and protected branches. Preserve all source environments. Keep a surviving checkout and the shared Git common directory available until database cleanup finishes.

## Remove the worktree, then its databases

1. Complete the authorized worktree removal or archive using the appropriate Git or managed-worktree tool. Preserve work according to that tool's workflow. Do not force-delete dirty worktrees or delete Git branches merely to clean a database.
2. Confirm removal succeeded and the target directory is gone. If removal failed, retain its databases. If an archive keeps the checkout directory, the CLI still regards its binding as live; report the retained database instead of bypassing that check.
3. Refresh Git's worktree list and DBFork's `list`. For each mapped environment, check for other existing paths in its bindings and live worktrees using it. Preserve shared environments; do not switch unrelated worktrees to source just to unblock deletion.
4. Delete each eligible environment once from the surviving checkout:

   ```sh
   dbfork-proto -C "$project" --json delete "$environment" --yes
   ```

   The worktree cleanup request covers these corresponding databases. Do not ask again when that scope is already clear. Honor an explicit data-retention instruction.
5. Read `list` again and verify the environment is absent. Report removed worktrees, deleted environments, and any retained environments with the reason. Repeated deletion may return `deleted: false` when the environment is already absent.

DBFork refuses to delete protected sources, unowned resources, active bindings at existing paths, and databases referenced by live worktree environment files. A refusal is a reason to inspect and retain the resource. Do not bypass it with SQL drops, container removal, registry edits, or deleted ownership markers. If provider cleanup fails, keep the state for retry and report the exact environment that remains.

## Worktrees already removed

Use retained path bindings in `list` or a reliable mapping captured earlier in the task. Do not run `sync` first: it prunes bindings to missing paths and can erase the association needed for cleanup. If the mapping is gone, obtain the intended environment names before deletion. An unbound environment may have been deliberately created with `--no-switch`; absence from Git alone is not evidence that it belongs to this cleanup.

`sync` never deletes databases, and the CLI has no worktree-removal hook. This skill performs explicit deletion while the agent handles cleanup. Do not claim that removing worktrees outside the agent will trigger database deletion.
