//! Layered TOML editing for `wallermax.toml` and `wallermax.local.toml`.
//!
//! The server reads its configuration as **layers** — built-in defaults,
//! then the versioned `wallermax.toml`, then the personal
//! `wallermax.local.toml` — so the manager edits exactly those two files
//! and validates **the pair**, never a file in isolation: an edit to the
//! base is checked with the local overrides applied, the way the server
//! will actually see it at startup.
//!
//! Validation goes through
//! [`wallermax_server::config::AppConfig::load_from_toml_layers`] — the
//! server's real `config`-crate pipeline — so the editor and the server
//! can never disagree about what is valid. On top of that pass, a syntax
//! pre-check with `toml_edit` catches malformed files **with their line
//! numbers** before the semantic layer even runs.
//!
//! Every write is three steps, in order:
//!
//! 1. **validate** — the pair, with the incoming text in place;
//! 2. **backup** — the file being replaced is copied to
//!    `wallermax.toml.bak-<timestamp>`;
//! 3. **write atomically** — a sibling temporary renamed over the target,
//!    so a crash mid-write can never leave a half-written configuration.
//!
//! Form-style edits are *surgical*: `toml_edit` rewrites only the touched
//! keys, preserving comments, ordering and formatting everywhere else —
//! and creating intermediate tables when a key does not exist yet.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;
use toml_edit::{DocumentMut, Item, Table};
use wallermax_server::config::AppConfig;

/// The versioned project configuration.
pub const BASE_FILE: &str = "wallermax.toml";

/// The git-ignored personal overrides.
pub const LOCAL_FILE: &str = "wallermax.local.toml";

/// Which of the two configuration layers an operation targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigWhich {
    /// `wallermax.toml`.
    Base,
    /// `wallermax.local.toml`.
    Local,
}

impl ConfigWhich {
    /// Parses the UI-facing name (`base` / `local`).
    ///
    /// # Errors
    ///
    /// A human-readable message for any other value.
    pub fn from_name(name: &str) -> Result<Self, String> {
        match name {
            "base" => Ok(Self::Base),
            "local" => Ok(Self::Local),
            other => Err(format!(
                "unknown configuration file `{other}`; expected `base` or `local`"
            )),
        }
    }

    /// The file name on disk.
    pub fn file_name(self) -> &'static str {
        match self {
            Self::Base => BASE_FILE,
            Self::Local => LOCAL_FILE,
        }
    }
}

/// One timestamped backup of a configuration file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BackupInfo {
    pub file_name: String,
    pub bytes: u64,
    pub modified_unix: u64,
}

/// Owns the directory holding both configuration files.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigManager {
    dir: PathBuf,
}

impl ConfigManager {
    /// A manager for the configuration living in `dir`.
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    /// The directory both configuration files live in.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The full path of one of the two files.
    pub fn path(&self, which: ConfigWhich) -> PathBuf {
        self.dir.join(which.file_name())
    }

    /// Whether the file exists on disk.
    pub fn exists(&self, which: ConfigWhich) -> bool {
        self.path(which).is_file()
    }

    /// Reads one layer. A missing file reads as empty — the server treats
    /// a missing file the same way, and empty inputs fall back to the
    /// built-in defaults.
    ///
    /// # Errors
    ///
    /// A human-readable message when the file exists but cannot be read.
    pub fn read(&self, which: ConfigWhich) -> Result<String, String> {
        let path = self.path(which);
        match fs::read_to_string(&path) {
            Ok(text) => Ok(text),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
            Err(error) => Err(format!("could not read {}: {error}", path.display())),
        }
    }

    /// Validates a configuration pair, with optional replacement text for
    /// either layer — the "what if I save this" view.
    ///
    /// # Errors
    ///
    /// The first problem found: a syntax error names its file and line; a
    /// semantic problem carries the server's own validation message.
    pub fn validate(&self, base: Option<&str>, local: Option<&str>) -> Result<(), String> {
        if let Some(error) = syntax_error(BASE_FILE, base) {
            return Err(error);
        }
        if let Some(error) = syntax_error(LOCAL_FILE, local) {
            return Err(error);
        }
        AppConfig::load_from_toml_layers(base, local)
            .map(|_| ())
            .map_err(|error| error.to_string())
    }

    /// Validates and loads the pair currently on disk.
    ///
    /// # Errors
    ///
    /// See [`ConfigManager::validate`].
    pub fn load_merged(&self) -> Result<AppConfig, String> {
        let base = self.read(ConfigWhich::Base)?;
        let local = self.read(ConfigWhich::Local)?;
        AppConfig::load_from_toml_layers(Some(&base), Some(&local))
            .map_err(|error| error.to_string())
    }

    /// Overwrites one layer with `content`: validate first, then back up
    /// the file being replaced, then write atomically. A failed
    /// validation leaves the disk untouched — not even a backup appears.
    ///
    /// # Errors
    ///
    /// See [`ConfigManager::validate`], plus filesystem problems.
    pub fn write_raw(&self, which: ConfigWhich, content: &str) -> Result<(), String> {
        let base = self.read(ConfigWhich::Base)?;
        let local = self.read(ConfigWhich::Local)?;
        let (base, local) = match which {
            ConfigWhich::Base => (Some(content.to_owned()), Some(local)),
            ConfigWhich::Local => (Some(base), Some(content.to_owned())),
        };
        self.validate(base.as_deref(), local.as_deref())?;
        self.backup(which)?;
        crate::settings::write_atomically(&self.path(which), content.as_bytes())
    }

    /// Applies surgical edits to one layer: every `key` is a dotted path
    /// (`server.port`), missing tables are created on the way down, and
    /// everything else — comments included — is preserved verbatim. The
    /// result goes through [`ConfigManager::write_raw`].
    ///
    /// # Errors
    ///
    /// When the current file is malformed TOML, a dotted path crosses a
    /// non-table value, or the validation of the result fails.
    pub fn set_values(&self, which: ConfigWhich, pairs: &[(String, String)]) -> Result<(), String> {
        let mut document = self
            .read(which)?
            .parse::<DocumentMut>()
            .map_err(|error| format!("{}: {error}", which.file_name()))?;
        for (key, value) in pairs {
            set_dotted(&mut document, key, to_toml_value(value)?)?;
        }
        let updated = document.to_string();
        self.write_raw(which, &updated)
    }

    /// Reads one dotted path as display text (strings unquoted). Answers
    /// `None` when the file is missing the key or cannot be parsed.
    pub fn get_value(&self, which: ConfigWhich, dotted: &str) -> Option<String> {
        let mut document = self.read(which).ok()?.parse::<DocumentMut>().ok()?;
        let (tables, leaf) = split_dotted(dotted)?;
        let mut table = document.as_table_mut();
        for part in &tables {
            table = table.get_mut(part.as_str())?.as_table_mut()?;
        }
        let item = table.get_mut(leaf.as_str())?;
        Some(match item {
            Item::Value(toml_edit::Value::String(text)) => text.value().to_owned(),
            other => other.to_string().trim().to_owned(),
        })
    }

    /// Lists the backups of one layer, newest first.
    pub fn backups(&self, which: ConfigWhich) -> Vec<BackupInfo> {
        let prefix = format!("{}.bak-", which.file_name());
        let mut backups: Vec<BackupInfo> = Vec::new();
        let Ok(entries) = fs::read_dir(&self.dir) else {
            return backups;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            if !name.starts_with(&prefix) {
                continue;
            }
            let Ok(meta) = entry.metadata() else {
                continue;
            };
            if !meta.is_file() {
                continue;
            }
            backups.push(BackupInfo {
                file_name: name.to_owned(),
                bytes: meta.len(),
                modified_unix: meta
                    .modified()
                    .ok()
                    .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
                    .map(|d| d.as_secs())
                    .unwrap_or_default(),
            });
        }
        backups.sort_by(|a, b| b.file_name.cmp(&a.file_name));
        backups
    }

    /// Restores a backup: its content is validated as the new layer, the
    /// current file is backed up first (so restores can be undone the
    /// same way), and the write is atomic.
    ///
    /// # Errors
    ///
    /// Unknown or unsafe names, unreadable backups, or validation
    /// failures — with the reason.
    pub fn restore_backup(&self, which: ConfigWhich, file_name: &str) -> Result<(), String> {
        let prefix = format!("{}.bak-", which.file_name());
        if file_name.contains('/') || file_name.contains('\\') || !file_name.starts_with(&prefix) {
            return Err(format!(
                "`{file_name}` is not a backup of {}",
                which.file_name()
            ));
        }
        let path = self.dir.join(file_name);
        let content = fs::read_to_string(&path)
            .map_err(|error| format!("could not read {file_name}: {error}"))?;
        self.write_raw(which, &content)
    }

    /// Copies the current file of one layer to a fresh timestamped
    /// backup. Answers the backup's name when a file was backed up.
    fn backup(&self, which: ConfigWhich) -> Result<Option<String>, String> {
        let path = self.path(which);
        if !path.is_file() {
            return Ok(None);
        }
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default();
        let mut candidate = format!("{}.bak-{stamp}", which.file_name());
        let mut n = 1;
        while self.dir.join(&candidate).exists() {
            n += 1;
            candidate = format!("{}.bak-{stamp}-{n}", which.file_name());
        }
        fs::copy(&path, self.dir.join(&candidate))
            .map_err(|error| format!("could not back up {}: {error}", path.display()))?;
        Ok(Some(candidate))
    }
}

/// Names a syntax error with its file and 1-based line, if the text is
/// not parseable TOML. Empty and absent text never has syntax errors.
fn syntax_error(file: &str, text: Option<&str>) -> Option<String> {
    let text = text?;
    if text.trim().is_empty() {
        return None;
    }
    let error = match text.parse::<DocumentMut>() {
        Ok(_) => return None,
        Err(error) => error,
    };
    let line = error
        .span()
        .map(|span| text[..span.start].matches('\n').count() + 1)
        .unwrap_or(0);
    if line > 0 {
        Some(format!("{file}: line {line}: {error}"))
    } else {
        Some(format!("{file}: {error}"))
    }
}

/// Splits a dotted path into its table parts and leaf key.
fn split_dotted(dotted: &str) -> Option<(Vec<String>, String)> {
    let mut parts: Vec<String> = dotted
        .split('.')
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .map(str::to_owned)
        .collect();
    let leaf = parts.pop()?;
    Some((parts, leaf))
}

/// Writes `value` at a dotted path, creating missing tables on the way.
fn set_dotted(document: &mut DocumentMut, dotted: &str, value: Item) -> Result<(), String> {
    let Some((tables, leaf)) = split_dotted(dotted) else {
        return Err(format!("`{dotted}` is not a dotted key"));
    };
    let mut table = document.as_table_mut();
    for part in &tables {
        if table.get(part.as_str()).is_none() {
            table.insert(part.as_str(), Item::Table(Table::new()));
        }
        table = table
            .get_mut(part.as_str())
            .and_then(Item::as_table_mut)
            .ok_or_else(|| format!("`{dotted}` crosses a value that is not a table"))?;
    }
    table.insert(leaf.as_str(), value);
    Ok(())
}

/// Parses a form value into a TOML value: numbers, booleans, arrays and
/// quoted strings parse as their TOML selves; anything else is treated
/// as a plain string.
fn to_toml_value(raw: &str) -> Result<Item, String> {
    match raw.parse::<toml_edit::Value>() {
        Ok(value) => Ok(Item::Value(value)),
        Err(_) => Ok(Item::Value(toml_edit::Value::from(raw))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manager() -> (ConfigManager, PathBuf) {
        let dir = crate::testutil::temp_dir("config");
        (ConfigManager::new(&dir), dir)
    }

    #[test]
    fn empty_inputs_fall_back_to_defaults() {
        let (manager, _dir) = manager();
        manager
            .validate(Some(""), Some(""))
            .expect("empty layers are exactly the pristine defaults");
        let merged = manager
            .load_merged()
            .expect("an empty pair loads as defaults");
        assert_eq!(merged.server.port, 8080);
        assert_eq!(merged.logging.level, "info");
    }

    #[test]
    fn missing_local_reads_as_empty() {
        let (manager, _dir) = manager();
        assert!(!manager.exists(ConfigWhich::Local));
        assert_eq!(
            manager.read(ConfigWhich::Local).expect("reads as empty"),
            ""
        );
        assert!(!manager.exists(ConfigWhich::Base));
        assert_eq!(manager.read(ConfigWhich::Base).expect("reads as empty"), "");
    }

    #[test]
    fn local_overrides_base_in_validation() {
        let (manager, _dir) = manager();
        let base = "[server]\nport = 9000\n";
        let local = "[server]\nport = 8081\n";
        manager
            .write_raw(ConfigWhich::Base, base)
            .expect("the base writes");
        manager
            .write_raw(ConfigWhich::Local, local)
            .expect("the local overrides write");
        let merged = manager.load_merged().expect("the pair is valid");
        assert_eq!(merged.server.port, 8081, "the local layer wins");

        // An override that only makes sense together with the base still
        // validates through the server's real rules.
        manager
            .write_raw(ConfigWhich::Local, "[logging]\nlevel = \"debug\"\n")
            .expect("a different shape of override validates too");
        assert_eq!(manager.load_merged().expect("valid").logging.level, "debug");
    }

    #[test]
    fn raw_writes_validate_first_and_keep_a_backup() {
        let (manager, _dir) = manager();
        manager
            .write_raw(ConfigWhich::Base, "[server]\nport = 8080\n")
            .expect("seed a valid base");

        // A broken write is refused and leaves no trace — not even a backup.
        let error = manager
            .write_raw(ConfigWhich::Base, "port = ")
            .expect_err("syntax errors are refused");
        assert!(error.contains("line"), "the error names the line: {error}");
        assert_eq!(
            manager.read(ConfigWhich::Base).expect("read back"),
            "[server]\nport = 8080\n",
            "the file is untouched after a refused write"
        );
        assert!(
            manager.backups(ConfigWhich::Base).is_empty(),
            "refused writes do not create backups"
        );

        // A valid write lands, and the previous file is backed up.
        manager
            .write_raw(ConfigWhich::Base, "[server]\nport = 9005\n")
            .expect("the good write lands");
        let backups = manager.backups(ConfigWhich::Base);
        assert_eq!(backups.len(), 1, "exactly one backup: {backups:?}");
        assert_eq!(
            manager.read(ConfigWhich::Base).expect("read back"),
            "[server]\nport = 9005\n"
        );
    }

    #[test]
    fn surgical_edits_create_missing_tables() {
        let (manager, _dir) = manager();
        manager
            .set_values(
                ConfigWhich::Base,
                &[
                    ("server.port".to_owned(), "9000".to_owned()),
                    ("logging.level".to_owned(), "debug".to_owned()),
                    ("rate_limit.capacity".to_owned(), "120".to_owned()),
                ],
            )
            .expect("the edits apply");
        let text = manager.read(ConfigWhich::Base).expect("read back");
        let document = text.parse::<DocumentMut>().expect("the result is TOML");
        assert_eq!(document["server"]["port"].as_integer(), Some(9000));
        assert_eq!(
            document["logging"]["level"].as_str(),
            Some("debug"),
            "plain strings stay strings"
        );
        assert_eq!(document["rate_limit"]["capacity"].as_integer(), Some(120));
        let merged = manager.load_merged().expect("the pair validates");
        assert_eq!(merged.server.port, 9000);
    }

    #[test]
    fn surgical_edits_preserve_comments() {
        let (manager, _dir) = manager();
        manager
            .write_raw(
                ConfigWhich::Base,
                "# operated by wallermax-manager\n[server]\nport = 8080 # the local port\n",
            )
            .expect("seed a commented base");
        manager
            .set_values(
                ConfigWhich::Base,
                &[("server.host".to_owned(), "127.0.0.1".to_owned())],
            )
            .expect("the edit applies");
        let text = manager.read(ConfigWhich::Base).expect("read back");
        assert!(
            text.contains("# operated by wallermax-manager"),
            "file comments survive: {text}"
        );
        assert!(
            text.contains("# the local port"),
            "inline comments survive: {text}"
        );
        assert!(
            text.contains("host = \"127.0.0.1\""),
            "the new key is there: {text}"
        );
    }

    #[test]
    fn validation_names_the_offending_line() {
        let (manager, _dir) = manager();
        let error = manager
            .validate(Some("a = 1\nb = \n"), None)
            .expect_err("the syntax error is caught");
        assert!(error.contains("line 2"), "the line is named: {error}");
        assert!(error.contains(BASE_FILE), "the file is named: {error}");

        // Semantic problems carry the server's own message.
        let error = manager
            .validate(Some("[server]\nrequest_timeout_secs = 0\n"), None)
            .expect_err("the server's own invariant fires");
        assert!(
            error.contains("request_timeout_secs"),
            "the offending key is named: {error}"
        );
    }

    #[test]
    fn backups_restore_round_trip() {
        let (manager, _dir) = manager();
        manager
            .write_raw(ConfigWhich::Base, "[server]\nport = 8080\n")
            .expect("first version");
        manager
            .write_raw(ConfigWhich::Base, "[server]\nport = 9000\n")
            .expect("second version");
        let backups = manager.backups(ConfigWhich::Base);
        let newest = backups.first().expect("there is a backup");
        manager
            .restore_backup(ConfigWhich::Base, &newest.file_name)
            .expect("the restore validates");
        assert_eq!(
            manager.read(ConfigWhich::Base).expect("read back"),
            "[server]\nport = 8080\n",
            "the backup content is back"
        );
        // The restore itself left a backup of what it replaced.
        assert!(manager.backups(ConfigWhich::Base).len() >= 2);
        // Unknown or unsafe names are refused.
        assert!(manager
            .restore_backup(ConfigWhich::Base, "nope.bak-1")
            .is_err());
        assert!(manager
            .restore_backup(ConfigWhich::Base, &format!("{}../nope", BASE_FILE))
            .is_err());
    }

    #[test]
    fn get_value_reads_through_the_layers() {
        let (manager, _dir) = manager();
        manager
            .set_values(
                ConfigWhich::Base,
                &[("server.port".to_owned(), "8080".to_owned())],
            )
            .expect("seed the base");
        assert_eq!(
            manager
                .get_value(ConfigWhich::Base, "server.port")
                .expect("the key is there"),
            "8080",
            "integers read back as text without decoration"
        );
        assert_eq!(manager.get_value(ConfigWhich::Base, "server.host"), None);
    }
}
