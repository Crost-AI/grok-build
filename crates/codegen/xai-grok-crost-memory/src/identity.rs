//! Project identity resolution.
//!
//! Identity comes from **committed repo content** and nothing else. The
//! resolver walks ancestor directories from the session cwd looking for
//! `.crost/project.yaml`; the first hit wins. It never consults the checkout
//! path, the worktree name, `$USER`, or the current branch — which is exactly
//! why every clone and every `git worktree` of the same repo resolves to the
//! same `projectId`: the yaml travels with the commit, so every checkout has a
//! byte-identical copy of it.
//!
//! The parser is a deliberately tiny `key: value` subset of YAML. Adding a
//! real YAML dependency for four scalar fields would be a poor trade.

use std::fmt;
use std::path::Path;
use std::path::PathBuf;

/// Default location of the identity file, relative to the repo root.
pub const DEFAULT_PROJECT_FILE: &str = ".crost/project.yaml";

/// The only `apiVersion` this client understands.
pub const SUPPORTED_API_VERSION: &str = "memory.crost/v1";

/// Identity of the project whose memory this session reads and writes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectIdentity {
    /// Immutable id (uuid/ulid). Stable across renames.
    pub project_id: String,
    /// Readable name, `[a-z0-9][a-z0-9._-]*`.
    pub slug: String,
    /// Optional bank-name prefix override. Defaults to `crost--{slug}`.
    pub bank_prefix: Option<String>,
}

/// Why identity resolution produced nothing. Surfaced by `doctor`; the normal
/// call path only cares that it is `Err` (memory silently disables).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IdentityError {
    /// No ancestor directory contained the project file.
    NotFound { start: PathBuf, file: String },
    /// The file exists but could not be read.
    Unreadable { path: PathBuf, detail: String },
    /// A required key was missing or blank.
    MissingField { path: PathBuf, field: &'static str },
    /// `slug` did not match `[a-z0-9][a-z0-9._-]*`.
    InvalidSlug { path: PathBuf, slug: String },
    /// `apiVersion` was present but not `memory.crost/v1`.
    UnsupportedApiVersion { path: PathBuf, value: String },
}

impl fmt::Display for IdentityError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound { start, file } => write!(
                f,
                "no `{}` found in `{}` or any ancestor directory",
                file,
                start.display()
            ),
            Self::Unreadable { path, detail } => {
                write!(f, "could not read `{}`: {detail}", path.display())
            }
            Self::MissingField { path, field } => {
                write!(f, "`{}` is missing required key `{field}`", path.display())
            }
            Self::InvalidSlug { path, slug } => write!(
                f,
                "`{}` has invalid slug `{slug}` (expected [a-z0-9][a-z0-9._-]*)",
                path.display()
            ),
            Self::UnsupportedApiVersion { path, value } => write!(
                f,
                "`{}` has unsupported apiVersion `{value}` (expected {SUPPORTED_API_VERSION})",
                path.display()
            ),
        }
    }
}

impl std::error::Error for IdentityError {}

/// A successful resolution: the identity plus the file it came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedIdentity {
    pub identity: ProjectIdentity,
    pub source: PathBuf,
}

/// Resolve project identity from `start_dir`, using the default project file
/// path. `None` means memory is disabled for the session.
pub fn resolve_project_identity(start_dir: &Path) -> Option<ProjectIdentity> {
    resolve_project_identity_at(start_dir, DEFAULT_PROJECT_FILE)
}

/// As [`resolve_project_identity`], with a caller-chosen relative project-file
/// path (config key `project_file`).
pub fn resolve_project_identity_at(
    start_dir: &Path,
    project_file: &str,
) -> Option<ProjectIdentity> {
    resolve_project_identity_detailed(start_dir, project_file)
        .ok()
        .map(|r| r.identity)
}

/// Resolution with the failure reason preserved, for `doctor`.
pub fn resolve_project_identity_detailed(
    start_dir: &Path,
    project_file: &str,
) -> Result<ResolvedIdentity, IdentityError> {
    let path =
        find_project_file(start_dir, project_file).ok_or_else(|| IdentityError::NotFound {
            start: start_dir.to_path_buf(),
            file: project_file.to_string(),
        })?;
    let text = std::fs::read_to_string(&path).map_err(|e| IdentityError::Unreadable {
        path: path.clone(),
        detail: e.to_string(),
    })?;
    let identity = parse_project_file(&path, &text)?;
    Ok(ResolvedIdentity {
        identity,
        source: path,
    })
}

/// Walk `start_dir` and its ancestors, stopping at the filesystem root.
fn find_project_file(start_dir: &Path, project_file: &str) -> Option<PathBuf> {
    let mut cur = Some(start_dir);
    while let Some(dir) = cur {
        let candidate = dir.join(project_file);
        if candidate.is_file() {
            return Some(candidate);
        }
        cur = dir.parent();
    }
    None
}

/// Validate the parsed key/value pairs into a [`ProjectIdentity`].
fn parse_project_file(path: &Path, text: &str) -> Result<ProjectIdentity, IdentityError> {
    let pairs = parse_minimal_yaml(text);
    let get = |key: &str| -> Option<&str> {
        pairs
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
            .filter(|v| !v.trim().is_empty())
    };

    if let Some(api) = get("apiVersion")
        && api.trim() != SUPPORTED_API_VERSION
    {
        return Err(IdentityError::UnsupportedApiVersion {
            path: path.to_path_buf(),
            value: api.trim().to_string(),
        });
    }

    let project_id = get("projectId").ok_or_else(|| IdentityError::MissingField {
        path: path.to_path_buf(),
        field: "projectId",
    })?;
    let slug = get("slug").ok_or_else(|| IdentityError::MissingField {
        path: path.to_path_buf(),
        field: "slug",
    })?;
    if !is_valid_slug(slug) {
        return Err(IdentityError::InvalidSlug {
            path: path.to_path_buf(),
            slug: slug.to_string(),
        });
    }

    Ok(ProjectIdentity {
        project_id: project_id.trim().to_string(),
        slug: slug.to_string(),
        bank_prefix: get("bankPrefix").map(|v| v.trim().to_string()),
    })
}

/// `[a-z0-9][a-z0-9._-]*`, matched in full.
pub fn is_valid_slug(slug: &str) -> bool {
    let mut chars = slug.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !(first.is_ascii_lowercase() || first.is_ascii_digit()) {
        return false;
    }
    chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-'))
}

/// Minimal YAML subset: top-level `key: value` scalars, `#` comments, optional
/// single/double quotes. Anything else (nesting, lists, anchors) is ignored
/// rather than rejected — the four keys we care about are all scalars.
fn parse_minimal_yaml(text: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with('-') {
            continue;
        }
        let Some((key, rest)) = line.split_once(':') else {
            continue;
        };
        let key = key.trim();
        if key.is_empty() || key.contains(char::is_whitespace) {
            continue;
        }
        out.push((key.to_string(), parse_scalar(rest)));
    }
    out
}

/// Strip surrounding quotes and trailing `#` comments from a scalar value.
fn parse_scalar(rest: &str) -> String {
    let v = rest.trim_start();
    let mut chars = v.chars();
    match chars.next() {
        Some(q @ ('"' | '\'')) => {
            // Quoted: take everything up to the next matching quote.
            let body: String = chars.by_ref().take_while(|c| *c != q).collect();
            body
        }
        _ => {
            // Unquoted: a comment starts at a `#` preceded by whitespace, or at
            // a leading `#` (already filtered above).
            let mut cut = v.len();
            let bytes = v.as_bytes();
            for (i, b) in bytes.iter().enumerate() {
                if *b == b'#' && i > 0 && bytes[i - 1].is_ascii_whitespace() {
                    cut = i;
                    break;
                }
            }
            v[..cut].trim().to_string()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOOD: &str = "\
# Crost project identity (committed)
apiVersion: memory.crost/v1
projectId: 01J8QF6X0000000000000000
slug: ohm-storefront   # readable name
";

    fn write_project(dir: &Path, body: &str) -> PathBuf {
        let crost = dir.join(".crost");
        std::fs::create_dir_all(&crost).unwrap();
        let path = crost.join("project.yaml");
        std::fs::write(&path, body).unwrap();
        path
    }

    #[test]
    fn resolves_from_the_directory_itself() {
        let tmp = tempfile::tempdir().unwrap();
        write_project(tmp.path(), GOOD);
        let id = resolve_project_identity(tmp.path()).unwrap();
        assert_eq!(id.project_id, "01J8QF6X0000000000000000");
        assert_eq!(id.slug, "ohm-storefront");
        assert_eq!(id.bank_prefix, None);
    }

    #[test]
    fn walks_ancestors_to_the_repo_root() {
        let tmp = tempfile::tempdir().unwrap();
        write_project(tmp.path(), GOOD);
        let deep = tmp.path().join("crates").join("codegen").join("thing");
        std::fs::create_dir_all(&deep).unwrap();
        let id = resolve_project_identity(&deep).unwrap();
        assert_eq!(id.slug, "ohm-storefront");
    }

    #[test]
    fn worktrees_resolve_identically() {
        // Two unrelated directory trees with different names, different depths
        // and different parents. Because the yaml is committed content, both
        // checkouts carry the same bytes — and therefore the same identity.
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        let main = a.path().join("ohm-storefront");
        let worktree = b.path().join("wt").join("feature-xyz");
        std::fs::create_dir_all(&main).unwrap();
        std::fs::create_dir_all(&worktree).unwrap();
        write_project(&main, GOOD);
        write_project(&worktree, GOOD);

        let from_main = resolve_project_identity(&main).unwrap();
        let from_worktree = resolve_project_identity(&worktree).unwrap();
        assert_eq!(from_main, from_worktree);
    }

    #[test]
    fn missing_file_disables_memory() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(resolve_project_identity(tmp.path()), None);
        let err = resolve_project_identity_detailed(tmp.path(), DEFAULT_PROJECT_FILE).unwrap_err();
        assert!(matches!(err, IdentityError::NotFound { .. }));
        assert!(err.to_string().contains(".crost/project.yaml"));
    }

    #[test]
    fn missing_project_id_is_invalid() {
        let tmp = tempfile::tempdir().unwrap();
        write_project(tmp.path(), "apiVersion: memory.crost/v1\nslug: thing\n");
        let err = resolve_project_identity_detailed(tmp.path(), DEFAULT_PROJECT_FILE).unwrap_err();
        assert!(matches!(
            err,
            IdentityError::MissingField {
                field: "projectId",
                ..
            }
        ));
        assert_eq!(resolve_project_identity(tmp.path()), None);
    }

    #[test]
    fn missing_slug_is_invalid() {
        let tmp = tempfile::tempdir().unwrap();
        write_project(tmp.path(), "projectId: abc\n");
        let err = resolve_project_identity_detailed(tmp.path(), DEFAULT_PROJECT_FILE).unwrap_err();
        assert!(matches!(
            err,
            IdentityError::MissingField { field: "slug", .. }
        ));
    }

    #[test]
    fn bad_slug_is_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        write_project(tmp.path(), "projectId: abc\nslug: Ohm Storefront\n");
        let err = resolve_project_identity_detailed(tmp.path(), DEFAULT_PROJECT_FILE).unwrap_err();
        assert!(matches!(err, IdentityError::InvalidSlug { .. }));
    }

    #[test]
    fn slug_charset() {
        assert!(is_valid_slug("a"));
        assert!(is_valid_slug("0hm-store.front_2"));
        assert!(!is_valid_slug(""));
        assert!(!is_valid_slug("-leading"));
        assert!(!is_valid_slug(".leading"));
        assert!(!is_valid_slug("Upper"));
        assert!(!is_valid_slug("has space"));
        assert!(!is_valid_slug("slash/es"));
    }

    #[test]
    fn unsupported_api_version_is_invalid() {
        let tmp = tempfile::tempdir().unwrap();
        write_project(
            tmp.path(),
            "apiVersion: memory.crost/v2\nprojectId: abc\nslug: thing\n",
        );
        let err = resolve_project_identity_detailed(tmp.path(), DEFAULT_PROJECT_FILE).unwrap_err();
        assert!(matches!(err, IdentityError::UnsupportedApiVersion { .. }));
    }

    #[test]
    fn api_version_is_optional() {
        let tmp = tempfile::tempdir().unwrap();
        write_project(tmp.path(), "projectId: abc\nslug: thing\n");
        assert!(resolve_project_identity(tmp.path()).is_some());
    }

    #[test]
    fn quotes_and_comments_are_handled() {
        let tmp = tempfile::tempdir().unwrap();
        write_project(
            tmp.path(),
            "# leading comment\napiVersion: \"memory.crost/v1\"\nprojectId: 'id-with #hash'\nslug: thing   # trailing\nbankPrefix: crost--custom # override\n",
        );
        let id = resolve_project_identity(tmp.path()).unwrap();
        assert_eq!(id.project_id, "id-with #hash");
        assert_eq!(id.slug, "thing");
        assert_eq!(id.bank_prefix.as_deref(), Some("crost--custom"));
    }

    #[test]
    fn custom_project_file_path_is_honored() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("cfg")).unwrap();
        std::fs::write(
            tmp.path().join("cfg").join("id.yaml"),
            "projectId: abc\nslug: thing\n",
        )
        .unwrap();
        assert!(resolve_project_identity(tmp.path()).is_none());
        let id = resolve_project_identity_at(tmp.path(), "cfg/id.yaml").unwrap();
        assert_eq!(id.slug, "thing");
    }

    #[test]
    fn unrelated_lines_are_ignored() {
        let pairs = parse_minimal_yaml("list:\n  - a\n  - b\nslug: ok\n\n#c\nnot a pair\n");
        assert!(pairs.iter().any(|(k, v)| k == "slug" && v == "ok"));
        assert!(!pairs.iter().any(|(k, _)| k == "not a pair"));
    }
}
