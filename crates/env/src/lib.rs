use anyhow::{bail, Context, Result};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, OpenOptions},
    io::{Read, Write},
    path::{Component, Path},
    process::Command,
};

/// Read dotenv values without changing this process's environment.
pub fn read(path: &Path) -> Result<BTreeMap<String, String>> {
    let Some(text) = read_text(path)? else {
        return Ok(BTreeMap::new());
    };
    dotenvy::from_read_iter(text.as_bytes())
        .collect::<std::result::Result<_, _>>()
        .map_err(|_| anyhow::anyhow!("invalid environment file {}", path.display()))
}

fn read_text(path: &Path) -> Result<Option<String>> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if !metadata.is_file() || metadata.file_type().is_symlink() => {
            bail!("refusing non-regular environment file {}", path.display())
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let mut file = options.open(path)?;
    if !file.metadata()?.is_file() {
        bail!("refusing non-regular environment file");
    }
    let mut text = String::new();
    file.read_to_string(&mut text)?;
    Ok(Some(text))
}

fn valid_key(key: &str) -> bool {
    let mut chars = key.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn assignment(line: &str) -> Option<(&str, usize)> {
    let mut prefix = line.trim_start();
    if let Some(rest) = prefix.strip_prefix("export") {
        if rest.starts_with(char::is_whitespace) {
            prefix = rest.trim_start();
        }
    }
    let (key, _) = prefix.split_once('=')?;
    let key = key.trim_end();
    valid_key(key).then_some((key, line.find('=')? + 1))
}

/// Split logical assignments so quoted multiline values are replaced together.
fn records(text: &str) -> Vec<(&str, Option<&str>, Option<&str>)> {
    let mut result = Vec::new();
    let mut offset = 0;
    while offset < text.len() {
        let remaining = &text[offset..];
        let first_end = remaining.find('\n').map_or(remaining.len(), |n| n + 1);
        let Some((key, value_start)) = assignment(&remaining[..first_end]) else {
            result.push((&remaining[..first_end], None, None));
            offset += first_end;
            continue;
        };
        let mut quote = None;
        let mut escaped = false;
        let mut comment = None;
        let mut end = remaining.len();
        for (index, ch) in remaining[value_start..].char_indices() {
            let index = index + value_start;
            if escaped {
                escaped = false;
                continue;
            }
            if ch == '\\' && quote != Some('\'') {
                escaped = true;
                continue;
            }
            if let Some(current) = quote {
                if ch == current {
                    quote = None;
                }
            } else if ch == '\'' || ch == '"' {
                quote = Some(ch);
            } else if ch == '#'
                && (index == value_start || remaining.as_bytes()[index - 1].is_ascii_whitespace())
            {
                let comment_end = remaining[index..]
                    .find('\n')
                    .map_or(remaining.len(), |n| index + n);
                comment = Some(remaining[index..comment_end].trim_end_matches('\r'));
                end = if comment_end < remaining.len() {
                    comment_end + 1
                } else {
                    comment_end
                };
                break;
            } else if ch == '\n' {
                end = index + 1;
                break;
            }
        }
        result.push((&remaining[..end], Some(key), comment));
        offset += end;
    }
    result
}

fn quote(value: &str) -> String {
    // Single quotes keep literal dollar signs and backslashes compatible with
    // dotenv readers that do not implement shell-style double-quote escapes.
    if !value.contains(['\'', '\n', '\r']) {
        return format!("'{value}'");
    }
    let mut output = String::from("\"");
    for ch in value.chars() {
        match ch {
            '\\' => output.push_str("\\\\"),
            '"' => output.push_str("\\\""),
            '$' => output.push_str("\\$"),
            '\n' => output.push_str("\\n"),
            '\r' => output.push('\r'),
            _ => output.push(ch),
        }
    }
    output.push('"');
    output
}

/// Atomically replace selected keys, preserving other assignments and comments.
/// Callers coordinate concurrent updates with their project-level lock.
pub fn update(path: &Path, values: &BTreeMap<String, String>) -> Result<()> {
    for key in values.keys() {
        if !valid_key(key) {
            bail!("invalid environment variable name {key}");
        }
    }
    let original = read_text(path)?.unwrap_or_default();
    // Validate before editing, so malformed multiline records cannot hide keys.
    let _: BTreeMap<String, String> = dotenvy::from_read_iter(original.as_bytes())
        .collect::<std::result::Result<_, _>>()
        .map_err(|_| anyhow::anyhow!("cannot update malformed environment file"))?;
    let newline = if original.contains("\r\n") {
        "\r\n"
    } else {
        "\n"
    };
    let mut output = String::new();
    let mut written = BTreeSet::new();
    for (record, key, comment) in records(&original) {
        if let Some((key, value)) = key.and_then(|key| values.get(key).map(|value| (key, value))) {
            if written.insert(key.to_owned()) {
                output.push_str(key);
                output.push('=');
                output.push_str(&quote(value));
                if let Some(comment) = comment {
                    output.push(' ');
                    output.push_str(comment);
                }
                output.push_str(newline);
            } else if let Some(comment) = comment {
                output.push_str(comment);
                output.push_str(newline);
            }
        } else {
            output.push_str(record);
        }
    }
    for (key, value) in values {
        if !written.contains(key) {
            if !output.is_empty() && !output.ends_with('\n') {
                output.push_str(newline);
            }
            output.push_str(key);
            output.push('=');
            output.push_str(&quote(value));
            output.push_str(newline);
        }
    }
    atomic_write(path, output.as_bytes())
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    if fs::symlink_metadata(parent)?.file_type().is_symlink() {
        bail!("refusing symlink parent {}", parent.display());
    }
    let previous = match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => Some(metadata),
        Ok(_) => bail!("refusing non-regular file {}", path.display()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    if let Some(metadata) = previous {
        temporary
            .as_file()
            .set_permissions(metadata.permissions())?;
    }
    #[cfg(unix)]
    if !path.exists() {
        use std::os::unix::fs::PermissionsExt;
        temporary
            .as_file()
            .set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    temporary.write_all(bytes)?;
    temporary.as_file().sync_all()?;
    temporary.persist(path).map_err(|error| error.error)?;
    #[cfg(unix)]
    fs::File::open(parent)?.sync_all()?;
    Ok(())
}

/// Keep secret files out of Git using repository-local exclude rules.
/// Existing tracked files are rejected since ignore rules cannot protect them.
pub fn ensure_ignored(root: &Path, relative: &Path) -> Result<()> {
    if relative.as_os_str().is_empty()
        || relative
            .components()
            .any(|c| !matches!(c, Component::Normal(_)))
    {
        bail!("ignored path must be relative and cannot contain parent traversal");
    }
    let value = relative.to_str().context("ignored path must be UTF-8")?;
    if value.contains(['\n', '\r']) {
        bail!("ignored path cannot contain line breaks");
    }
    let repository = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["rev-parse", "--git-dir"])
        .output();
    let in_git = match repository {
        Ok(output) => output.status.success(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => return Err(error.into()),
    };
    let destination = if in_git {
        let tracked = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(["ls-files", "--error-unmatch", "--"])
            .arg(relative)
            .output()?;
        if tracked.status.success() {
            bail!(
                "refusing to write credentials to tracked file {}",
                relative.display()
            );
        }
        let output = Command::new("git")
            .arg("-C")
            .arg(root)
            .args([
                "rev-parse",
                "--path-format=absolute",
                "--git-path",
                "info/exclude",
            ])
            .output()?;
        if !output.status.success() {
            bail!("cannot resolve repository exclude file");
        }
        let mut output = output.stdout;
        if output.last() == Some(&b'\n') {
            output.pop();
        }
        #[cfg(unix)]
        let destination = {
            use std::os::unix::ffi::OsStringExt;
            std::path::PathBuf::from(std::ffi::OsString::from_vec(output))
        };
        #[cfg(not(unix))]
        let destination = std::path::PathBuf::from(String::from_utf8(output)?);
        destination
    } else {
        root.join(".gitignore")
    };
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut rule = String::from("/");
    let normalized = relative
        .components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/");
    for ch in normalized.chars() {
        if matches!(ch, '\\' | '*' | '?' | '[' | ']' | '!' | '#' | ' ') {
            rule.push('\\');
        }
        rule.push(ch);
    }
    let mut text = read_text(&destination)?.unwrap_or_default();
    if !text.lines().any(|line| line == rule) {
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str(&rule);
        text.push('\n');
        atomic_write(&destination, text.as_bytes())?;
    }
    if in_git {
        // A repository .gitignore negation has higher precedence than
        // info/exclude. Verify protection before the caller writes secrets.
        let ignored = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(["check-ignore", "--no-index", "--quiet", "--"])
            .arg(relative)
            .output()?;
        if !ignored.status.success() {
            bail!("Git ignore rules do not protect {}; remove overriding ignore negations before writing credentials", relative.display());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn updates_roundtrip_and_preserve_comments_multiline_and_duplicates() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(".env");
        fs::write(&path, "# keep\r\nOTHER='a\nb'\r\nexport URL=old # connection\r\nURL=duplicate # duplicate comment\r\nTAIL=yes\r\n").unwrap();
        let value = "postgres://user:p$a#\\\" ss@localhost/db?x='&a=1\nnext\r";
        update(
            &path,
            &BTreeMap::from([("URL".into(), value.into()), ("NEW".into(), "ok".into())]),
        )
        .unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("# keep\r\nOTHER='a\nb'\r\n"));
        assert!(text.contains("# connection\r\n# duplicate comment\r\nTAIL=yes\r\n"));
        assert_eq!(text.matches("URL=").count(), 1);
        let values = read(&path).unwrap();
        assert_eq!(values["URL"], value);
        assert_eq!(values["OTHER"], "a\nb");
        assert_eq!(values["NEW"], "ok");
    }
    #[test]
    fn replaces_multiline_target_and_preserves_no_final_newline() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(".env");
        fs::write(&path, "URL=\"one\ntwo\"\nOTHER=value").unwrap();
        update(&path, &BTreeMap::from([("URL".into(), "new".into())])).unwrap();
        assert_eq!(fs::read_to_string(path).unwrap(), "URL='new'\nOTHER=value");
    }
    #[test]
    fn literal_dollar_and_backslash_use_single_quotes() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(".env");
        let value = "postgres://test:pa$ss\\word@localhost/db#fragment";
        update(&path, &BTreeMap::from([("URL".into(), value.into())])).unwrap();
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            format!("URL='{value}'\n")
        );
        assert_eq!(read(&path).unwrap()["URL"], value);
    }
    #[test]
    fn manual_ignore_is_idempotent() {
        let temp = tempfile::tempdir().unwrap();
        ensure_ignored(temp.path(), Path::new(".env.local")).unwrap();
        ensure_ignored(temp.path(), Path::new(".env.local")).unwrap();
        assert_eq!(
            fs::read_to_string(temp.path().join(".gitignore")).unwrap(),
            "/.env.local\n"
        );
        assert!(ensure_ignored(temp.path(), Path::new("../outside")).is_err());
    }
    #[test]
    fn git_ignore_uses_local_exclude_and_rejects_tracked_files() {
        let temp = tempfile::tempdir().unwrap();
        assert!(Command::new("git")
            .arg("init")
            .arg(temp.path())
            .output()
            .unwrap()
            .status
            .success());
        ensure_ignored(temp.path(), Path::new(".env.local")).unwrap();
        assert!(!temp.path().join(".gitignore").exists());
        assert!(Command::new("git")
            .arg("-C")
            .arg(temp.path())
            .args(["check-ignore", ".env.local"])
            .output()
            .unwrap()
            .status
            .success());
        fs::write(temp.path().join("tracked"), "").unwrap();
        assert!(Command::new("git")
            .arg("-C")
            .arg(temp.path())
            .args(["add", "tracked"])
            .output()
            .unwrap()
            .status
            .success());
        assert!(ensure_ignored(temp.path(), Path::new("tracked")).is_err());
    }
    #[test]
    fn git_ignore_negation_cannot_expose_credentials() {
        let temp = tempfile::tempdir().unwrap();
        assert!(Command::new("git")
            .arg("init")
            .arg(temp.path())
            .output()
            .unwrap()
            .status
            .success());
        fs::write(temp.path().join(".gitignore"), "!.env\n").unwrap();
        assert!(ensure_ignored(temp.path(), Path::new(".env")).is_err());
        // The same protection check must run if the exclude rule already exists.
        assert!(ensure_ignored(temp.path(), Path::new(".env")).is_err());
        fs::write(temp.path().join(".gitignore"), "").unwrap();
        ensure_ignored(temp.path(), Path::new(".env")).unwrap();
    }
    #[cfg(unix)]
    #[test]
    fn permissions_and_symlink_safety() {
        use std::os::unix::fs::{symlink, PermissionsExt};
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(".env");
        let values = BTreeMap::from([("URL".into(), "sqlite://secret".into())]);
        update(&path, &values).unwrap();
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();
        update(&path, &values).unwrap();
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o640
        );
        let link = temp.path().join("link");
        symlink(&path, &link).unwrap();
        assert!(update(&link, &values).is_err());
        assert!(read(&link).is_err());
    }
}
