use omnisolo_builtin_agent_core::types::ToolError;
use std::path::{Component, Path, PathBuf};

fn path_error(action: &str, requested: &str, message: impl std::fmt::Display) -> ToolError {
    ToolError::LlmRecoverable(format!("{action}: path {requested:?}: {message}"))
}

fn relative_path(action: &str, requested: &str, allow_dot: bool) -> Result<PathBuf, ToolError> {
    if requested.contains('\0') {
        return Err(path_error(action, requested, "NUL byte is not allowed"));
    }
    if requested.starts_with('/') || requested.starts_with('\\') {
        return Err(path_error(
            action,
            requested,
            "absolute paths and parent traversal are not allowed",
        ));
    }
    let mut relative = PathBuf::new();
    let mut saw_cur_dir = false;
    for component in Path::new(requested).components() {
        match component {
            Component::Normal(part) => relative.push(part),
            Component::CurDir => {
                saw_cur_dir = true;
            }
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(path_error(
                    action,
                    requested,
                    "path traversal via '..' and absolute paths are not allowed",
                ));
            }
        }
    }
    if relative.as_os_str().is_empty() {
        if allow_dot && (saw_cur_dir || requested.trim().is_empty()) {
            return Ok(PathBuf::from("."));
        }
        return Err(path_error(action, requested, "path is empty"));
    }
    Ok(relative)
}

async fn canonical_root(action: &str, root: &Path) -> Result<PathBuf, ToolError> {
    tokio::fs::canonicalize(root)
        .await
        .map_err(|error| path_error(action, &root.display().to_string(), error))
}

fn require_within_root(
    action: &str,
    requested: &str,
    root: &Path,
    target: &Path,
) -> Result<(), ToolError> {
    if target.starts_with(root) {
        Ok(())
    } else {
        Err(path_error(action, requested, "path escapes workspace root"))
    }
}

/// Captures and canonicalizes the workspace root once at tool construction.
pub fn configured_root(working_dir: Option<PathBuf>) -> Result<PathBuf, ToolError> {
    let root = match working_dir {
        Some(root) => root,
        None => std::env::current_dir().map_err(|error| path_error("workspace", ".", error))?,
    };
    std::fs::canonicalize(&root)
        .map_err(|error| path_error("workspace", &root.display().to_string(), error))
}

/// Fails closed unless an explicit workspace root was configured, returning its canonical path.
pub fn required_root(working_dir: Option<PathBuf>) -> Result<PathBuf, ToolError> {
    let root = working_dir.ok_or_else(|| {
        ToolError::LlmRecoverable(
            "workspace: explicit working_dir is required; refusing unconfined filesystem access"
                .to_string(),
        )
    })?;
    std::fs::canonicalize(&root)
        .map_err(|error| path_error("workspace", &root.display().to_string(), error))
}

/// Resolves an existing path and rejects traversal or symlink escapes.
pub async fn existing(root: &Path, requested: &str) -> Result<PathBuf, ToolError> {
    let relative = relative_path("read", requested, false)?;
    let root = canonical_root("read", root).await?;
    let target = tokio::fs::canonicalize(root.join(relative))
        .await
        .map_err(|error| path_error("read", requested, error))?;
    require_within_root("read", requested, &root, &target)?;
    Ok(target)
}

/// Resolves a write target through its nearest existing parent without allowing
/// absolute paths, parent traversal, symlinked-parent escapes, or existing destination symlinks.
pub async fn for_write(root: &Path, requested: &str) -> Result<PathBuf, ToolError> {
    let relative = relative_path("write", requested, false)?;
    let file_name = relative
        .file_name()
        .ok_or_else(|| path_error("write", requested, "path has no file name"))?
        .to_owned();
    let root = canonical_root("write", root).await?;
    let requested_parent = relative.parent().unwrap_or_else(|| Path::new(""));
    let lexical_parent = root.join(requested_parent);
    let mut existing_parent = lexical_parent.as_path();

    loop {
        match tokio::fs::symlink_metadata(existing_parent).await {
            Ok(_) => break,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                existing_parent = existing_parent
                    .parent()
                    .ok_or_else(|| path_error("write", requested, "path has no existing parent"))?;
            }
            Err(error) => return Err(path_error("write", requested, error)),
        }
    }

    let canonical_parent = tokio::fs::canonicalize(existing_parent)
        .await
        .map_err(|error| path_error("write", requested, error))?;
    require_within_root("write", requested, &root, &canonical_parent)?;
    let remaining_parent = lexical_parent
        .strip_prefix(existing_parent)
        .map_err(|error| path_error("write", requested, error))?;

    let final_target = canonical_parent.join(remaining_parent).join(file_name);
    match tokio::fs::symlink_metadata(&final_target).await {
        Ok(meta) if meta.file_type().is_symlink() => {
            return Err(path_error(
                "write",
                requested,
                "refusing to follow or replace symlink",
            ));
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(path_error("write", requested, error)),
    }

    Ok(final_target)
}

/// Unified workspace path confinement helper for existing reads/dirs or non-existing write targets.
/// Accepts `"."` when `must_exist` is true (e.g., directory roots) and always enforces canonical
/// root containment and symlink checks.
pub async fn resolve_workspace_path(
    root: &Path,
    requested: &str,
    must_exist: bool,
) -> Result<PathBuf, ToolError> {
    if must_exist {
        let relative = relative_path("workspace", requested, true)?;
        let canonical_base = canonical_root("workspace", root).await?;
        let candidate = if relative == Path::new(".") {
            canonical_base.clone()
        } else {
            canonical_base.join(&relative)
        };
        if let Ok(meta) = tokio::fs::symlink_metadata(&candidate).await
            && meta.file_type().is_symlink()
        {
            let resolved = tokio::fs::canonicalize(&candidate)
                .await
                .map_err(|error| path_error("workspace", requested, error))?;
            require_within_root("workspace", requested, &canonical_base, &resolved)?;
            return Err(path_error(
                "workspace",
                requested,
                "symlink traversal is not allowed",
            ));
        }
        let target = tokio::fs::canonicalize(&candidate)
            .await
            .map_err(|error| path_error("workspace", requested, error))?;
        require_within_root("workspace", requested, &canonical_base, &target)?;
        Ok(target)
    } else {
        for_write(root, requested).await
    }
}

/// Synchronous workspace directory resolution for blocking directory walkers (`RepoMap`, etc.).
pub fn resolve_workspace_path_sync(
    root: &Path,
    requested: &str,
) -> Result<(PathBuf, PathBuf), ToolError> {
    let relative = relative_path("workspace", requested, true)?;
    let canonical_base = std::fs::canonicalize(root)
        .map_err(|error| path_error("workspace", &root.display().to_string(), error))?;
    let candidate = if relative == Path::new(".") {
        canonical_base.clone()
    } else {
        canonical_base.join(&relative)
    };
    if let Ok(meta) = std::fs::symlink_metadata(&candidate)
        && meta.file_type().is_symlink()
    {
        return Err(path_error(
            "workspace",
            requested,
            "symlink traversal is not allowed",
        ));
    }
    let target = std::fs::canonicalize(&candidate)
        .map_err(|error| path_error("workspace", requested, error))?;
    require_within_root("workspace", requested, &canonical_base, &target)?;
    Ok((canonical_base, target))
}

/// Returns true only if `path` is not a symlink and canonicalizes inside `canonical_root`.
pub fn is_contained_non_symlink_sync(canonical_root: &Path, path: &Path) -> bool {
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return false;
    };
    if meta.file_type().is_symlink() {
        return false;
    }
    std::fs::canonicalize(path)
        .map(|resolved| resolved.starts_with(canonical_root))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::{
        existing, for_write, is_contained_non_symlink_sync, required_root, resolve_workspace_path,
        resolve_workspace_path_sync,
    };
    use tempfile::tempdir;

    #[tokio::test]
    async fn workspace_path_rejects_parent_and_absolute_paths() {
        let root = tempdir().unwrap();

        assert!(required_root(None).is_err());
        assert!(existing(root.path(), "../outside").await.is_err());
        assert!(for_write(root.path(), "../outside").await.is_err());
        assert!(existing(root.path(), "/etc/passwd").await.is_err());
        assert!(for_write(root.path(), "/tmp/outside").await.is_err());
        assert!(
            resolve_workspace_path(root.path(), "//etc", true)
                .await
                .is_err()
        );
        assert!(
            resolve_workspace_path(root.path(), "a\0b", false)
                .await
                .is_err()
        );
        assert_eq!(
            resolve_workspace_path(root.path(), ".", true)
                .await
                .unwrap(),
            std::fs::canonicalize(root.path()).unwrap()
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn workspace_path_rejects_existing_symlink_to_outside() {
        let root = tempdir().unwrap();
        let outside = tempdir().unwrap();
        let outside_file = outside.path().join("secret.txt");
        tokio::fs::write(&outside_file, "secret").await.unwrap();
        std::os::unix::fs::symlink(&outside_file, root.path().join("escape.txt")).unwrap();

        assert!(existing(root.path(), "escape.txt").await.is_err());
        assert!(
            resolve_workspace_path(root.path(), "escape.txt", true)
                .await
                .is_err()
        );
        assert!(
            resolve_workspace_path(root.path(), "escape.txt", false)
                .await
                .is_err()
        );
        assert!(resolve_workspace_path_sync(root.path(), "escape.txt").is_err());
        let canon = std::fs::canonicalize(root.path()).unwrap();
        assert!(!is_contained_non_symlink_sync(
            &canon,
            &root.path().join("escape.txt")
        ));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn workspace_path_rejects_write_through_symlinked_parent() {
        let root = tempdir().unwrap();
        let outside = tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), root.path().join("escape")).unwrap();

        assert!(for_write(root.path(), "escape/new.txt").await.is_err());
    }

    #[tokio::test]
    async fn workspace_path_accepts_normal_nested_paths() {
        let root = tempdir().unwrap();
        let nested = root.path().join("nested");
        tokio::fs::create_dir(&nested).await.unwrap();
        let file = nested.join("file.txt");
        tokio::fs::write(&file, "ok").await.unwrap();

        assert_eq!(
            existing(root.path(), "nested/file.txt").await.unwrap(),
            file
        );
        assert_eq!(
            for_write(root.path(), "nested/new.txt").await.unwrap(),
            nested.join("new.txt")
        );
    }
}
