//! Functions for working with the host environment.
use std::{
    borrow::Cow,
    ffi::{OsStr, OsString},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, RwLock,
    },
};

use once_cell::sync::Lazy;

// We keep the CWD as a static so that we can access it in places where we don't have access to the Editor
static CWD: RwLock<Option<PathBuf>> = RwLock::new(None);

/// Environment variables for the programs we run, when they aren't this process's own.
pub type Environment = Arc<[(OsString, OsString)]>;

static ENVIRONMENT: RwLock<Option<Environment>> = RwLock::new(None);

/// Get the current working directory.
/// This information is managed internally as the call to std::env::current_dir
/// might fail if the cwd has been deleted.
pub fn current_working_dir() -> PathBuf {
    if let Some(path) = &*CWD.read().unwrap() {
        return path.clone();
    }

    // implementation of crossplatform pwd -L
    // we want pwd -L so that symlinked directories are handled correctly
    let mut cwd = std::env::current_dir().expect("Couldn't determine current working directory");

    let pwd = std::env::var_os("PWD");
    #[cfg(windows)]
    let pwd = pwd.or_else(|| std::env::var_os("CD"));

    if let Some(pwd) = pwd.map(PathBuf::from) {
        if pwd.canonicalize().ok().as_ref() == Some(&cwd) {
            cwd = pwd;
        }
    }
    let mut dst = CWD.write().unwrap();
    *dst = Some(cwd.clone());

    cwd
}

static KEEP_PROCESS_DIR: AtomicBool = AtomicBool::new(false);

/// Makes [`set_current_working_dir`] leave the process's working directory alone. A server
/// shared by clients in different directories stays where it started, and everything that
/// needs a client's directory reads [`current_working_dir`] instead.
pub fn keep_process_working_dir() {
    KEEP_PROCESS_DIR.store(true, Ordering::Relaxed);
}

/// Update the current working directory.
pub fn set_current_working_dir(path: impl AsRef<Path>) -> std::io::Result<Option<PathBuf>> {
    let path = crate::path::canonicalize(path);
    if !KEEP_PROCESS_DIR.load(Ordering::Relaxed) {
        std::env::set_current_dir(&path)?;
    } else if !std::fs::metadata(&path)?.is_dir() {
        return Err(std::io::ErrorKind::NotADirectory.into());
    }
    let mut cwd = CWD.write().unwrap();

    Ok(cwd.replace(path))
}

/// Replaces the working directory [`current_working_dir`] reports, without changing the
/// process's. A server with several clients swaps in the active client's directory this way.
/// `None` means the process's working directory.
pub fn swap_current_working_dir(path: Option<PathBuf>) -> Option<PathBuf> {
    std::mem::replace(&mut *CWD.write().unwrap(), path)
}

/// Replaces the environment [`command`] gives programs. A server with several clients swaps in
/// the active client's environment this way. `None` means the process's environment.
pub fn swap_environment(env: Option<Environment>) -> Option<Environment> {
    std::mem::replace(&mut *ENVIRONMENT.write().unwrap(), env)
}

/// A command that runs in [`current_working_dir`] with the environment from
/// [`swap_environment`]. In a server, those are the active client's, not the process's.
pub fn command(program: impl AsRef<OsStr>) -> std::process::Command {
    let mut command = std::process::Command::new(program);
    command.current_dir(current_working_dir());
    if let Some(env) = &*ENVIRONMENT.read().unwrap() {
        command
            .env_clear()
            .envs(env.iter().map(|(key, value)| (key, value)));
    }
    command
}

/// The environment [`command`] gives programs, if it isn't this process's own: in a server,
/// the active client's.
pub fn environment() -> Option<Environment> {
    ENVIRONMENT.read().unwrap().clone()
}

/// An environment variable as [`command`] gives it to programs: in a server, the active
/// client's.
pub fn var_os(name: impl AsRef<OsStr>) -> Option<OsString> {
    let name = name.as_ref();
    match &*ENVIRONMENT.read().unwrap() {
        Some(env) => env
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.clone()),
        None => std::env::var_os(name),
    }
}

/// Checks if the given environment variable is set.
pub fn env_var_is_set(env_var_name: &str) -> bool {
    std::env::var_os(env_var_name).is_some()
}

/// Checks if a binary with the given name exists.
pub fn binary_exists<T: AsRef<OsStr>>(binary_name: T) -> bool {
    which::which(binary_name).is_ok()
}

/// Attempts to find a binary of the given name. See [which](https://linux.die.net/man/1/which).
pub fn which<T: AsRef<OsStr>>(
    binary_name: T,
) -> Result<std::path::PathBuf, ExecutableNotFoundError> {
    let binary_name = binary_name.as_ref();
    which::which(binary_name).map_err(|err| ExecutableNotFoundError {
        command: binary_name.to_string_lossy().into_owned(),
        inner: err,
    })
}

fn find_brace_end(src: &[u8]) -> Option<usize> {
    use regex_automata::meta::Regex;

    static REGEX: Lazy<Regex> = Lazy::new(|| Regex::builder().build("[{}]").unwrap());
    let mut depth = 0;
    for mat in REGEX.find_iter(src) {
        let pos = mat.start();
        match src[pos] {
            b'{' => depth += 1,
            b'}' if depth == 0 => return Some(pos),
            b'}' => depth -= 1,
            _ => unreachable!(),
        }
    }
    None
}

fn expand_impl(src: &OsStr, mut resolve: impl FnMut(&OsStr) -> Option<OsString>) -> Cow<'_, OsStr> {
    use regex_automata::meta::Regex;

    static REGEX: Lazy<Regex> = Lazy::new(|| {
        Regex::builder()
            .build_many(&[
                r"\$\{([^\}:]+):-",
                r"\$\{([^\}:]+):=",
                r"\$\{([^\}-]+)-",
                r"\$\{([^\}=]+)=",
                r"\$\{([^\}]+)",
                r"\$(\w+)",
            ])
            .unwrap()
    });

    let bytes = src.as_encoded_bytes();
    let mut res = Vec::with_capacity(bytes.len());
    let mut pos = 0;
    for captures in REGEX.captures_iter(bytes) {
        let mat = captures.get_match().unwrap();
        let pattern_id = mat.pattern().as_usize();
        let mut range = mat.range();
        // A pattern may match multiple times on a single variable, for example `${HOME:-$HOME}`:
        // `${HOME:-` matches and also the default value (`$HOME`). Skip past any variables which
        // have already been expanded.
        if range.start < pos {
            continue;
        }
        let var = &bytes[captures.get_group(1).unwrap().range()];
        let default = if pattern_id != 5 {
            let Some(bracket_pos) = find_brace_end(&bytes[range.end..]) else {
                break;
            };
            let default = &bytes[range.end..range.end + bracket_pos];
            range.end += bracket_pos + 1;
            default
        } else {
            &[]
        };
        // safety: this is a codepoint aligned substring of an osstr (always valid)
        let var = unsafe { OsStr::from_encoded_bytes_unchecked(var) };
        let expansion = resolve(var);
        let expansion = match &expansion {
            Some(val) => {
                if val.is_empty() && pattern_id < 2 {
                    default
                } else {
                    val.as_encoded_bytes()
                }
            }
            None => default,
        };
        res.extend_from_slice(&bytes[pos..range.start]);
        pos = range.end;
        res.extend_from_slice(expansion);
    }
    if pos == 0 {
        src.into()
    } else {
        res.extend_from_slice(&bytes[pos..]);
        // safety: this is a composition of valid osstr (and codepoint aligned slices which are also valid)
        unsafe { OsString::from_encoded_bytes_unchecked(res) }.into()
    }
}

/// performs substitution of enviorment variables. Supports the following (POSIX) syntax:
///
/// * `$<var>`, `${<var>}`
/// * `${<var>:-<default>}`, `${<var>-<default>}`
/// * `${<var>:=<default>}`, `${<var>=default}`
///
pub fn expand<S: AsRef<OsStr> + ?Sized>(src: &S) -> Cow<'_, OsStr> {
    expand_impl(src.as_ref(), |var| std::env::var_os(var))
}

#[derive(Debug)]
pub struct ExecutableNotFoundError {
    command: String,
    inner: which::Error,
}

impl std::fmt::Display for ExecutableNotFoundError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "command '{}' not found: {}", self.command, self.inner)
    }
}

impl std::error::Error for ExecutableNotFoundError {}

#[cfg(test)]
mod tests {
    use std::ffi::{OsStr, OsString};

    use super::{current_working_dir, expand_impl, set_current_working_dir};

    #[test]
    fn current_dir_is_set() {
        let new_path = dunce::canonicalize(std::env::temp_dir()).unwrap();
        let cwd = current_working_dir();
        assert_ne!(cwd, new_path);

        set_current_working_dir(&new_path).expect("Couldn't set new path");

        let cwd = current_working_dir();
        assert_eq!(cwd, new_path);
    }

    macro_rules! assert_env_expand {
        ($env: expr, $lhs: expr, $rhs: expr) => {
            assert_eq!(&*expand_impl($lhs.as_ref(), $env), OsStr::new($rhs));
        };
    }

    /// paths that should work on all platforms
    #[test]
    fn test_env_expand() {
        let env = |var: &OsStr| -> Option<OsString> {
            match var.to_str().unwrap() {
                "FOO" => Some("foo".into()),
                "EMPTY" => Some("".into()),
                _ => None,
            }
        };
        assert_env_expand!(env, "pass_trough", "pass_trough");
        assert_env_expand!(env, "$FOO", "foo");
        assert_env_expand!(env, "bar/$FOO/baz", "bar/foo/baz");
        assert_env_expand!(env, "bar/${FOO}/baz", "bar/foo/baz");
        assert_env_expand!(env, "baz/${BAR:-bar}/foo", "baz/bar/foo");
        assert_env_expand!(env, "baz/${FOO:-$FOO}/foo", "baz/foo/foo");
        assert_env_expand!(env, "baz/${BAR:=bar}/foo", "baz/bar/foo");
        assert_env_expand!(env, "baz/${BAR-bar}/foo", "baz/bar/foo");
        assert_env_expand!(env, "baz/${BAR=bar}/foo", "baz/bar/foo");
        assert_env_expand!(env, "baz/${EMPTY:-bar}/foo", "baz/bar/foo");
        assert_env_expand!(env, "baz/${EMPTY:=bar}/foo", "baz/bar/foo");
        assert_env_expand!(env, "baz/${EMPTY-bar}/foo", "baz//foo");
        assert_env_expand!(env, "baz/${EMPTY=bar}/foo", "baz//foo");
    }
}
