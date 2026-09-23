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
    /// Distinguishes one process lifetime's decisions from another's in a
    /// log they share.
    ///
    /// `sequence` restarts at zero every time a `Capture` is opened, so in
    /// an append-only log that outlives the process -- a supervised agent
    /// that is restarted, a crash loop, two runs on the same day -- rows
    /// from different runs interleave with colliding sequences and cannot
    /// be told apart afterwards. That matters for the fine-tuning use case
    /// specifically: a train/test split has to divide by *run*, because
    /// consecutive decisions within a run are heavily correlated and
    /// splitting by row leaks the test set into training.
    pub run_id: String,
    /// Monotonic within one [`Capture`]'s lifetime -- not a global
    /// sequence, so a caller starting fresh each run doesn't have to
    /// persist a counter anywhere.
    pub sequence: u64,
    pub at_unix_ms: u128,
    /// The caller's own id for this call, when it supplied one, copied from
    /// [`Request::correlation`] and never sent to any endpoint.
    ///
    /// This is the join key between a decision and its consequence. What
    /// followed from acting on the answer is not knowable when this record
    /// is written -- the next state is caused by the action -- so it is
    /// necessarily recorded later and elsewhere. This is what lets the two
    /// be put back together.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub correlation: Option<String>,
    /// The request sent, as JSON -- the fine-tuning input.
    pub request: Value,
    /// The outcome received, as JSON, when the call succeeded -- the
    /// fine-tuning target. Exactly one of `outcome`/`error` is present.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub outcome: Option<Value>,
    /// The call's error, when it failed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Caller-supplied context, constant for the life of this [`Capture`].
    ///
    /// Deliberately untyped: this crate has no business knowing whether a
    /// caller's decisions vary by game, board revision, or tenant. Without
    /// somewhere to put that, a caller either writes one log per
    /// configuration and encodes the context in file paths, or loses it --
    /// and a corpus that cannot say which configuration produced a row
    /// cannot be filtered or stratified later.
    #[serde(skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub labels: std::collections::BTreeMap<String, String>,
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
    run_id: String,
    labels: std::collections::BTreeMap<String, String>,
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
            run_id: new_run_id(),
            labels: std::collections::BTreeMap::new(),
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
            run_id: self.run_id.clone(),
            sequence: self.sequence.fetch_add(1, Ordering::Relaxed),
            at_unix_ms,
            correlation: request.correlation.clone(),
            request: request_json(request),
            outcome: outcome.as_ref().ok().map(outcome_json),
            error: outcome.as_ref().err().map(std::string::ToString::to_string),
            labels: self.labels.clone(),
        }
    }

    /// This capture's run identifier, stamped onto every record it writes.
    #[must_use]
    pub fn run_id(&self) -> &str {
        &self.run_id
    }

    /// Attaches caller context to every record this capture writes from here
    /// on, replacing any previous value for the same key.
    ///
    /// Set these before the first decision: labels are recorded as they
    /// stand when a record is written, so a key added halfway through leaves
    /// the earlier rows without it.
    #[must_use]
    pub fn labelled(
        mut self,
        labels: impl IntoIterator<Item = (impl Into<String>, impl Into<String>)>,
    ) -> Self {
        self.labels
            .extend(labels.into_iter().map(|(k, v)| (k.into(), v.into())));
        self
    }
}

/// A run identifier with no dependency on a UUID crate: process id and the
/// opening instant, which is unique enough to separate runs sharing a log
/// and stable for the life of the capture.
fn new_run_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{:x}-{:x}", std::process::id(), nanos)
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

    /// A correlation must reach the log, because it is the only way a
    /// decision can be rejoined to what followed from it. Nothing else in a
    /// captured record refers to the caller's world.
    #[test]
    fn a_correlation_is_written_and_absent_when_unset() {
        let dir = temp_dir("correlation");
        let _ = std::fs::remove_dir_all(&dir);
        let question = Question::choice("pick", [("a", "first")]);
        let reply = r#"{"answers":{"q":{"type":"choice","choice":"a","confidence":1.0}}}"#;

        let capture = Capture::at(&dir).unwrap();
        let client = CapturingClient::new(
            ScriptedClient::new([reply.to_owned(), reply.to_owned()]),
            capture,
        );
        client
            .decide(
                &Request::single(serde_json::json!({"s": 1}), "q", question.clone())
                    .correlated("call-7"),
            )
            .unwrap();
        client
            .decide(&Request::single(serde_json::json!({"s": 2}), "q", question))
            .unwrap();

        let log = std::fs::read_to_string(dir.join("decisions.jsonl")).unwrap();
        let rows: Vec<serde_json::Value> = log
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(rows[0]["correlation"], "call-7");
        assert!(
            rows[1].get("correlation").is_none(),
            "an uncorrelated call must not invent an id: a wrong join is worse \
             than a missing one"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A correlation is the caller's private bookkeeping. Sending it to a
    /// provider would leak the caller's internal identifiers off-box for no
    /// benefit, so it must stay out of the wire body.
    #[test]
    fn a_correlation_is_never_transmitted() {
        let request = Request::single(serde_json::json!({"s": 1}), "q", Question::choice("pick", [("a", "first")]))
            .correlated("secret-internal-id");
        // The wire body is built field by field from exactly these two
        // pieces; this is the contract that keeps the id local.
        let body = serde_json::json!({
            "model": "m",
            "state": request.observation,
            "questions": request.questions,
        });
        assert!(
            !serde_json::to_string(&body).unwrap().contains("secret-internal-id"),
            "the correlation must not appear anywhere in a transmitted body"
        );
    }

    /// `Client` is implementable from outside the crate, but its return type
    /// was not constructible from outside it -- so a deterministic policy
    /// could not be captured through the same path a model is. These
    /// demonstrations are the supervised half of a corpus; they must not
    /// need a second format.
    #[test]
    fn a_locally_answered_decision_captures_like_any_other() {
        use crate::question::Answer;

        struct Baseline;
        impl Client for Baseline {
            fn decide(&self, _request: &Request) -> Result<Outcome, Error> {
                Ok(Outcome::answered([(
                    "q",
                    Answer::Choice {
                        choice: "a".to_owned(),
                        probabilities: std::collections::BTreeMap::new(),
                        // A deterministic policy is not guessing.
                        confidence: 1.0,
                    },
                )]))
            }
        }

        let dir = temp_dir("local");
        let _ = std::fs::remove_dir_all(&dir);
        let client = CapturingClient::new(Baseline, Capture::at(&dir).unwrap());
        let outcome = client
            .decide(
                &Request::single(
                    serde_json::json!({"s": 1}),
                    "q",
                    Question::choice("pick", [("a", "first")]),
                )
                .correlated("teacher-1"),
            )
            .unwrap();
        assert!(matches!(outcome.answer("q").unwrap(), Answer::Choice { .. }));

        let log = std::fs::read_to_string(dir.join("decisions.jsonl")).unwrap();
        let row: serde_json::Value = serde_json::from_str(log.lines().next().unwrap()).unwrap();
        assert_eq!(row["correlation"], "teacher-1");
        assert_eq!(row.pointer("/outcome/answers/q/choice").unwrap(), "a");
        assert!(
            row.pointer("/request/observation").is_some(),
            "a demonstration is only trainable with the observation it answered"
        );
        assert!(
            row.pointer("/outcome/resolved_model").is_none()
                || row.pointer("/outcome/resolved_model").unwrap().is_null(),
            "there was no endpoint, so no model may be claimed"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Two runs sharing a log must be separable afterwards. `sequence`
    /// alone cannot do it: it restarts at zero each time a capture opens, so
    /// a restarted agent writes colliding sequences into the same file.
    #[test]
    fn records_from_different_runs_are_distinguishable_in_one_log() {
        let dir = temp_dir("runs");
        let _ = std::fs::remove_dir_all(&dir);
        let question = Question::choice("pick", [("a", "first")]);

        let mut ids = Vec::new();
        for _ in 0..2 {
            let capture = Capture::at(&dir).unwrap();
            ids.push(capture.run_id().to_owned());
            let client = CapturingClient::new(
                ScriptedClient::new([
                    r#"{"answers":{"q":{"type":"choice","choice":"a","confidence":1.0}}}"#
                        .to_owned(),
                ]),
                capture,
            );
            let request = Request::single(serde_json::json!({"s": 1}), "q", question.clone());
            client.decide(&request).unwrap();
        }
        assert_ne!(ids[0], ids[1], "each capture must take its own run id");

        let log = std::fs::read_to_string(dir.join("decisions.jsonl")).unwrap();
        let rows: Vec<serde_json::Value> = log
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(rows.len(), 2);
        // Both are sequence 0 -- which is exactly why run_id has to exist.
        assert_eq!(rows[0]["sequence"], 0);
        assert_eq!(rows[1]["sequence"], 0);
        assert_ne!(rows[0]["run_id"], rows[1]["run_id"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Labels let a corpus say which configuration produced a row, without
    /// this crate knowing what a caller's configurations are.
    #[test]
    fn labels_are_written_and_absent_when_unset() {
        let dir = temp_dir("labels");
        let _ = std::fs::remove_dir_all(&dir);
        let question = Question::choice("pick", [("a", "first")]);
        let answer =
            r#"{"answers":{"q":{"type":"choice","choice":"a","confidence":1.0}}}"#.to_owned();

        let capture = Capture::at(&dir).unwrap().labelled([("game", "tetris"), ("policy", "teacher")]);
        let client = CapturingClient::new(ScriptedClient::new([answer.clone()]), capture);
        client
            .decide(&Request::single(serde_json::json!({"s": 1}), "q", question.clone()))
            .unwrap();

        let row: serde_json::Value =
            serde_json::from_str(std::fs::read_to_string(dir.join("decisions.jsonl")).unwrap().lines().next().unwrap())
                .unwrap();
        assert_eq!(row["labels"]["game"], "tetris");
        assert_eq!(row["labels"]["policy"], "teacher");
        let _ = std::fs::remove_dir_all(&dir);

        // Unlabelled captures do not carry an empty object around.
        let dir = temp_dir("nolabels");
        let _ = std::fs::remove_dir_all(&dir);
        let client = CapturingClient::new(ScriptedClient::new([answer]), Capture::at(&dir).unwrap());
        client
            .decide(&Request::single(serde_json::json!({"s": 1}), "q", question))
            .unwrap();
        let row: serde_json::Value =
            serde_json::from_str(std::fs::read_to_string(dir.join("decisions.jsonl")).unwrap().lines().next().unwrap())
                .unwrap();
        assert!(row.get("labels").is_none(), "empty labels should be omitted");
        let _ = std::fs::remove_dir_all(&dir);
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
