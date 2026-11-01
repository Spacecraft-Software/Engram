// SPDX-FileCopyrightText: 2026 Mohamed Hammad & Spacecraft Software
// SPDX-License-Identifier: GPL-3.0-or-later
//! Bearer-token authentication for `engram serve`.
//!
//! Binding to `127.0.0.1` keeps other machines out, not other programs: any
//! local process could read every memory and, through `POST /v1/rules/sync`,
//! rewrite a project's `AGENTS.md`. So the server requires a bearer token by
//! default. The token lives in a file only its owner can read, and a client
//! proves it can read that file. `--no-auth` restores the old open server and
//! is refused on anything but a loopback address.
//!
//! `/v1/health` stays open so a supervisor can probe liveness without the
//! secret; it reveals nothing but "ok" and the time.

use axum::{
    extract::{Request, State},
    http::{header, HeaderValue, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
    Json,
};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Routes reachable without a token.
const OPEN_PATHS: &[&str] = &["/v1/health"];

/// Shortest token engram accepts from a token file. A generated token is 64
/// hex characters; this floor only stops a hand-written file holding
/// something guessable like `secret`.
const MIN_TOKEN_LEN: usize = 32;

/// How the HTTP surface authenticates callers.
#[derive(Clone)]
pub enum Auth {
    /// Every route outside [`OPEN_PATHS`] needs `Authorization: Bearer <token>`.
    Bearer(Arc<str>),
    /// No check at all (`serve --no-auth`).
    Disabled,
}

impl std::fmt::Debug for Auth {
    // Never print the secret, even in a debug dump.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Bearer(_) => f.write_str("Auth::Bearer(<redacted>)"),
            Self::Disabled => f.write_str("Auth::Disabled"),
        }
    }
}

/// Where the token lives when `--token-file` is not given:
/// `$XDG_STATE_HOME/engram/http-token`, else `~/.local/state/engram/http-token`.
/// A relative `XDG_STATE_HOME` is ignored, as the XDG spec requires.
pub fn default_token_path() -> Option<PathBuf> {
    let state = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| crate::harness::home_dir().map(|h| h.join(".local").join("state")))?;
    Some(state.join("engram").join("http-token"))
}

/// What [`load_or_create`] did, so `serve` can say so.
#[derive(Debug, PartialEq, Eq)]
pub enum TokenSource {
    Created,
    Existing,
}

/// Reads the token at `path`, creating it first when it does not exist.
///
/// A new token is two random v4 UUIDs in hex, 244 bits from the operating
/// system's CSPRNG through `uuid`'s `getrandom`; that needs no new dependency.
/// The directory is created `0700` and the file `0600`, with `create_new` so
/// a file another process creates at the same moment is never truncated.
///
/// # Errors
///
/// A message ready for the user when the file cannot be created or read,
/// when group or others may read it (on Unix), or when it holds fewer than
/// [`MIN_TOKEN_LEN`] characters.
pub fn load_or_create(path: &Path) -> Result<(Arc<str>, TokenSource), String> {
    let source = if path.exists() {
        TokenSource::Existing
    } else {
        create(path)?;
        TokenSource::Created
    };
    check_permissions(path)?;
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("cannot read token file {}: {e}", path.display()))?;
    let token = text.trim();
    if token.chars().count() < MIN_TOKEN_LEN {
        return Err(format!(
            "token file {} holds {} characters; at least {MIN_TOKEN_LEN} are required",
            path.display(),
            token.chars().count()
        ));
    }
    Ok((Arc::from(token), source))
}

fn create(path: &Path) -> Result<(), String> {
    use std::io::Write;

    if let Some(dir) = path.parent() {
        let mut builder = std::fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
        builder
            .create(dir)
            .map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    }
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut file = options
        .open(path)
        .map_err(|e| format!("cannot create token file {}: {e}", path.display()))?;
    let token = format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    );
    writeln!(file, "{token}")
        .map_err(|e| format!("cannot write token file {}: {e}", path.display()))
}

#[cfg(unix)]
fn check_permissions(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;

    let mode = std::fs::metadata(path)
        .map_err(|e| format!("cannot read token file {}: {e}", path.display()))?
        .permissions()
        .mode();
    if mode & 0o077 != 0 {
        return Err(format!(
            "token file {} is readable by other users (mode {:o}); run `chmod 600 {}`",
            path.display(),
            mode & 0o777,
            path.display()
        ));
    }
    Ok(())
}

/// Fails closed where engram cannot inspect permissions. On Windows a new
/// file inherits its directory's ACL, which may grant other users read
/// access, and verifying that needs the Win32 security API. Engram is built
/// and tested on Unix only, so rather than ship an untested ACL check it
/// refuses to trust a token file it cannot verify. `--no-auth` remains,
/// loopback only.
#[cfg(not(unix))]
fn check_permissions(path: &Path) -> Result<(), String> {
    Err(format!(
        "cannot verify that token file {} is private on this platform; \
         token authentication is supported on Unix only",
        path.display()
    ))
}

/// Compares two byte strings in time that depends only on their lengths, so
/// response timing does not reveal how much of a guessed token was right.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Extracts the credential from an `Authorization: Bearer <token>` header.
/// The scheme name is case-insensitive (RFC 9110 §11.1).
fn bearer(value: &HeaderValue) -> Option<&str> {
    let value = value.to_str().ok()?;
    let (scheme, token) = value.split_once(' ')?;
    scheme
        .eq_ignore_ascii_case("bearer")
        .then(|| token.trim())
        .filter(|t| !t.is_empty())
}

/// Axum middleware enforcing [`Auth`]. Wired with
/// `middleware::from_fn_with_state` in [`crate::http::router`].
pub async fn require_token(State(auth): State<Auth>, request: Request, next: Next) -> Response {
    let Auth::Bearer(expected) = &auth else {
        return next.run(request).await;
    };
    if OPEN_PATHS.contains(&request.uri().path()) {
        return next.run(request).await;
    }
    let presented = request.headers().get(header::AUTHORIZATION).map(bearer);
    match presented {
        Some(Some(token)) if constant_time_eq(token.as_bytes(), expected.as_bytes()) => {
            next.run(request).await
        }
        Some(Some(_)) => unauthorized("invalid bearer token"),
        Some(None) => unauthorized("the Authorization header is not a bearer token"),
        None => unauthorized("missing Authorization header"),
    }
}

fn unauthorized(message: &str) -> Response {
    let body = Json(serde_json::json!({
        "error": {
            "message": message,
            "hint": "send `Authorization: Bearer <token>`; the token is in the file `engram serve` names at startup (default ~/.local/state/engram/http-token)",
            "timestamp": crate::time::now_iso8601(),
        }
    }));
    let mut response = (StatusCode::UNAUTHORIZED, body).into_response();
    response.headers_mut().insert(
        header::WWW_AUTHENTICATE,
        HeaderValue::from_static("Bearer realm=\"engram\""),
    );
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_new_token_file_is_private_and_reused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("state").join("engram").join("http-token");
        let (first, source) = load_or_create(&path).expect("create");
        assert_eq!(source, TokenSource::Created);
        assert_eq!(first.len(), 64);
        assert!(first.bytes().all(|b| b.is_ascii_hexdigit()));

        let (second, source) = load_or_create(&path).expect("reuse");
        assert_eq!(source, TokenSource::Existing);
        assert_eq!(first, second, "a restart keeps the token clients hold");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode =
                |p: &Path| std::fs::metadata(p).expect("metadata").permissions().mode() & 0o777;
            assert_eq!(mode(&path), 0o600);
            assert_eq!(mode(path.parent().expect("dir")), 0o700);
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_token_file_others_can_read_is_refused() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("http-token");
        std::fs::write(&path, "a".repeat(64)).expect("write");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).expect("chmod");
        let err = load_or_create(&path).expect_err("0644 must be refused");
        assert!(err.contains("chmod 600"), "{err}");
    }

    #[test]
    fn a_short_token_is_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("http-token");
        std::fs::write(&path, "secret\n").expect("write");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).expect("chmod");
        }
        let err = load_or_create(&path).expect_err("too short");
        assert!(err.contains("at least 32"), "{err}");
    }

    #[test]
    fn bearer_parsing_accepts_any_scheme_case_and_rejects_others() {
        let parse = |v: &'static str| bearer(&HeaderValue::from_static(v)).map(str::to_owned);
        assert_eq!(parse("Bearer abc").as_deref(), Some("abc"));
        assert_eq!(parse("bearer abc").as_deref(), Some("abc"));
        assert_eq!(parse("Basic abc"), None);
        assert_eq!(parse("Bearer "), None);
        assert_eq!(parse("Bearer"), None);
    }

    #[test]
    fn constant_time_eq_matches_only_identical_input() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"abcd"));
    }
}
