//! Reading configuration from the process environment or a `KEY=value` file.
//!
//! [`Env`] is the lookup every setting goes through: the process environment wins, then the file
//! named by `SWISHA_ENV_FILE`, parsed by [`parse`]. A blank value counts as absent, which is what
//! lets the shipped templates list every setting empty.
//!
//! ```no_run
//! use swisha::env::Env;
//!
//! let env = Env::discover()?;
//! let listen = env.optional("SWISH_SERVER_ADDR", "127.0.0.1:8083");
//! # Ok::<(), swisha::error::BoxError>(())
//! ```

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use crate::error::{Context, Result, err};

/// Configuration lookup: the process environment first, then an optional file.
///
/// The file becomes a map and is never written back. `env::set_var` is unsafe in edition 2024
/// because POSIX `setenv` races other threads, and a runtime is already running by then.
#[derive(Debug)]
pub struct Env {
    file: Option<EnvFile>,
    process: bool,
}

impl Env {
    /// Reads the process environment only.
    pub fn from_process() -> Self {
        Self { file: None, process: true }
    }

    /// Loads the file named by `SWISHA_ENV_FILE`, if that variable is set.
    ///
    /// A named file that cannot be read is an error rather than a silent fallback: a service
    /// told to load its configuration from somewhere should not start with a different one. So
    /// is one other users can open, since it holds the database password and the API secret.
    ///
    /// # Errors
    ///
    /// When the named file cannot be read, is open to other users, or does not parse.
    pub fn discover() -> Result<Self> {
        match std::env::var("SWISHA_ENV_FILE") {
            Ok(path) if !path.trim().is_empty() => Ok(Self {
                file: Some(EnvFile::load_private(Path::new(path.trim()))?),
                process: true,
            }),
            _ => Ok(Self::from_process()),
        }
    }

    /// Uses an already-loaded file, under the process environment.
    pub fn with_file(file: EnvFile) -> Self {
        Self { file: Some(file), process: true }
    }

    /// Uses an already-loaded file and nothing else: the process environment is never read. For
    /// a test or a tool that must not pick up whatever the shell around it exported.
    pub fn file_only(file: EnvFile) -> Self {
        Self { file: Some(file), process: false }
    }

    /// Looks a key up. The process environment wins, so a systemd unit or `docker --env-file`
    /// can override a value without the file being edited.
    pub fn get(&self, key: &str) -> Option<String> {
        if self.process
            && let Ok(value) = std::env::var(key)
        {
            return Some(value);
        }
        self.file.as_ref()?.get(key).map(str::to_string)
    }

    /// A setting that must be present and non-blank.
    ///
    /// # Errors
    ///
    /// When the key is missing or blank, naming the file when one is loaded.
    pub fn required(&self, key: &str) -> Result<String> {
        self.get(key)
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| match self.source() {
                Some(path) => err(format!(
                    "Missing required setting {key}: not in the environment, and not in {}",
                    path.display()
                )),
                None => err(format!("Missing required env var: {key}")),
            })
    }

    /// A setting with a fallback.
    ///
    /// A key that is present but blank reads as not configured, exactly as
    /// [`required`](Self::required) treats it. That is what lets a template ship its fields
    /// empty: without it a blank line would silently replace the default.
    pub fn optional(&self, key: &str, default: &str) -> String {
        self.get(key)
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| default.to_string())
    }

    /// Every key the file sets to a non-blank value, sorted. The process environment is left out:
    /// it carries everything else on the machine.
    pub fn file_keys(&self) -> std::collections::BTreeSet<String> {
        self.file
            .iter()
            .flat_map(|file| file.values.iter())
            .filter(|(_, value)| !value.trim().is_empty())
            .map(|(key, _)| key.clone())
            .collect()
    }

    /// Where a file-supplied value came from, so a misconfiguration can name the file.
    pub fn source(&self) -> Option<&Path> {
        self.file.as_ref().map(|file| file.path.as_path())
    }
}

/// A parsed `KEY=value` file. Debug output names the keys and never shows a value.
pub struct EnvFile {
    path: PathBuf,
    values: HashMap<String, String>,
}

impl std::fmt::Debug for EnvFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let keys: std::collections::BTreeSet<&String> = self.values.keys().collect();
        f.debug_struct("EnvFile").field("path", &self.path).field("keys", &keys).finish()
    }
}

impl EnvFile {
    /// Reads and parses a file, whoever else can read it.
    ///
    /// # Errors
    ///
    /// When the file cannot be read or does not parse.
    pub fn load(path: &Path) -> Result<Self> {
        let contents = std::fs::read_to_string(path)
            .with_context(|| format!("Cannot read env file: {}", path.display()))?;
        Self::parsed(path, &contents)
    }

    /// Reads and parses a file, refusing one other users can open, as the service does with the
    /// file `SWISHA_ENV_FILE` names.
    ///
    /// # Errors
    ///
    /// When the file cannot be read, does not parse, or is open to anyone but its owner: it must be
    /// readable by its owner alone (chmod 600).
    pub fn load_private(path: &Path) -> Result<Self> {
        Self::parsed(path, &crate::util::fs::read_private(path, "env file")?)
    }

    fn parsed(path: &Path, contents: &str) -> Result<Self> {
        Ok(Self {
            path: path.to_path_buf(),
            values: parse(contents, path)?,
        })
    }

    /// One value from the file, ignoring the process environment.
    pub fn get(&self, key: &str) -> Option<&str> {
        self.values.get(key).map(String::as_str)
    }

    /// How many settings the file carries.
    pub fn len(&self) -> usize {
        self.values.len()
    }

    /// Whether the file carries no settings at all. A file of nothing but comments is still a
    /// loaded file.
    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }
}

/// Parses `KEY=value` lines.
///
/// Accepts an `export ` prefix, skips blanks and `#` comments, strips matching quotes without
/// interpreting escapes, and cuts an unquoted value at a ` #` comment. `path` only names the file
/// in errors.
///
/// # Errors
///
/// For a line that is not `KEY=value`, a key that is not a variable name, or a key set twice:
/// silently taking one of two is how a service ends up running configuration nobody intended.
///
/// # Examples
///
/// ```
/// use std::path::Path;
/// use swisha::env::parse;
///
/// let file = "export DB_NAME=swisha\nDB_PASS=\"p #ss\"\nRUST_LOG=   # blank means the default\n";
/// let values = parse(file, Path::new("prod.env"))?;
/// assert_eq!(values["DB_NAME"], "swisha");
/// assert_eq!(values["DB_PASS"], "p #ss", "quoting keeps a ` #`");
/// assert_eq!(values["RUST_LOG"], "");
///
/// assert!(parse("DB_NAME=a\nDB_NAME=b\n", Path::new("prod.env")).is_err(), "set twice");
/// # Ok::<(), swisha::error::BoxError>(())
/// ```
pub fn parse(contents: &str, path: &Path) -> Result<HashMap<String, String>> {
    let mut values = HashMap::new();

    for (index, raw) in contents.lines().enumerate() {
        let number = index + 1;
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        let line = line.strip_prefix("export ").unwrap_or(line).trim_start();
        let Some((key, value)) = line.split_once('=') else {
            return Err(err(format!(
                "{}:{number}: expected KEY=value",
                path.display()
            )));
        };

        let key = key.trim();
        if !is_valid_key(key) {
            return Err(err(format!(
                "{}:{number}: '{key}' is not a valid variable name",
                path.display()
            )));
        }

        // A duplicate is almost always a mistake, and silently taking one of the two is how a
        // service ends up running with configuration nobody intended.
        if values.contains_key(key) {
            return Err(err(format!(
                "{}:{number}: '{key}' is set more than once",
                path.display()
            )));
        }

        values.insert(key.to_string(), unquote(value).to_string());
    }

    Ok(values)
}

fn is_valid_key(key: &str) -> bool {
    let mut chars = key.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

// A quoted value ends at its first closing quote followed by nothing or a ` #` comment, verbatim.
// An unquoted one is cut at ` #` before trimming, so a blank field never reads as its comment.
fn unquote(value: &str) -> &str {
    let trimmed = value.trim();
    for quote in ['"', '\''] {
        let Some(inner) = trimmed.strip_prefix(quote) else { continue };
        for (end, _) in inner.match_indices(quote) {
            let rest = &inner[end + 1..];
            if rest.is_empty() || (rest.starts_with(char::is_whitespace) && rest.trim_start().starts_with('#')) {
                return &inner[..end];
            }
        }
    }
    match value.split_once(" #") {
        Some((before, _)) => before.trim(),
        None => trimmed,
    }
}
