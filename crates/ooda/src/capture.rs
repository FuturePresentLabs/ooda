//! Durable local capture of every [`Client::decide`] call, for fine-tuning
//! later.
//!
//! Feature-gated (`capture`) and fully opt-in: nothing here runs unless a
//! caller explicitly wraps their [`Client`] in a [`CapturingClient`].
//! Adapted from `speedy`'s own already-production `rlcd.decision.v1` event
//! log -- independently built there because `ooda` had no equivalent yet,
//! exactly the kind of duplication this crate exists to end. Narrowed to
//! one combined record per call (request paired with its outcome-or-error)
//! rather than `speedy`'s split requested/completed/failed phases: a
//! fine-tuning example needs a request paired with its real answer, not a
//! partial "asked but never got one" record with nothing to train on.
//!
//! The append-only-log-with-rollover discipline is kept intact, carried
//! forward from a real incident: a `speedy` run once reached 106 MB on a
//! volume that was already 99% full, and a full volume is what breaks a
//! process, not the log itself.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;
use serde_json::Value;

use crate::client::{Client, Outcome, Request};
use crate::error::Error;

/// Cap for one capture log before it rolls over. One previous generation is
/// kept, so a run's captured history costs at most twice this.
pub const MAX_CAPTURE_LOG_BYTES: u64 = 64 * 1024 * 1024;

/// One captured [`Client::decide`] call, as written to the log.
#[derive(Debug, Clone, Serialize)]
pub struct CapturedDecision {
    pub schema_version: u32,
    /// Monotonic within one [`Capture`]'s lifetime -- not a global
    /// sequence, so a caller starting fresh each run doesn't have to
    /// persist a counter anywhere.
    pub sequence: u64,
    pub at_unix_ms: u128,
    /// The request sent, as JSON -- the fine-tuning input.
    pub request: Value,
    /// The outcome received, as JSON, when the call succeeded -- the
    /// fine-tuning target. Exactly one of `outcome`/`error` is present.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub outcome: Option<Value>,
    /// The call's error, when it failed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

fn request_json(request: &Request) -> Value {
    serde_json::json!({
        "observation": request.observation,
        "questions": request.questions,
    })
}

fn outcome_json(outcome: &Outcome) -> Value {
    serde_json::json!({
        "answers": outcome.answers(),
        "usage": outcome.usage,
        "resolved_model": outcome.resolved_model,
        "elapsed_ms": outcome.elapsed.map(|d| d.as_millis()),
        "retries": outcome.retries,
    })
}

/// An append-only, rollover-capped JSONL log of [`CapturedDecision`]s.
pub struct Capture {
    file: Mutex<std::fs::File>,
    path: PathBuf,
    sequence: AtomicU64,
}

impl Capture {
    /// Opens (creating the directory and file if absent) a capture log at
    /// `.ooda/<binary-name>/decisions.jsonl`, where `<binary-name>` is
    /// inferred from [`std::env::current_exe`] -- no caller-supplied name
    /// to keep in sync with the binary actually running.
    ///
    /// # Errors
    /// The current executable's path couldn't be read, or the log
    /// couldn't be created/opened.
    pub fn for_current_binary() -> Result<Self, Error> {
        let exe = std::env::current_exe().map_err(|source| Error::Capture {
            path: "<current_exe>".to_owned(),
            source,
        })?;
        let name = exe
            .file_stem()
            .map(|stem| stem.to_string_lossy().into_owned())
            .unwrap_or_else(|| "unknown".to_owned());
        Self::at(Path::new(".ooda").join(name))
    }

    /// Opens (creating if absent) a capture log at `dir/decisions.jsonl`.
    /// Prefer [`Capture::for_current_binary`] unless a caller genuinely
    /// needs to override the location.
    ///
    /// # Errors
    /// The directory or log file couldn't be created/opened.
    pub fn at(dir: impl AsRef<Path>) -> Result<Self, Error> {
        let dir = dir.as_ref();
        std::fs::create_dir_all(dir).map_err(|source| Error::Capture {
            path: dir.display().to_string(),
            source,
        })?;
        let path = dir.join("decisions.jsonl");
        let file = open_log(&path)?;
        Ok(Self {
            file: Mutex::new(file),
            path,
            sequence: AtomicU64::new(0),
        })
    }

    /// Appends one record, locking against concurrent writers from another
    /// process sharing this path, then rolls the log over if it's now past
    /// [`MAX_CAPTURE_LOG_BYTES`].
    ///
    /// # Errors
    /// The lock, write, or rollover failed.
    pub fn append(&self, record: &CapturedDecision) -> Result<(), Error> {
        let mut file = self.file.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        lock_exclusive(&file, &self.path)?;
        let result = (|| -> Result<(), Error> {
            serde_json::to_writer(&mut *file, record).map_err(|source| Error::Capture {
                path: self.path.display().to_string(),
                source: std::io::Error::other(source),
            })?;
            file.write_all(b"\n").map_err(|source| Error::Capture {
                path: self.path.display().to_string(),
                source,
            })?;
            file.flush().map_err(|source| Error::Capture {
                path: self.path.display().to_string(),
                source,
            })
        })();
        unlock(&file, &self.path)?;
        result?;
        self.roll_over_if_full(&mut file)
    }

    fn roll_over_if_full(&self, file: &mut std::fs::File) -> Result<(), Error> {
        let size = file
            .metadata()
            .map_err(|source| Error::Capture {
                path: self.path.display().to_string(),
                source,
            })?
            .len();
        if size <= MAX_CAPTURE_LOG_BYTES {
            return Ok(());
        }
        let rolled = rolled_path(&self.path);
        std::fs::rename(&self.path, &rolled).map_err(|source| Error::Capture {
            path: self.path.display().to_string(),
            source,
        })?;
        *file = open_log(&self.path)?;
        Ok(())
    }

    fn next_record(&self, request: &Request, outcome: &Result<Outcome, Error>) -> CapturedDecision {
        let at_unix_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        CapturedDecision {
            schema_version: 1,
            sequence: self.sequence.fetch_add(1, Ordering::Relaxed),
            at_unix_ms,
            request: request_json(request),
            outcome: outcome.as_ref().ok().map(outcome_json),
            error: outcome.as_ref().err().map(std::string::ToString::to_string),
        }
    }
}

/// Where a rolled-over log is kept. A caller reading whole files (the
/// fine-tuning use case) wants a plain previous generation, not a
/// compressed one it has to decompress first.
fn rolled_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".1");
    path.with_file_name(name)
}

fn open_log(path: &Path) -> Result<std::fs::File, Error> {
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|source| Error::Capture {
            path: path.display().to_string(),
            source,
        })
}

#[cfg(unix)]
fn lock_exclusive(file: &std::fs::File, path: &Path) -> Result<(), Error> {
    use std::os::fd::AsRawFd;
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } == 0 {
        Ok(())
    } else {
        Err(Error::Capture {
            path: path.display().to_string(),
            source: std::io::Error::last_os_error(),
        })
    }
}

#[cfg(unix)]
fn unlock(file: &std::fs::File, path: &Path) -> Result<(), Error> {
    use std::os::fd::AsRawFd;
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_UN) } == 0 {
        Ok(())
    } else {
        Err(Error::Capture {
            path: path.display().to_string(),
            source: std::io::Error::last_os_error(),
        })
    }
}

#[cfg(not(unix))]
fn lock_exclusive(_file: &std::fs::File, _path: &Path) -> Result<(), Error> {
    Ok(())
}

#[cfg(not(unix))]
fn unlock(_file: &std::fs::File, _path: &Path) -> Result<(), Error> {
    Ok(())
}

/// Wraps any [`Client`], durably appending every [`Client::decide`] call
/// (request paired with its outcome or error) to a [`Capture`] log before
/// returning the result unchanged.
///
/// A capture-write failure is itself a hard error when the underlying
/// decision *succeeded* -- a caller must not be left believing a decision
/// was durably logged when it wasn't. When the underlying decision
/// *failed*, that original error is what's returned; a secondary capture
/// failure on top of it is not more informative than the real one.
pub struct CapturingClient<C> {
    inner: C,
    capture: Capture,
}

impl<C> CapturingClient<C> {
    /// Wraps `inner`, capturing to `capture`.
    #[must_use]
    pub fn new(inner: C, capture: Capture) -> Self {
        Self { inner, capture }
    }
}

impl<C: Client> Client for CapturingClient<C> {
    fn decide(&self, request: &Request) -> Result<Outcome, Error> {
        let result = self.inner.decide(request);
        let record = self.capture.next_record(request, &result);
        match (self.capture.append(&record), result) {
            (Ok(()), result) => result,
            (Err(capture_error), Ok(_)) => Err(capture_error),
            (Err(_), Err(decide_error)) => Err(decide_error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::question::Question;
    use crate::scripted::ScriptedClient;

    fn temp_dir(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!("ooda-capture-test-{label}-{}", std::process::id()))
    }

    #[test]
    fn a_successful_call_is_captured_with_its_outcome() {
        let dir = temp_dir("success");
        let capture = Capture::at(&dir).unwrap();
        let client = CapturingClient::new(
            ScriptedClient::new([
                r#"{"answers":{"q":{"type":"choice","choice":"a","confidence":0.9}}}"#.to_owned(),
            ]),
            capture,
        );
        let request = Request::single(
            serde_json::json!({"x": 1}),
            "q",
            Question::choice("pick", [("a", "desc")]),
        );
        client.decide(&request).unwrap();

        let logged = std::fs::read_to_string(dir.join("decisions.jsonl")).unwrap();
        let record: Value = serde_json::from_str(logged.trim()).unwrap();
        assert_eq!(record["request"]["observation"]["x"], 1);
        assert_eq!(record["outcome"]["answers"]["q"]["choice"], "a");
        assert!(record["error"].is_null());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_failed_call_is_captured_with_its_error_and_still_returns_it() {
        let dir = temp_dir("failure");
        let capture = Capture::at(&dir).unwrap();
        let client = CapturingClient::new(ScriptedClient::new(Vec::<String>::new()), capture);
        let request = Request::single(
            serde_json::json!({}),
            "q",
            Question::choice("pick", [("a", "desc")]),
        );
        let result = client.decide(&request);
        assert!(result.is_err());

        let logged = std::fs::read_to_string(dir.join("decisions.jsonl")).unwrap();
        let record: Value = serde_json::from_str(logged.trim()).unwrap();
        assert!(record["outcome"].is_null());
        assert!(record["error"].as_str().is_some());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn sequence_increments_across_calls_and_both_land_in_the_same_log() {
        let dir = temp_dir("sequence");
        let capture = Capture::at(&dir).unwrap();
        let client = CapturingClient::new(
            ScriptedClient::new([
                r#"{"answers":{"q":{"type":"choice","choice":"a","confidence":0.9}}}"#.to_owned(),
                r#"{"answers":{"q":{"type":"choice","choice":"a","confidence":0.9}}}"#.to_owned(),
            ]),
            capture,
        );
        let request = Request::single(
            serde_json::json!({}),
            "q",
            Question::choice("pick", [("a", "desc")]),
        );
        client.decide(&request).unwrap();
        client.decide(&request).unwrap();

        let logged = std::fs::read_to_string(dir.join("decisions.jsonl")).unwrap();
        let lines: Vec<&str> = logged.lines().collect();
        assert_eq!(lines.len(), 2);
        let first: Value = serde_json::from_str(lines[0]).unwrap();
        let second: Value = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(first["sequence"], 0);
        assert_eq!(second["sequence"], 1);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn for_current_binary_infers_a_name_from_the_running_executable() {
        // Not asserting the exact name (it's the test harness binary's own,
        // whatever cargo happens to call it) -- just that it resolves to a
        // real, writable `.ooda/<something>` directory without a caller
        // having to name it. `.ooda/` is gitignored for exactly this
        // reason: any consumer's own test suite, including this one,
        // creates it in the current directory.
        let capture = Capture::for_current_binary().unwrap();
        assert!(capture.path.starts_with(".ooda"));
        let dir = capture.path.parent().unwrap().to_path_buf();
        drop(capture);
        std::fs::remove_dir_all(&dir).ok();
    }
}
