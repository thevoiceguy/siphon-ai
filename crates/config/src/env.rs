//! Config-load-time secret/variable expansion.
//!
//! Per `docs/DEV_PLAN.md` §6.6 every string value in the TOML can
//! reference an environment variable via `${VAR}` / `${VAR:-default}`.
//! As of v0.18.0 a reference can also pull a secret from outside the
//! process environment, so operators don't have to put plaintext
//! secrets in env vars (visible in `/proc/<pid>/environ`, dumps, unit
//! files). Two source prefixes are recognised inside `${...}`:
//!
//! | Form | Resolves to |
//! |---|---|
//! | `${VAR}` / `${VAR:-default}` | process env (default; unchanged) |
//! | `${file:/path/to/secret}`    | trimmed contents of that file |
//! | `${cred:NAME}`               | `$CREDENTIALS_DIRECTORY/NAME` contents |
//!
//! `file:` covers Docker/Kubernetes secrets and Vault-Agent templated
//! files; `cred:` covers systemd `LoadCredential=` / `ImportCredential=`.
//! All three share one fail-loud pass: a missing env var, an unreadable
//! file, or an unset `$CREDENTIALS_DIRECTORY` surfaces as a single
//! [`EnvError`] *before* the daemon starts, never as a downstream parse
//! failure on a half-substituted string.
//!
//! Disambiguation: the `:-` default operator is always an env reference
//! (checked first), so a literal env var named `file` with a default
//! (`${file:-x}`) is never mistaken for the `file:` prefix. A path that
//! itself contains `:-` is the one ambiguous case and fails loudly as a
//! malformed reference rather than resolving silently.
//!
//! Resolved values are never logged in expanded form.

use std::borrow::Cow;

use thiserror::Error;

/// Resolves config references to their values. Pluggable so tests can
/// drive expansion without touching the real environment or filesystem.
///
/// Only [`lookup`](EnvSource::lookup) is required; the file and
/// credential resolvers default to the real OS and are overridden in
/// tests for hermeticity.
pub trait EnvSource {
    /// Look up an environment variable (`${VAR}`).
    fn lookup(&self, name: &str) -> Option<String>;

    /// Read the full contents of a secret file (`${file:PATH}`). The
    /// caller trims trailing newlines. Default reads the real filesystem.
    fn read_file(&self, path: &str) -> std::io::Result<String> {
        std::fs::read_to_string(path)
    }

    /// The systemd credentials directory (`$CREDENTIALS_DIRECTORY`), the
    /// base path for `${cred:NAME}`. Default reads the process env.
    fn credentials_dir(&self) -> Option<String> {
        std::env::var("CREDENTIALS_DIRECTORY").ok()
    }
}

/// Default impl: reads from the process environment and filesystem.
#[derive(Debug, Default, Clone, Copy)]
pub struct ProcessEnv;

impl EnvSource for ProcessEnv {
    fn lookup(&self, name: &str) -> Option<String> {
        std::env::var(name).ok()
    }
}

#[derive(Debug, Error)]
pub enum EnvError {
    /// `${VAR}` referenced an env var that isn't set and has no
    /// default (`${VAR:-default}` form).
    #[error("environment variable {name:?} is referenced by config but not set")]
    Missing { name: String },

    /// `${file:PATH}` referenced a file that couldn't be read.
    #[error("secret file {path:?} referenced by config could not be read: {message}")]
    FileUnreadable { path: String, message: String },

    /// `${cred:NAME}` referenced a systemd credential that couldn't be read.
    #[error("systemd credential {name:?} referenced by config could not be read: {message}")]
    CredentialUnreadable { name: String, message: String },

    /// `${cred:NAME}` was used but `$CREDENTIALS_DIRECTORY` isn't set
    /// (the daemon wasn't started with systemd `LoadCredential=`).
    #[error(
        "config references credential {name:?} via `${{cred:...}}` but \
         $CREDENTIALS_DIRECTORY is not set"
    )]
    CredentialsDirUnset { name: String },

    /// Malformed `${...}` — unterminated, empty name/path, etc.
    #[error("malformed env reference at byte {at}: {message}")]
    Malformed { at: usize, message: String },
}

/// Expand every `${VAR}` and `${VAR:-default}` in `input`. Returns
/// the input unchanged if no references appear.
///
/// Substring scanning is byte-oriented so multibyte chars in literal
/// values stay intact. Variable names match `[A-Za-z_][A-Za-z0-9_]*`
/// per POSIX shell convention; anything else inside `${...}` is
/// reported as malformed.
pub fn expand<E: EnvSource>(input: &str, env: &E) -> Result<String, EnvError> {
    if !input.contains("${") {
        return Ok(input.to_string());
    }
    let bytes = input.as_bytes();
    let mut out = String::with_capacity(input.len());
    let mut i = 0;
    while i < bytes.len() {
        if i + 1 < bytes.len() && bytes[i] == b'$' && bytes[i + 1] == b'{' {
            let start = i;
            let close = find_close_brace(bytes, i + 2).ok_or_else(|| EnvError::Malformed {
                at: start,
                message: "unterminated `${`".into(),
            })?;
            let inner = &input[i + 2..close];
            out.push_str(&resolve_ref(inner, env, start)?);
            i = close + 1;
        } else {
            out.push(input[i..].chars().next().expect("non-empty slice"));
            i += input[i..]
                .chars()
                .next()
                .expect("non-empty slice")
                .len_utf8();
        }
    }
    Ok(out)
}

/// Resolve a single `${...}` reference (the text between the braces).
/// Dispatches on the source prefix; `at` is the byte offset of the
/// opening `${` for error context.
fn resolve_ref<E: EnvSource>(inner: &str, env: &E, at: usize) -> Result<String, EnvError> {
    // The `:-` default operator is always an env reference, checked
    // first so `${file:-x}` (env var `file`, default `x`) is never read
    // as the `file:` prefix.
    if let Some((name, default)) = inner.split_once(":-") {
        return resolve_env(name, Some(default), at, env);
    }
    if let Some(path) = inner.strip_prefix("file:") {
        return resolve_file(path, env, at);
    }
    if let Some(name) = inner.strip_prefix("cred:") {
        return resolve_cred(name, env, at);
    }
    resolve_env(inner, None, at, env)
}

fn resolve_env<E: EnvSource>(
    name: &str,
    default: Option<&str>,
    at: usize,
    env: &E,
) -> Result<String, EnvError> {
    if !is_valid_env_name(name) {
        return Err(EnvError::Malformed {
            at,
            message: format!("invalid env var name {name:?}"),
        });
    }
    match env.lookup(name) {
        Some(value) => Ok(value),
        None => match default {
            Some(d) => Ok(d.to_string()),
            None => Err(EnvError::Missing { name: name.into() }),
        },
    }
}

fn resolve_file<E: EnvSource>(path: &str, env: &E, at: usize) -> Result<String, EnvError> {
    if path.is_empty() {
        return Err(EnvError::Malformed {
            at,
            message: "empty file path in `${file:...}`".into(),
        });
    }
    match env.read_file(path) {
        Ok(contents) => Ok(trim_secret(&contents)),
        Err(e) => Err(EnvError::FileUnreadable {
            path: path.into(),
            message: e.to_string(),
        }),
    }
}

fn resolve_cred<E: EnvSource>(name: &str, env: &E, at: usize) -> Result<String, EnvError> {
    if name.is_empty() {
        return Err(EnvError::Malformed {
            at,
            message: "empty credential name in `${cred:...}`".into(),
        });
    }
    // Credential names are flat identifiers under $CREDENTIALS_DIRECTORY;
    // reject anything that could escape it.
    if name.contains('/') || name.contains("..") {
        return Err(EnvError::Malformed {
            at,
            message: format!("invalid credential name {name:?} (must not contain '/' or '..')"),
        });
    }
    let dir = env
        .credentials_dir()
        .ok_or_else(|| EnvError::CredentialsDirUnset { name: name.into() })?;
    let path = format!("{}/{}", dir.trim_end_matches('/'), name);
    match env.read_file(&path) {
        Ok(contents) => Ok(trim_secret(&contents)),
        Err(e) => Err(EnvError::CredentialUnreadable {
            name: name.into(),
            message: e.to_string(),
        }),
    }
}

/// Strip trailing CR/LF from a file-sourced secret. Secret files are
/// conventionally written with a trailing newline (`echo "..." > f`);
/// internal and leading bytes are preserved so a secret that genuinely
/// contains whitespace stays intact.
fn trim_secret(contents: &str) -> String {
    contents.trim_end_matches(['\r', '\n']).to_string()
}

fn find_close_brace(bytes: &[u8], from: usize) -> Option<usize> {
    let mut i = from;
    while i < bytes.len() {
        if bytes[i] == b'}' {
            return Some(i);
        }
        i += 1;
    }
    None
}

fn is_valid_env_name(name: &str) -> bool {
    if name.is_empty() {
        return false;
    }
    let mut chars = name.chars();
    let first = chars.next().unwrap();
    if !(first.is_ascii_alphabetic() || first == '_') {
        return false;
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Cow-flavoured convenience for callers that only sometimes need
/// to allocate. Returns `Cow::Borrowed` when no `${` appears.
pub fn expand_cow<'a, E: EnvSource>(input: &'a str, env: &E) -> Result<Cow<'a, str>, EnvError> {
    if !input.contains("${") {
        Ok(Cow::Borrowed(input))
    } else {
        expand(input, env).map(Cow::Owned)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// Test env source backed by HashMaps — hermetic across env vars,
    /// "files" (keyed by path), and a fake `$CREDENTIALS_DIRECTORY`.
    #[derive(Default)]
    struct MapEnv {
        vars: HashMap<String, String>,
        files: HashMap<String, String>,
        cred_dir: Option<String>,
    }
    impl MapEnv {
        fn new<I: IntoIterator<Item = (&'static str, &'static str)>>(items: I) -> Self {
            Self {
                vars: items
                    .into_iter()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect(),
                ..Default::default()
            }
        }
        fn with_file(mut self, path: &str, contents: &str) -> Self {
            self.files.insert(path.to_string(), contents.to_string());
            self
        }
        fn with_cred_dir(mut self, dir: &str) -> Self {
            self.cred_dir = Some(dir.to_string());
            self
        }
    }
    impl EnvSource for MapEnv {
        fn lookup(&self, name: &str) -> Option<String> {
            self.vars.get(name).cloned()
        }
        fn read_file(&self, path: &str) -> std::io::Result<String> {
            self.files.get(path).cloned().ok_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::NotFound, "no such test file")
            })
        }
        fn credentials_dir(&self) -> Option<String> {
            self.cred_dir.clone()
        }
    }

    #[test]
    fn passthrough_when_no_references() {
        let env = MapEnv::new([]);
        assert_eq!(expand("hello world", &env).unwrap(), "hello world");
        assert_eq!(expand("", &env).unwrap(), "");
    }

    #[test]
    fn substitutes_set_variable() {
        let env = MapEnv::new([("FOO", "bar")]);
        assert_eq!(expand("x=${FOO}", &env).unwrap(), "x=bar");
    }

    #[test]
    fn substitutes_multiple_variables() {
        let env = MapEnv::new([("A", "1"), ("B", "2")]);
        assert_eq!(expand("${A}-${B}-${A}", &env).unwrap(), "1-2-1");
    }

    #[test]
    fn missing_variable_is_an_error() {
        let env = MapEnv::new([]);
        let err = expand("token=${MISSING}", &env).unwrap_err();
        match err {
            EnvError::Missing { name } => assert_eq!(name, "MISSING"),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn default_used_when_variable_missing() {
        let env = MapEnv::new([]);
        assert_eq!(expand("port=${PORT:-5060}", &env).unwrap(), "port=5060");
    }

    #[test]
    fn default_ignored_when_variable_set() {
        let env = MapEnv::new([("PORT", "5070")]);
        assert_eq!(expand("port=${PORT:-5060}", &env).unwrap(), "port=5070");
    }

    #[test]
    fn empty_default_yields_empty_string() {
        let env = MapEnv::new([]);
        assert_eq!(expand("x=${X:-}", &env).unwrap(), "x=");
    }

    #[test]
    fn unterminated_reference_is_malformed() {
        let env = MapEnv::new([]);
        let err = expand("x=${UNCLOSED", &env).unwrap_err();
        assert!(matches!(err, EnvError::Malformed { .. }));
    }

    #[test]
    fn invalid_var_name_reported() {
        let env = MapEnv::new([]);
        // Digits-leading name isn't a valid env var per POSIX.
        let err = expand("x=${1FOO}", &env).unwrap_err();
        assert!(matches!(err, EnvError::Malformed { .. }));
    }

    #[test]
    fn dollar_without_brace_is_literal() {
        let env = MapEnv::new([]);
        assert_eq!(expand("price=$5", &env).unwrap(), "price=$5");
    }

    #[test]
    fn multibyte_chars_pass_through() {
        let env = MapEnv::new([("WHO", "🦀")]);
        assert_eq!(expand("hi ${WHO} from λ", &env).unwrap(), "hi 🦀 from λ");
    }

    #[test]
    fn cow_borrows_when_unchanged() {
        let env = MapEnv::new([]);
        let c = expand_cow("plain", &env).unwrap();
        assert!(matches!(c, Cow::Borrowed(_)));
    }

    #[test]
    fn cow_owns_when_substituted() {
        let env = MapEnv::new([("X", "y")]);
        let c = expand_cow("${X}", &env).unwrap();
        assert!(matches!(c, Cow::Owned(_)));
        assert_eq!(c, "y");
    }

    // --- file: prefix --------------------------------------------------

    #[test]
    fn file_prefix_reads_contents() {
        let env = MapEnv::default().with_file("/run/secrets/tok", "s3cret");
        assert_eq!(
            expand("token=${file:/run/secrets/tok}", &env).unwrap(),
            "token=s3cret"
        );
    }

    #[test]
    fn file_prefix_trims_trailing_newline() {
        // A secret written with `echo "x" > f` has a trailing newline.
        let env = MapEnv::default().with_file("/s", "s3cret\n");
        assert_eq!(expand("${file:/s}", &env).unwrap(), "s3cret");
        let env = MapEnv::default().with_file("/s", "s3cret\r\n");
        assert_eq!(expand("${file:/s}", &env).unwrap(), "s3cret");
    }

    #[test]
    fn file_prefix_preserves_internal_whitespace() {
        let env = MapEnv::default().with_file("/s", "a b\tc\n");
        assert_eq!(expand("${file:/s}", &env).unwrap(), "a b\tc");
    }

    #[test]
    fn file_prefix_missing_file_is_an_error() {
        let env = MapEnv::default();
        let err = expand("${file:/nope}", &env).unwrap_err();
        match err {
            EnvError::FileUnreadable { path, .. } => assert_eq!(path, "/nope"),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn file_prefix_empty_path_is_malformed() {
        let env = MapEnv::default();
        let err = expand("${file:}", &env).unwrap_err();
        assert!(matches!(err, EnvError::Malformed { .. }));
    }

    // --- cred: prefix --------------------------------------------------

    #[test]
    fn cred_prefix_reads_from_credentials_dir() {
        let env = MapEnv::default()
            .with_cred_dir("/run/creds")
            .with_file("/run/creds/admin_token", "abc123\n");
        assert_eq!(
            expand("token=${cred:admin_token}", &env).unwrap(),
            "token=abc123"
        );
    }

    #[test]
    fn cred_prefix_without_dir_is_an_error() {
        let env = MapEnv::default(); // no CREDENTIALS_DIRECTORY
        let err = expand("${cred:admin_token}", &env).unwrap_err();
        match err {
            EnvError::CredentialsDirUnset { name } => assert_eq!(name, "admin_token"),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn cred_prefix_missing_credential_is_an_error() {
        let env = MapEnv::default().with_cred_dir("/run/creds");
        let err = expand("${cred:gone}", &env).unwrap_err();
        match err {
            EnvError::CredentialUnreadable { name, .. } => assert_eq!(name, "gone"),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn cred_prefix_rejects_path_traversal() {
        let env = MapEnv::default().with_cred_dir("/run/creds");
        for bad in ["${cred:../etc/passwd}", "${cred:sub/dir}", "${cred:}"] {
            let err = expand(bad, &env).unwrap_err();
            assert!(
                matches!(err, EnvError::Malformed { .. }),
                "expected malformed for {bad}, got {err:?}"
            );
        }
    }

    // --- prefix vs env-default disambiguation --------------------------

    #[test]
    fn env_default_takes_precedence_over_file_prefix() {
        // `${file:-x}` is env var `file` with default `x`, NOT a file ref.
        let env = MapEnv::default();
        assert_eq!(expand("${file:-fallback}", &env).unwrap(), "fallback");
        let env = MapEnv::new([("file", "set")]);
        assert_eq!(expand("${file:-fallback}", &env).unwrap(), "set");
    }

    #[test]
    fn bare_file_name_is_an_env_lookup() {
        // `${file}` (no colon) is a plain env var named `file`.
        let env = MapEnv::new([("file", "v")]);
        assert_eq!(expand("${file}", &env).unwrap(), "v");
    }

    #[test]
    fn prefixes_mix_with_env_vars_in_one_string() {
        let env = MapEnv::new([("HOST", "h")])
            .with_cred_dir("/c")
            .with_file("/c/pw", "p\n")
            .with_file("/etc/tok", "t\n");
        assert_eq!(
            expand("${HOST}:${cred:pw}:${file:/etc/tok}", &env).unwrap(),
            "h:p:t"
        );
    }
}
