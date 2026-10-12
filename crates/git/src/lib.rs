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

fn git(dir: &Path, args: &[&str]) -> Result<Output> {
    Command::new("git")
        .arg("-C")
        .arg(dir)
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
        Ok(Context {
            root: dir.canonicalize()?,
            common_dir: None,
            branch: None,
        })
    };
    let output = match Command::new("git")
        .arg("-C")
        .arg(dir)
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
    let root = path(one_line(output.stdout))?;
    let common_dir = path(one_line(successful(git(
        dir,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )?)?))?;
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
            current = Some(Worktree {
                path: path(value.to_vec())?,
                branch: None,
            });
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
    }
}
