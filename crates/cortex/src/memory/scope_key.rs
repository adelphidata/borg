//! Canonical [`Scope`] / [`ScopeKey`] types for the memory subsystem.
//!
//! Memory-entry rows are identified by a `(scope, name)` pair throughout
//! the DB layer. Scope strings are not free-form: they're one of `global`,
//! `project:<name>`, or `session:<id>`. Historically each call site did its
//! own string-shaped check; this module is the single source of truth for
//! parsing, formatting, and validation.
//!
//! See `docs/memory.md` (memory model) for the broader scope rules.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::session_types::{IdError, SessionId};

/// Canonical `global` scope literal — used as both the on-disk directory
/// name and the SQLite `scope` column value.
pub const GLOBAL_SCOPE: &str = "global";
/// Prefix for project-scoped entries: `project:<name>`.
pub const PROJECT_SCOPE_PREFIX: &str = "project:";
/// Prefix for session-scoped entries: `session:<id>`.
pub const SESSION_SCOPE_PREFIX: &str = "session:";

/// Validation failures for [`Scope::from_str`] / [`ScopeKey::from_str`].
#[derive(Debug, Error, PartialEq, Eq)]
pub enum ScopeError {
    /// Empty input.
    #[error("scope must not be empty")]
    Empty,
    /// Unknown prefix (not `global`, `project:`, or `session:`).
    #[error("unknown scope prefix: '{0}'")]
    UnknownPrefix(String),
    /// Project name was empty after the `project:` prefix.
    #[error("project scope must include a name after 'project:'")]
    EmptyProject,
    /// Project name contained characters that could escape the project
    /// directory (e.g. `..`, `/`, `\`, control chars).
    #[error("project name '{0}' contains forbidden characters (path traversal)")]
    InvalidProject(String),
    /// Session ID failed [`SessionId`] validation.
    #[error("invalid session scope: {0}")]
    InvalidSession(#[from] IdError),
    /// `ScopeKey` string did not contain `/`.
    #[error("scope key must be of the form '<scope>/<name>'")]
    MissingSeparator,
    /// Entry name was empty after the `/`.
    #[error("scope key entry name must not be empty")]
    EmptyName,
}

/// Canonical scope discriminant. Renders to the same string that the DB
/// `scope` column stores.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub enum Scope {
    /// Shared cross-session memory.
    Global,
    /// Project-scoped memory keyed by an opaque project name.
    Project(String),
    /// Per-session memory keyed by a validated [`SessionId`].
    Session(SessionId),
}

impl Scope {
    /// Project scope constructor. Validates the inner name is non-empty and
    /// free of path-traversal sequences — consumers may map it to a
    /// filesystem subdir (per-project memory dir, per-project skill dir).
    pub fn project(name: impl Into<String>) -> Result<Self, ScopeError> {
        let name = name.into();
        if name.is_empty() {
            return Err(ScopeError::EmptyProject);
        }
        if name.contains("..")
            || name.contains('/')
            || name.contains('\\')
            || name.chars().any(|c| c.is_control())
        {
            return Err(ScopeError::InvalidProject(name));
        }
        Ok(Self::Project(name))
    }

    /// Session scope constructor.
    pub fn session(id: SessionId) -> Self {
        Self::Session(id)
    }

    /// Render as the canonical wire/DB string.
    pub fn as_db_string(&self) -> String {
        match self {
            Self::Global => GLOBAL_SCOPE.to_string(),
            Self::Project(name) => format!("{PROJECT_SCOPE_PREFIX}{name}"),
            Self::Session(id) => format!("{SESSION_SCOPE_PREFIX}{}", id.as_str()),
        }
    }
}

impl fmt::Display for Scope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.as_db_string())
    }
}

impl FromStr for Scope {
    type Err = ScopeError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.is_empty() {
            return Err(ScopeError::Empty);
        }
        if s == GLOBAL_SCOPE {
            return Ok(Self::Global);
        }
        if let Some(rest) = s.strip_prefix(PROJECT_SCOPE_PREFIX) {
            return Self::project(rest);
        }
        if let Some(rest) = s.strip_prefix(SESSION_SCOPE_PREFIX) {
            let id = SessionId::from_str(rest)?;
            return Ok(Self::Session(id));
        }
        Err(ScopeError::UnknownPrefix(s.to_string()))
    }
}

impl TryFrom<String> for Scope {
    type Error = ScopeError;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        value.parse()
    }
}

impl From<Scope> for String {
    fn from(value: Scope) -> Self {
        value.as_db_string()
    }
}

/// A canonical `(scope, name)` pair. Renders as `scope/name`, e.g.
/// `global/INDEX`, `project:abc/notes`, `session:550e.../summary`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ScopeKey {
    /// Scope discriminant.
    pub scope: Scope,
    /// Entry name within the scope. Empty names are rejected at parse time.
    pub name: String,
}

impl ScopeKey {
    /// Construct a [`ScopeKey`], rejecting empty names.
    pub fn new(scope: Scope, name: impl Into<String>) -> Result<Self, ScopeError> {
        let name = name.into();
        if name.is_empty() {
            return Err(ScopeError::EmptyName);
        }
        Ok(Self { scope, name })
    }

    /// Convenience: build a `global/<name>` key.
    pub fn global(name: impl Into<String>) -> Result<Self, ScopeError> {
        Self::new(Scope::Global, name)
    }
}

impl fmt::Display for ScopeKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.scope, self.name)
    }
}

impl FromStr for ScopeKey {
    type Err = ScopeError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        // Split at the LAST '/' so `session:<id>/name` parses correctly even
        // if the session-id alphabet ever expands to include other
        // separators. Today `SessionId` rejects '/' so a plain split_once
        // would also work — but anchoring on the trailing '/' is more
        // forgiving and the canonical render uses exactly one '/'.
        let (scope_part, name) = s.rsplit_once('/').ok_or(ScopeError::MissingSeparator)?;
        if name.is_empty() {
            return Err(ScopeError::EmptyName);
        }
        let scope = Scope::from_str(scope_part)?;
        Ok(Self {
            scope,
            name: name.to_string(),
        })
    }
}

impl TryFrom<String> for ScopeKey {
    type Error = ScopeError;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        value.parse()
    }
}

impl From<ScopeKey> for String {
    fn from(value: ScopeKey) -> Self {
        value.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scope_round_trips_global() {
        let s: Scope = "global".parse().unwrap();
        assert_eq!(s, Scope::Global);
        assert_eq!(s.to_string(), "global");
    }

    #[test]
    fn scope_round_trips_project() {
        let s: Scope = "project:abc".parse().unwrap();
        assert_eq!(s, Scope::Project("abc".into()));
        assert_eq!(s.to_string(), "project:abc");
    }

    #[test]
    fn scope_rejects_project_path_traversal() {
        for bad in [
            "project:../etc",
            "project:..",
            "project:foo/bar",
            "project:foo\\bar",
            "project:foo\nbar",
        ] {
            assert!(
                matches!(bad.parse::<Scope>(), Err(ScopeError::InvalidProject(_))),
                "expected InvalidProject for {bad:?}"
            );
        }
    }

    #[test]
    fn scope_round_trips_session() {
        let s: Scope = "session:550e8400-e29b-41d4-a716-446655440000"
            .parse()
            .unwrap();
        match &s {
            Scope::Session(id) => {
                assert_eq!(id.as_str(), "550e8400-e29b-41d4-a716-446655440000");
            }
            _ => panic!("expected Session"),
        }
        assert_eq!(
            s.to_string(),
            "session:550e8400-e29b-41d4-a716-446655440000"
        );
    }

    #[test]
    fn scope_rejects_empty() {
        assert_eq!("".parse::<Scope>(), Err(ScopeError::Empty));
    }

    #[test]
    fn scope_rejects_unknown_prefix() {
        assert!(matches!(
            "team:abc".parse::<Scope>(),
            Err(ScopeError::UnknownPrefix(_))
        ));
    }

    #[test]
    fn scope_rejects_empty_project() {
        assert_eq!("project:".parse::<Scope>(), Err(ScopeError::EmptyProject));
    }

    #[test]
    fn scope_propagates_session_validation() {
        // path traversal in session id surfaces as InvalidSession
        assert!(matches!(
            "session:../../etc".parse::<Scope>(),
            Err(ScopeError::InvalidSession(_))
        ));
    }

    #[test]
    fn scope_key_round_trips_global_index() {
        let k: ScopeKey = "global/INDEX".parse().unwrap();
        assert_eq!(k.scope, Scope::Global);
        assert_eq!(k.name, "INDEX");
        assert_eq!(k.to_string(), "global/INDEX");
    }

    #[test]
    fn scope_key_round_trips_project_notes() {
        let k: ScopeKey = "project:abc/notes".parse().unwrap();
        assert_eq!(k.scope, Scope::Project("abc".into()));
        assert_eq!(k.name, "notes");
        assert_eq!(k.to_string(), "project:abc/notes");
    }

    #[test]
    fn scope_key_round_trips_session_foo() {
        let k: ScopeKey = "session:xyz/foo".parse().unwrap();
        match &k.scope {
            Scope::Session(id) => assert_eq!(id.as_str(), "xyz"),
            _ => panic!("expected Session"),
        }
        assert_eq!(k.name, "foo");
        assert_eq!(k.to_string(), "session:xyz/foo");
    }

    #[test]
    fn scope_key_rejects_missing_separator() {
        assert_eq!(
            "global".parse::<ScopeKey>(),
            Err(ScopeError::MissingSeparator)
        );
    }

    #[test]
    fn scope_key_rejects_empty_name() {
        assert_eq!("global/".parse::<ScopeKey>(), Err(ScopeError::EmptyName));
    }

    #[test]
    fn scope_key_rejects_unknown_scope() {
        assert!(matches!(
            "team:abc/notes".parse::<ScopeKey>(),
            Err(ScopeError::UnknownPrefix(_))
        ));
    }

    #[test]
    fn scope_key_global_helper() {
        let k = ScopeKey::global("INDEX").unwrap();
        assert_eq!(k.to_string(), "global/INDEX");
    }

    #[test]
    fn scope_key_global_helper_rejects_empty() {
        assert_eq!(ScopeKey::global(""), Err(ScopeError::EmptyName));
    }

    #[test]
    fn scope_key_serde_round_trip() {
        let k = ScopeKey::new(Scope::Project("abc".into()), "notes").unwrap();
        let json = serde_json::to_string(&k).unwrap();
        assert_eq!(json, "\"project:abc/notes\"");
        let back: ScopeKey = serde_json::from_str(&json).unwrap();
        assert_eq!(back, k);
    }

    #[test]
    fn scope_serde_round_trip() {
        let s = Scope::Project("abc".into());
        let json = serde_json::to_string(&s).unwrap();
        assert_eq!(json, "\"project:abc\"");
        let back: Scope = serde_json::from_str(&json).unwrap();
        assert_eq!(back, s);
    }

    #[test]
    fn scope_key_session_with_uuid_round_trips() {
        let raw = "session:550e8400-e29b-41d4-a716-446655440000/summary";
        let k: ScopeKey = raw.parse().unwrap();
        assert_eq!(k.to_string(), raw);
    }
}
