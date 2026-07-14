//! Session identity and per-session injection state: which rules have already
//! been emitted, guarded by a directory lock, plus cache hygiene.

use std::env;
use std::fmt::Write as _;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process;
use std::thread;
use std::time::Duration;

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::HookResult;
use crate::pathutil::path_to_string;
use crate::payload::read_field_string;

/// Time spent waiting for a contended session cache lock before failing open.
const STATE_LOCK_RETRIES: usize = 200;
const STATE_LOCK_SLEEP: Duration = Duration::from_millis(5);
/// Age past which a lock directory is assumed leaked (e.g. the hook was
/// SIGKILLed mid-run) and is broken. A healthy hook holds the lock for
/// milliseconds, so one minute is far on the safe side.
const STALE_LOCK_AGE: Duration = Duration::from_secs(60);
/// Age past which an idle session's cache entries are swept. A session resumed
/// after this long merely has its rules injected once more.
pub(crate) const STALE_STATE_AGE: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// Per-session record of which rules have already been injected or warned
/// about.
#[derive(Debug, Default)]
pub(crate) struct HookState {
    pub(crate) injected_rules: Vec<String>,
    pub(crate) warned_rules: Vec<String>,
}

/// Directory-backed lock guarding one session cache file.
#[derive(Debug)]
pub(crate) struct StateLock {
    path: PathBuf,
}

impl Drop for StateLock {
    fn drop(&mut self) {
        if let Err(error) = fs::remove_dir(&self.path)
            && error.kind() != io::ErrorKind::NotFound
        {
            eprintln!(
                "[path-rules] failed to remove cache state lock {}: {error}",
                path_to_string(&self.path)
            );
        }
    }
}

/// Resolve a stable, filesystem-safe session identifier.
///
/// Prefers the payload's session id, then the `CODEX_SESSION_ID` environment
/// variable, and finally derives a per-parent-process, per-directory fallback
/// so independent sessions in the same directory do not share state.
pub(crate) fn read_session_id(input: &Value, cwd: &Path) -> String {
    let id = read_field_string(input, &["session_id", "sessionId", "thread_id", "threadId"])
        .map(ToOwned::to_owned)
        .or_else(|| {
            env::var("CODEX_SESSION_ID")
                .ok()
                .filter(|value| !value.trim().is_empty())
        })
        .unwrap_or_else(|| {
            format!(
                "unknown-{}-{}",
                parent_process_id(),
                &hash_text(&path_to_string(cwd))[..8]
            )
        });

    sanitize_file_name(&id)
}

/// Root directory of the session cache.
///
/// Taken from `cache_root` (used by tests), else `CODEX_PATH_RULES_CACHE`,
/// else `XDG_CACHE_HOME`, else `~/.cache`.
fn cache_root_dir(cache_root: Option<&Path>) -> PathBuf {
    cache_root
        .map(PathBuf::from)
        .or_else(|| env::var_os("CODEX_PATH_RULES_CACHE").map(PathBuf::from))
        .or_else(|| {
            env::var_os("XDG_CACHE_HOME").map(|value| PathBuf::from(value).join("codex-path-rules"))
        })
        .unwrap_or_else(|| home_dir().join(".cache").join("codex-path-rules"))
}

/// Resolve the path of the per-session state file.
///
/// Entries are namespaced by a hash of the working directory so different
/// repositories never collide.
fn cache_file(cwd: &Path, session_id: &str, cache_root: Option<&Path>) -> PathBuf {
    cache_root_dir(cache_root)
        .join(&hash_text(&path_to_string(cwd))[..16])
        .join(format!("{session_id}.json"))
}

/// Resolve the directory used as the lock for a session state file.
fn state_lock_dir(cwd: &Path, session_id: &str, cache_root: Option<&Path>) -> PathBuf {
    cache_file(cwd, session_id, cache_root).with_extension("json.lock")
}

/// Acquire the per-session cache lock with the production stale-lock age.
pub(crate) fn acquire_state_lock(
    cwd: &Path,
    session_id: &str,
    cache_root: Option<&Path>,
) -> HookResult<StateLock> {
    acquire_state_lock_with_stale_age(cwd, session_id, cache_root, STALE_LOCK_AGE)
}

/// Acquire the per-session cache lock.
///
/// Uses `create_dir` as the atomic operation, keeping the implementation in the
/// standard library. A lock directory older than `stale_age` is assumed leaked
/// (e.g. after SIGKILL), removed, and acquisition retried.
///
/// # Errors
///
/// Returns an error if the lock parent cannot be created, the lock cannot be
/// created, or another process keeps the lock too long.
fn acquire_state_lock_with_stale_age(
    cwd: &Path,
    session_id: &str,
    cache_root: Option<&Path>,
    stale_age: Duration,
) -> HookResult<StateLock> {
    let path = state_lock_dir(cwd, session_id, cache_root);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("failed to create cache lock directory: {error}"))?;
    }

    for _ in 0..STATE_LOCK_RETRIES {
        match fs::create_dir(&path) {
            Ok(()) => return Ok(StateLock { path }),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                if !remove_stale_lock(&path, stale_age) {
                    thread::sleep(STATE_LOCK_SLEEP);
                }
            }
            Err(error) => {
                return Err(format!(
                    "failed to create cache state lock {}: {error}",
                    path_to_string(&path)
                ));
            }
        }
    }

    Err(format!(
        "timed out acquiring cache state lock {}",
        path_to_string(&path)
    ))
}

/// Remove a lock directory whose mtime is at least `max_age` old, returning
/// whether the caller should immediately retry acquisition.
///
/// Two contenders may race to remove the same stale lock; the loser sees
/// `NotFound`, which counts as success because the lock is gone either way.
fn remove_stale_lock(path: &Path, max_age: Duration) -> bool {
    if !is_older_than(path, max_age) {
        return false;
    }

    match fs::remove_dir(path) {
        Ok(()) => true,
        Err(error) if error.kind() == io::ErrorKind::NotFound => true,
        Err(error) => {
            eprintln!(
                "[path-rules] failed to remove stale cache lock {}: {error}",
                path_to_string(path)
            );
            false
        }
    }
}

/// True when `path`'s modification time is at least `max_age` in the past.
/// Unreadable metadata counts as fresh — the safe direction for cleanup.
fn is_older_than(path: &Path, max_age: Duration) -> bool {
    fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .ok()
        .and_then(|modified| modified.elapsed().ok())
        .is_some_and(|age| age >= max_age)
}

/// Load the per-session state, returning a fresh default when no cache file
/// exists yet.
///
/// # Errors
///
/// A missing cache file is the expected first-run case and yields a default
/// state. Any other IO error, or a corrupt (non-JSON) cache file, is surfaced
/// as an error rather than silently reset, in line with the repository's
/// fail-fast policy.
pub(crate) fn read_state(
    cwd: &Path,
    session_id: &str,
    cache_root: Option<&Path>,
) -> HookResult<HookState> {
    let file = cache_file(cwd, session_id, cache_root);
    let text = match fs::read_to_string(&file) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(HookState::default()),
        Err(error) => {
            return Err(format!(
                "failed to read cache state {}: {error}",
                path_to_string(&file)
            ));
        }
    };

    let parsed: Value = serde_json::from_str(&text).map_err(|error| {
        format!(
            "failed to parse cache state {}: {error}",
            path_to_string(&file)
        )
    })?;

    let injected_rules = read_string_array(&parsed, "injectedRules");
    let warned_rules = read_string_array(&parsed, "warnedRules");

    Ok(HookState {
        injected_rules,
        warned_rules,
    })
}

/// Read one optional array of strings from the cache state.
fn read_string_array(parsed: &Value, field: &str) -> Vec<String> {
    parsed
        .get(field)
        .and_then(Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(Value::as_str)
                .map(ToOwned::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

/// Persist the per-session state, creating the cache directory if needed.
///
/// # Errors
///
/// Returns an error if the cache directory cannot be created or the file
/// cannot be written.
pub(crate) fn write_state(
    cwd: &Path,
    session_id: &str,
    cache_root: Option<&Path>,
    state: &HookState,
) -> HookResult<()> {
    let file = cache_file(cwd, session_id, cache_root);
    if let Some(parent) = file.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("failed to create cache directory: {error}"))?;
    }

    let output = json!({
        "injectedRules": state.injected_rules,
        "warnedRules": state.warned_rules,
    });
    let temporary = file.with_extension(format!("json.tmp-{}", process::id()));
    fs::write(&temporary, format!("{output}\n")).map_err(|error| {
        format!(
            "failed to write temporary cache state {}: {error}",
            path_to_string(&temporary)
        )
    })?;
    fs::rename(&temporary, &file).map_err(|error| {
        format!(
            "failed to replace cache state {}: {error}",
            path_to_string(&file)
        )
    })
}

/// Delete the per-session state file so rules are re-injected from scratch.
///
/// A missing file is treated as success.
///
/// # Errors
///
/// Returns an error if the file exists but cannot be removed.
pub(crate) fn reset_state(
    cwd: &Path,
    session_id: &str,
    cache_root: Option<&Path>,
) -> HookResult<()> {
    match fs::remove_file(cache_file(cwd, session_id, cache_root)) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("failed to reset cache state: {error}")),
    }
}

/// Best-effort cache hygiene, run on reset events: remove session state files
/// and lock directories older than `max_age` under the cache root, then drop
/// namespace directories left empty.
///
/// Best-effort by design: concurrent sessions race with the sweep, so per-entry
/// failures are reported on stderr but never fail the hook. Removing an idle
/// session's state only means its rules are injected once more if that session
/// ever resumes.
pub(crate) fn sweep_stale_sessions(cache_root: Option<&Path>, max_age: Duration) {
    let root = cache_root_dir(cache_root);
    let Ok(namespaces) = fs::read_dir(&root) else {
        return;
    };

    for namespace in namespaces.flatten() {
        let directory = namespace.path();
        if !directory.is_dir() {
            continue;
        }

        let Ok(entries) = fs::read_dir(&directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !is_older_than(&path, max_age) {
                continue;
            }

            let removed = if path.is_dir() {
                fs::remove_dir_all(&path)
            } else {
                fs::remove_file(&path)
            };
            if let Err(error) = removed
                && error.kind() != io::ErrorKind::NotFound
            {
                eprintln!(
                    "[path-rules] failed to sweep stale cache entry {}: {error}",
                    path_to_string(&path)
                );
            }
        }

        // Expected to fail while the namespace still holds live sessions.
        let _ = fs::remove_dir(&directory);
    }
}

/// Lowercase hex SHA-256 of `value`, used to derive collision-resistant cache
/// path components.
fn hash_text(value: &str) -> String {
    let digest = Sha256::digest(value.as_bytes());
    let mut output = String::with_capacity(digest.len() * 2);
    for byte in digest {
        let _ = write!(&mut output, "{byte:02x}");
    }
    output
}

/// Replace any character other than alphanumerics, `.`, `_`, or `-` with `_`, so
/// a session identifier is safe to embed in a file name.
fn sanitize_file_name(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '-') {
                character
            } else {
                '_'
            }
        })
        .collect()
}

/// The user's home directory from `HOME`, falling back to `.` when unset.
fn home_dir() -> PathBuf {
    env::var_os("HOME").map_or_else(|| PathBuf::from("."), PathBuf::from)
}

/// Parent process id, used only to disambiguate the fallback session
/// identifier when neither the payload nor the environment carries one.
///
/// Off Unix there is no portable way to read it from `std`, so the current
/// process id is used; the resulting identifier then changes per invocation,
/// which only weakens de-duplication in that already-degraded fallback case.
fn parent_process_id() -> u32 {
    #[cfg(unix)]
    {
        std::os::unix::process::parent_id()
    }
    #[cfg(not(unix))]
    {
        process::id()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::selftest::create_temp_dir;

    #[test]
    fn read_state_returns_default_when_the_cache_is_missing() {
        let root = create_temp_dir("path-rules-test").expect("temp dir");
        let state =
            read_state(Path::new("/repo"), "sess", Some(&root)).expect("missing cache is ok");
        let _ = fs::remove_dir_all(&root);
        assert!(state.injected_rules.is_empty() && state.warned_rules.is_empty());
    }

    #[test]
    fn read_state_errors_when_the_cache_is_corrupt() {
        let root = create_temp_dir("path-rules-test").expect("temp dir");
        let file = cache_file(Path::new("/repo"), "sess", Some(&root));
        fs::create_dir_all(file.parent().expect("cache parent")).expect("create cache dir");
        fs::write(&file, "not json").expect("write corrupt cache");
        let error = read_state(Path::new("/repo"), "sess", Some(&root)).unwrap_err();
        let _ = fs::remove_dir_all(&root);
        assert!(
            error.contains("failed to parse cache state"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn state_round_trips_through_write_then_read() {
        let root = create_temp_dir("path-rules-test").expect("temp dir");
        let written = HookState {
            injected_rules: vec!["a".to_owned(), "b".to_owned()],
            warned_rules: vec!["invalid".to_owned()],
        };
        write_state(Path::new("/repo"), "sess", Some(&root), &written).expect("write state");
        let loaded = read_state(Path::new("/repo"), "sess", Some(&root)).expect("read state");
        let _ = fs::remove_dir_all(&root);
        assert_eq!(
            (loaded.injected_rules, loaded.warned_rules),
            (
                vec!["a".to_owned(), "b".to_owned()],
                vec!["invalid".to_owned()]
            )
        );
    }

    #[test]
    fn read_state_defaults_warned_rules_for_an_older_cache() {
        let root = create_temp_dir("path-rules-test").expect("temp dir");
        let file = cache_file(Path::new("/repo"), "sess", Some(&root));
        fs::create_dir_all(file.parent().expect("cache parent")).expect("create cache dir");
        fs::write(&file, r#"{"injectedRules":["a"]}"#).expect("write older cache");

        let loaded = read_state(Path::new("/repo"), "sess", Some(&root)).expect("read state");
        let _ = fs::remove_dir_all(&root);

        assert_eq!(
            (loaded.injected_rules, loaded.warned_rules),
            (vec!["a".to_owned()], Vec::<String>::new())
        );
    }

    #[test]
    fn state_lock_removes_the_lock_dir_when_dropped() {
        let root = create_temp_dir("path-rules-test").expect("temp dir");
        let lock_dir = state_lock_dir(Path::new("/repo"), "sess", Some(&root));

        {
            let _lock =
                acquire_state_lock(Path::new("/repo"), "sess", Some(&root)).expect("lock state");
            assert!(lock_dir.is_dir());
        }

        let exists = lock_dir.exists();
        let _ = fs::remove_dir_all(&root);
        assert!(!exists);
    }

    #[test]
    fn acquire_breaks_a_lock_dir_older_than_the_stale_age() {
        let root = create_temp_dir("path-rules-test").expect("temp dir");
        let lock_dir = state_lock_dir(Path::new("/repo"), "sess", Some(&root));
        fs::create_dir_all(&lock_dir).expect("pre-create leaked lock");

        let lock = acquire_state_lock_with_stale_age(
            Path::new("/repo"),
            "sess",
            Some(&root),
            Duration::ZERO,
        );

        let acquired = lock.is_ok();
        drop(lock);
        let _ = fs::remove_dir_all(&root);
        assert!(acquired, "a stale lock should be broken and re-acquired");
    }

    #[test]
    fn remove_stale_lock_keeps_a_fresh_lock_dir() {
        let root = create_temp_dir("path-rules-test").expect("temp dir");
        let lock_dir = root.join("sess.json.lock");
        fs::create_dir(&lock_dir).expect("create lock");

        let removed = remove_stale_lock(&lock_dir, Duration::from_secs(3600));

        let exists = lock_dir.exists();
        let _ = fs::remove_dir_all(&root);
        assert!(!removed);
        assert!(exists);
    }

    #[test]
    fn sweep_removes_stale_state_files_and_lock_dirs() {
        let root = create_temp_dir("path-rules-test").expect("temp dir");
        let namespace = root.join("namespace");
        fs::create_dir_all(namespace.join("old.json.lock")).expect("create stale lock");
        fs::write(namespace.join("old.json"), "{}").expect("write stale state");

        sweep_stale_sessions(Some(&root), Duration::ZERO);

        let namespace_gone = !namespace.exists();
        let _ = fs::remove_dir_all(&root);
        assert!(
            namespace_gone,
            "stale entries and empty namespace should go"
        );
    }

    #[test]
    fn sweep_keeps_fresh_state_files() {
        let root = create_temp_dir("path-rules-test").expect("temp dir");
        let namespace = root.join("namespace");
        fs::create_dir_all(&namespace).expect("create namespace");
        fs::write(namespace.join("live.json"), "{}").expect("write live state");

        sweep_stale_sessions(Some(&root), Duration::from_secs(3600));

        let survived = namespace.join("live.json").exists();
        let _ = fs::remove_dir_all(&root);
        assert!(survived);
    }
}
