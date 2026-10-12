use anyhow::{bail, Context as _, Result};
use std::{
    path::{Path, PathBuf},
    process::{Command, Output},
};

#[derive(Debug, Clone)]
pub struct Context {
    pub root: PathBuf,
    pub common_dir: Option<PathBuf>,
    pub branch: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Worktree {
    pub path: PathBuf,
    pub branch: Option<String>,
}

/// Git for Windows cannot use the verbatim paths returned by canonicalize for
/// every command (notably `worktree add`). Keep those paths for filesystem I/O.
pub fn git_path(path: &Path) -> PathBuf {
    #[cfg(windows)]
    {
        let value = path.to_string_lossy();
        if let Some(unc) = value.strip_prefix(r"\\?\UNC\") {
            return format!(r"\\{}", unc).into();
        }
        if let Some(local) = value.strip_prefix(r"\\?\") {
            return local.into();
        }
    }
    path.to_owned()
}

fn git(dir: &Path, args: &[&str]) -> Result<Output> {
    Command::new("git")
        .arg("-C")
        .arg(git_path(dir))
        .args(args)
        .output()
        .context("could not run git")
}

fn successful(output: Output) -> Result<Vec<u8>> {
    if !output.status.success() {
        bail!(
            "git failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(output.stdout)
}

fn one_line(mut data: Vec<u8>) -> Vec<u8> {
    if data.last() == Some(&b'\n') {
        data.pop();
    }
    data
}

fn path(data: Vec<u8>) -> Result<PathBuf> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;
        Ok(std::ffi::OsString::from_vec(data).into())
    }
    #[cfg(not(unix))]
    {
        Ok(String::from_utf8(data)
            .context("git path is not UTF-8")?
            .into())
    }
}

pub fn discover(dir: &Path) -> Result<Context> {
    let manual = || -> Result<Context> {
        let directory = dir.canonicalize()?;
        let root = directory
            .ancestors()
            .find(|path| path.join(".dbfork.toml").is_file())
            .unwrap_or(&directory)
            .to_owned();
        Ok(Context {
            root,
            common_dir: None,
            branch: None,
        })
    };
    let output = match Command::new("git")
        .arg("-C")
        .arg(git_path(dir))
        .args(["rev-parse", "--show-toplevel"])
        .output()
    {
        Ok(output) => output,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return manual(),
        Err(e) => return Err(e.into()),
    };
    if !output.status.success() {
        if String::from_utf8_lossy(&output.stderr).contains("not a git repository") {
            return manual();
        }
        successful(output)?;
        unreachable!()
    }
    let root = path(one_line(output.stdout))?
        .canonicalize()
        .context("Git worktree root is inaccessible")?;
    let common_dir = path(one_line(successful(git(
        dir,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )?)?))?
    .canonicalize()
    .context("Git common directory is inaccessible")?;
    let branch_output = git(dir, &["symbolic-ref", "--quiet", "--short", "HEAD"])?;
    let branch = if branch_output.status.success() {
        Some(
            String::from_utf8(one_line(branch_output.stdout))
                .context("branch name is not UTF-8")?,
        )
    } else if branch_output.status.code() == Some(1) {
        None
    } else {
        successful(branch_output)?;
        unreachable!()
    };
    Ok(Context {
        root,
        common_dir: Some(common_dir),
        branch,
    })
}

pub fn worktrees(root: &Path) -> Result<Vec<Worktree>> {
    if discover(root)?.common_dir.is_none() {
        return Ok(Vec::new());
    }
    let data = successful(git(root, &["worktree", "list", "--porcelain", "-z"])?)?;
    let mut result = Vec::new();
    let mut current = None;
    for field in data.split(|b| *b == 0) {
        if let Some(value) = field.strip_prefix(b"worktree ") {
            if let Some(tree) = current.take() {
                result.push(tree);
            }
            let location = path(value.to_vec())?;
            match location.canonicalize() {
                Ok(location) => {
                    current = Some(Worktree {
                        path: location,
                        branch: None,
                    })
                }
                // Git can retain prunable records for worktrees removed outside Git.
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => current = None,
                Err(error) => return Err(error).context("Git worktree path is inaccessible"),
            }
        } else if let Some(value) = field.strip_prefix(b"branch refs/heads/") {
            if let Some(tree) = current.as_mut() {
                tree.branch =
                    Some(String::from_utf8(value.to_vec()).context("branch name is not UTF-8")?);
            }
        } else if field == b"bare" {
            current = None;
        }
    }
    if let Some(tree) = current {
        result.push(tree);
    }
    Ok(result)
}

pub fn branches(root: &Path) -> Result<Vec<String>> {
    if discover(root)?.common_dir.is_none() {
        return Ok(Vec::new());
    }
    let data = successful(git(
        root,
        &["for-each-ref", "--format=%(refname:strip=2)", "refs/heads/"],
    )?)?;
    Ok(String::from_utf8(data)
        .context("branch name is not UTF-8")?
        .lines()
        .map(str::to_owned)
        .collect())
}

/// Prefer the remote's declared default, then conventional local source branches.
pub fn default_branch(root: &Path) -> Result<String> {
    let context = discover(root)?;
    if context.common_dir.is_none() {
        return Ok("main".into());
    }
    let remote = git(
        root,
        &["symbolic-ref", "--quiet", "refs/remotes/origin/HEAD"],
    )?;
    if remote.status.success() {
        let reference = String::from_utf8(one_line(remote.stdout))
            .context("remote branch name is not UTF-8")?;
        if let Some(branch) = reference.strip_prefix("refs/remotes/origin/") {
            if !branch.is_empty()
                && git(
                    root,
                    &[
                        "rev-parse",
                        "--verify",
                        "--quiet",
                        "refs/remotes/origin/HEAD^{commit}",
                    ],
                )?
                .status
                .success()
            {
                return Ok(branch.to_owned());
            }
        }
    }
    let local = branches(root)?;
    for name in ["main", "master"] {
        if local.iter().any(|branch| branch == name) {
            return Ok(name.into());
        }
    }
    Ok(context.branch.unwrap_or_else(|| "main".into()))
}

const HOOK_MARKER: &str = "#!/bin/sh\n# dbfork-proto managed post-checkout hook v1\n";
const BACKUP_NAME: &str = ".dbfork-proto-post-checkout.original";

fn hook_path(root: &Path) -> Result<PathBuf> {
    if discover(root)?.common_dir.is_none() {
        bail!("automatic hooks require a Git repository");
    }
    // Git applies core.hooksPath here, including paths relative to the worktree.
    path(one_line(successful(git(
        root,
        &[
            "rev-parse",
            "--path-format=absolute",
            "--git-path",
            "hooks/post-checkout",
        ],
    )?)?))
}

fn shell_quote(value: &Path) -> Result<String> {
    let value = value
        .to_str()
        .context("hook executable path must be UTF-8")?;
    #[cfg(windows)]
    let value = if let Some(unc) = value.strip_prefix(r"\\?\UNC\") {
        format!("//{}", unc.replace('\\', "/"))
    } else {
        value
            .strip_prefix(r"\\?\")
            .unwrap_or(value)
            .replace('\\', "/")
    };
    Ok(format!("'{}'", value.replace('\'', "'\"'\"'")))
}

fn hook_contents(path: &Path) -> Result<Option<Vec<u8>>> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => {
            Ok(Some(std::fs::read(path)?))
        }
        Ok(_) => bail!("refusing non-regular hook file {}", path.display()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

/// Install a wrapper that runs the previous hook and synchronizes the current
/// worktree after branch checkouts. Database failures never fail the checkout.
pub fn install_hooks(root: &Path, executable: &Path) -> Result<()> {
    use std::io::Write;
    let executable = executable
        .canonicalize()
        .context("hook executable does not exist")?;
    if !executable.is_file() {
        bail!("hook executable must be a regular file");
    }
    let hook = hook_path(root)?;
    let directory = hook.parent().context("hook path has no parent")?;
    std::fs::create_dir_all(directory)?;
    if std::fs::symlink_metadata(directory)?
        .file_type()
        .is_symlink()
    {
        bail!("refusing symlink hooks directory");
    }
    let backup = directory.join(BACKUP_NAME);
    let existing = hook_contents(&hook)?;
    let managed = existing
        .as_ref()
        .is_some_and(|bytes| bytes.starts_with(HOOK_MARKER.as_bytes()));
    let previous = hook_contents(&backup)?;
    if !managed && previous.is_some() {
        bail!("hook backup already exists; restore or remove it before installing");
    }
    let script = format!("{HOOK_MARKER}\nprevious_status=0\nif [ -x {backup} ]; then\n  {backup} \"$@\" || previous_status=$?\nfi\n# Git passes 1 for a branch checkout and 0 for a file checkout.\nif [ \"${{3:-0}}\" = 1 ]; then\n  if ! {executable} sync --yes >/dev/null; then\n    printf '%s\\n' 'dbfork-proto: database sync failed; run dbfork-proto sync to retry.' >&2\n  fi\nfi\nexit \"$previous_status\"\n", backup=shell_quote(&backup)?, executable=shell_quote(&executable)?);
    let mut temporary = tempfile::NamedTempFile::new_in(directory)?;
    temporary.write_all(script.as_bytes())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        temporary
            .as_file()
            .set_permissions(std::fs::Permissions::from_mode(0o755))?;
    }
    temporary.as_file().sync_all()?;
    let moved = !managed && existing.is_some();
    if moved {
        std::fs::rename(&hook, &backup)?;
    }
    if let Err(error) = temporary.persist(&hook) {
        if moved {
            std::fs::rename(&backup, &hook)
                .context("hook install failed and original hook could not be restored")?;
        }
        return Err(error.error.into());
    }
    Ok(())
}

/// Remove only our managed wrapper and restore any original hook exactly.
pub fn uninstall_hooks(root: &Path) -> Result<()> {
    let hook = hook_path(root)?;
    let Some(contents) = hook_contents(&hook)? else {
        return Ok(());
    };
    if !contents.starts_with(HOOK_MARKER.as_bytes()) {
        bail!("post-checkout is not managed by dbfork-proto; leaving it unchanged");
    }
    let backup = hook
        .parent()
        .context("hook path has no parent")?
        .join(BACKUP_NAME);
    if hook_contents(&backup)?.is_some() {
        std::fs::rename(backup, hook)?;
    } else {
        std::fs::remove_file(hook)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn run(dir: &Path, args: &[&str]) {
        successful(git(dir, args).unwrap()).unwrap();
    }
    #[test]
    fn manual_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let context = discover(tmp.path()).unwrap();
        assert_eq!(context.root, tmp.path().canonicalize().unwrap());
        assert!(context.common_dir.is_none());
        assert!(worktrees(tmp.path()).unwrap().is_empty());
        assert_eq!(default_branch(tmp.path()).unwrap(), "main");
        std::fs::write(tmp.path().join(".dbfork.toml"), "").unwrap();
        let nested = tmp.path().join("nested").join("deeper");
        std::fs::create_dir_all(&nested).unwrap();
        assert_eq!(
            discover(&nested).unwrap().root,
            tmp.path().canonicalize().unwrap()
        );
    }

    #[test]
    fn default_branch_prefers_resolved_remote_then_main_master_and_current() {
        let tmp = tempfile::tempdir().unwrap();
        run(tmp.path(), &["init", "-b", "master"]);
        assert_eq!(default_branch(tmp.path()).unwrap(), "master");
        run(
            tmp.path(),
            &[
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.com",
                "commit",
                "--allow-empty",
                "-m",
                "initial",
            ],
        );
        run(tmp.path(), &["checkout", "-b", "feature"]);
        assert_eq!(default_branch(tmp.path()).unwrap(), "master");
        run(tmp.path(), &["branch", "main"]);
        assert_eq!(default_branch(tmp.path()).unwrap(), "main");
        run(
            tmp.path(),
            &["update-ref", "refs/remotes/origin/trunk", "HEAD"],
        );
        run(
            tmp.path(),
            &[
                "symbolic-ref",
                "refs/remotes/origin/HEAD",
                "refs/remotes/origin/trunk",
            ],
        );
        assert_eq!(default_branch(tmp.path()).unwrap(), "trunk");
        run(
            tmp.path(),
            &["update-ref", "-d", "refs/remotes/origin/trunk"],
        );
        assert_eq!(default_branch(tmp.path()).unwrap(), "main");
        run(tmp.path(), &["branch", "-D", "main", "master"]);
        assert_eq!(default_branch(tmp.path()).unwrap(), "feature");
        run(tmp.path(), &["checkout", "--detach"]);
        assert_eq!(default_branch(tmp.path()).unwrap(), "main");
    }
    #[test]
    fn branches_worktrees_and_detached_heads() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("repo with spaces");
        std::fs::create_dir(&root).unwrap();
        run(&root, &["init", "-b", "main"]);
        run(
            &root,
            &[
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.com",
                "commit",
                "--allow-empty",
                "-m",
                "initial",
            ],
        );
        let linked = tmp.path().join(if cfg!(unix) {
            "worktree space\nand newline"
        } else {
            "worktree space"
        });
        run(
            &root,
            &[
                "worktree",
                "add",
                "-b",
                "feat/test",
                linked.to_str().unwrap(),
            ],
        );
        let main = discover(&root).unwrap();
        let child = discover(&linked).unwrap();
        assert_eq!(main.common_dir, child.common_dir);
        assert_eq!(child.branch.as_deref(), Some("feat/test"));
        let trees = worktrees(&root).unwrap();
        assert_eq!(trees.len(), 2);
        assert!(trees
            .iter()
            .any(|tree| tree.path == linked.canonicalize().unwrap()
                && tree.branch.as_deref() == Some("feat/test")));
        assert_eq!(branches(&root).unwrap(), vec!["feat/test", "main"]);
        run(&linked, &["checkout", "--detach"]);
        assert!(discover(&linked).unwrap().branch.is_none());
        assert!(worktrees(&root)
            .unwrap()
            .iter()
            .any(|tree| tree.path == linked.canonicalize().unwrap() && tree.branch.is_none()));
        std::fs::remove_dir_all(&linked).unwrap();
        assert_eq!(worktrees(&root).unwrap().len(), 1);
    }

    #[cfg(windows)]
    #[test]
    fn shell_paths_strip_windows_verbatim_prefixes() {
        assert_eq!(
            git_path(Path::new(r"\\?\C:\directory\repo")),
            Path::new(r"C:\directory\repo")
        );
        assert_eq!(
            git_path(Path::new(r"\\?\UNC\server\share\repo")),
            Path::new(r"\\server\share\repo")
        );
        assert_eq!(
            shell_quote(Path::new(r"\\?\C:\directory\binary.exe")).unwrap(),
            "'C:/directory/binary.exe'"
        );
        assert_eq!(
            shell_quote(Path::new(r"\\?\UNC\server\share\binary.exe")).unwrap(),
            "'//server/share/binary.exe'"
        );
    }

    #[test]
    fn hooks_preserve_existing_and_respect_custom_path() {
        let tmp = tempfile::tempdir().unwrap();
        run(tmp.path(), &["init", "-b", "main"]);
        run(tmp.path(), &["config", "core.hooksPath", "custom hooks"]);
        let directory = tmp.path().join("custom hooks");
        std::fs::create_dir(&directory).unwrap();
        let hook = directory.join("post-checkout");
        let original = b"#!/bin/sh\n# original hook\nexit 0\n";
        std::fs::write(&hook, original).unwrap();
        let executable = tmp.path().join("a binary with ' quote");
        std::fs::write(&executable, "#!/bin/sh\nexit 1\n").unwrap();
        install_hooks(tmp.path(), &executable).unwrap();
        let first = std::fs::read(&hook).unwrap();
        install_hooks(tmp.path(), &executable).unwrap();
        assert_eq!(std::fs::read(&hook).unwrap(), first);
        assert_eq!(
            std::fs::read(directory.join(BACKUP_NAME)).unwrap(),
            original
        );
        assert!(String::from_utf8(first).unwrap().contains("sync --yes"));
        uninstall_hooks(tmp.path()).unwrap();
        assert_eq!(std::fs::read(&hook).unwrap(), original);
        assert!(!directory.join(BACKUP_NAME).exists());
        assert!(uninstall_hooks(tmp.path()).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn hook_failure_does_not_fail_checkout_and_preserves_original_hook_status() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        run(tmp.path(), &["init", "-b", "main"]);
        let executable = tmp.path().join("binary ' name");
        std::fs::write(
            &executable,
            "#!/bin/sh\nprintf '%s\\n' \"$*\" > invoked\nexit 1\n",
        )
        .unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755)).unwrap();
        install_hooks(tmp.path(), &executable).unwrap();
        let hook = hook_path(tmp.path()).unwrap();
        let result = Command::new(&hook)
            .current_dir(tmp.path())
            .args(["old", "new", "1"])
            .output()
            .unwrap();
        assert!(result.status.success());
        assert!(String::from_utf8_lossy(&result.stderr).contains("database sync failed"));
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("invoked")).unwrap(),
            "sync --yes\n"
        );
        uninstall_hooks(tmp.path()).unwrap();
        uninstall_hooks(tmp.path()).unwrap();
        std::fs::write(&hook, "#!/bin/sh\nexit 7\n").unwrap();
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
        install_hooks(tmp.path(), &executable).unwrap();
        assert_eq!(
            Command::new(hook)
                .current_dir(tmp.path())
                .args(["old", "new", "1"])
                .status()
                .unwrap()
                .code(),
            Some(7)
        );
    }
}
