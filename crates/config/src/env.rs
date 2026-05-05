//! `${VAR}` and `${VAR:-default}` expansion at config load time.
//!
//! Per `docs/DEV_PLAN.md` §6.6 every string value in the TOML can
//! reference an environment variable. We expand before parsing TOML
//! into raw types so missing env vars surface as a single
//! [`EnvError`] with line context, rather than as a downstream
//! parse failure on a half-substituted string.
//!
//! Resolved values are never logged in expanded form — see
//! [`expand`] for the redaction approach.

use std::borrow::Cow;

use thiserror::Error;

/// Looks up an environment variable. Pluggable so tests can drive
/// expansion without touching the real environment.
pub trait EnvSource {
    fn lookup(&self, name: &str) -> Option<String>;
}

/// Default impl: reads from the process environment.
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

    /// Malformed `${...}` — unterminated, empty name, etc.
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
            let (name, default) = match inner.split_once(":-") {
                Some((n, d)) => (n, Some(d)),
                None => (inner, None),
            };
            if !is_valid_env_name(name) {
                return Err(EnvError::Malformed {
                    at: start,
                    message: format!("invalid env var name {name:?}"),
                });
            }
            match env.lookup(name) {
                Some(value) => out.push_str(&value),
                None => match default {
                    Some(d) => out.push_str(d),
                    None => return Err(EnvError::Missing { name: name.into() }),
                },
            }
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

    /// Test env source backed by a HashMap.
    struct MapEnv(HashMap<String, String>);
    impl MapEnv {
        fn new<I: IntoIterator<Item = (&'static str, &'static str)>>(items: I) -> Self {
            Self(
                items
                    .into_iter()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect(),
            )
        }
    }
    impl EnvSource for MapEnv {
        fn lookup(&self, name: &str) -> Option<String> {
            self.0.get(name).cloned()
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
}
