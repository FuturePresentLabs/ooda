//! Speech-to-text: audio in, a timed transcript out — `ooda`'s capability
//! against Bifrost's OpenAI-compatible `/v1/audio/transcriptions` route.
//!
//! Unlike [`crate::embed`], which ships audio base64-encoded *inside* a JSON
//! body (that route is JSON), the transcription route is **multipart** and is
//! forwarded to the backend byte-for-byte — Bifrost deliberately does not
//! rewrite aliases here, so the `model` form field must name a model the
//! backend itself accepts.
//!
//! Kept a separate trait from [`crate::Client`]/[`crate::Complete`]/[`crate::Embed`]:
//! the wire shapes and response types are unrelated, and a caller that only
//! decides should never need to know this exists.

use std::collections::VecDeque;
use std::sync::Mutex;

use serde::Deserialize;
use serde_json::Value;

use crate::error::Error;
use crate::http::HttpClient;

/// Bifrost's transcription route, appended to a client's base URL.
pub const TRANSCRIPTIONS_PATH: &str = "/v1/audio/transcriptions";

/// One timed word.
#[derive(Clone, Debug, PartialEq)]
pub struct Word {
    pub word: String,
    pub start: f64,
    pub end: f64,
}

/// One timed segment — usually a lyric line, which is the natural unit a
/// karaoke display advances on.
#[derive(Clone, Debug, PartialEq)]
pub struct Segment {
    pub text: String,
    pub start: f64,
    pub end: f64,
    /// Word-level timings, when the backend provided them.
    pub words: Vec<Word>,
}

/// A transcript: the whole text, its timed segments, and what the endpoint
/// reported about how it was produced.
///
/// `segments` and `words` are independent: a line-oriented backend fills
/// `segments` (each with its own `words`); a word-oriented one (`fpl/stt`'s
/// Nemotron backend, observed live: `segments: []`, `words: N`) fills only the
/// top-level `words`. Both are surfaced — a caller that needs lines can group
/// `words` itself, and one that only wants words never has them dropped.
#[derive(Clone, Debug, PartialEq)]
pub struct Transcript {
    pub text: String,
    pub language: Option<String>,
    pub duration: Option<f64>,
    pub segments: Vec<Segment>,
    /// Every timed word, flattened from segments when the backend did not send
    /// them at the top level.
    pub words: Vec<Word>,
    pub model: Option<String>,
}

/// A transcription request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SttRequest {
    /// Raw audio bytes.
    pub audio: Vec<u8>,
    /// Filename sent in the multipart part; the backend infers the container
    /// from its extension.
    pub filename: String,
    /// Optional language hint (e.g. `"en"`).
    pub language: Option<String>,
    /// Ask for word-level timestamps as well as segments.
    pub word_timestamps: bool,
}

impl SttRequest {
    /// A request for WAV audio, asking for word timestamps.
    #[must_use]
    pub fn wav(audio: impl Into<Vec<u8>>) -> Self {
        Self {
            audio: audio.into(),
            filename: "audio.wav".to_owned(),
            language: None,
            word_timestamps: true,
        }
    }

    #[must_use]
    pub fn with_filename(mut self, filename: impl Into<String>) -> Self {
        self.filename = filename.into();
        self
    }

    #[must_use]
    pub fn with_language(mut self, language: impl Into<String>) -> Self {
        self.language = Some(language.into());
        self
    }

    #[must_use]
    pub fn without_word_timestamps(mut self) -> Self {
        self.word_timestamps = false;
        self
    }
}

/// Anything that can transcribe audio.
///
/// A separate trait from the decision/complete/embed capabilities, for the
/// same reason those are separate from each other: unrelated wire shapes and
/// unrelated response types.
pub trait Stt {
    /// # Errors
    /// See [`Error`].
    fn transcribe(&self, request: &SttRequest) -> Result<Transcript, Error>;
}

impl Stt for HttpClient {
    fn transcribe(&self, request: &SttRequest) -> Result<Transcript, Error> {
        let (response, resolved_model, ..) =
            self.send_request(&self.url(TRANSCRIPTIONS_PATH), |builder| {
                let part = reqwest::blocking::multipart::Part::bytes(request.audio.clone())
                    .file_name(request.filename.clone());
                let mut form = reqwest::blocking::multipart::Form::new()
                    .text("model", self.model().to_owned())
                    .text("response_format", "verbose_json")
                    .part("file", part);
                if let Some(language) = &request.language {
                    form = form.text("language", language.clone());
                }
                if request.word_timestamps {
                    form = form.text("timestamp_granularities[]", "word");
                }
                builder.multipart(form)
            })?;

        let body = response
            .text()
            .map_err(|e| Error::Transport(e.to_string()))?;
        let value: Value = serde_json::from_str(&body).map_err(|source| Error::Decode {
            source,
            body: crate::error::truncate(&body),
        })?;
        decode_transcript(&value, resolved_model)
    }
}

#[derive(Deserialize)]
struct RawTranscript {
    #[serde(default)]
    text: String,
    #[serde(default)]
    language: Option<String>,
    #[serde(default)]
    duration: Option<f64>,
    #[serde(default)]
    segments: Vec<RawSegment>,
    /// Some backends put all word timings at the top level instead of nesting
    /// them in segments; both are accepted.
    #[serde(default)]
    words: Vec<RawWord>,
    #[serde(default)]
    model: Option<String>,
}

#[derive(Deserialize)]
struct RawSegment {
    #[serde(default)]
    text: String,
    #[serde(default)]
    start: f64,
    #[serde(default)]
    end: f64,
    #[serde(default)]
    words: Vec<RawWord>,
}

#[derive(Deserialize)]
struct RawWord {
    #[serde(default, alias = "word")]
    text: String,
    #[serde(default)]
    start: f64,
    #[serde(default)]
    end: f64,
}

impl RawWord {
    fn to_word(&self) -> Word {
        Word {
            word: self.text.clone(),
            start: self.start,
            end: self.end,
        }
    }
}

/// Decodes a full transcription response — the shared core of
/// [`HttpClient::transcribe`] and [`ScriptedStt::transcribe`], so the two can
/// never drift on how a response is read.
pub(crate) fn decode_transcript(
    value: &Value,
    resolved_model: Option<String>,
) -> Result<Transcript, Error> {
    if let Some(message) = crate::http::extract_error_message(value) {
        return Err(Error::Api(message));
    }
    let raw: RawTranscript =
        serde_json::from_value(value.clone()).map_err(|source| Error::Decode {
            source,
            body: crate::error::truncate(&value.to_string()),
        })?;
    if raw.text.trim().is_empty() && raw.segments.is_empty() {
        return Err(Error::EmptyTranscript);
    }
    let top_words: Vec<Word> = raw.words.iter().map(RawWord::to_word).collect();
    let segments: Vec<Segment> = raw
        .segments
        .into_iter()
        .map(|segment| {
            let words = if segment.words.is_empty() {
                top_words
                    .iter()
                    .filter(|word| word.start >= segment.start - 1e-6 && word.end <= segment.end + 1e-6)
                    .cloned()
                    .collect()
            } else {
                segment.words.iter().map(RawWord::to_word).collect()
            };
            Segment {
                text: segment.text,
                start: segment.start,
                end: segment.end,
                words,
            }
        })
        .collect();
    // Prefer the backend's own top-level words; otherwise flatten the segment
    // words so a word-oriented consumer still gets everything.
    let words = if top_words.is_empty() {
        segments.iter().flat_map(|s| s.words.iter().cloned()).collect()
    } else {
        top_words
    };
    Ok(Transcript {
        text: raw.text,
        language: raw.language,
        duration: raw.duration,
        segments,
        words,
        model: resolved_model.or(raw.model),
    })
}

/// Answers [`Stt::transcribe`] from a queue of canned response bodies, in
/// order — [`crate::ScriptedClient`]'s counterpart for [`Stt`].
#[derive(Debug, Default)]
pub struct ScriptedStt {
    bodies: Mutex<VecDeque<String>>,
}

impl ScriptedStt {
    /// Queues response bodies, answered in order.
    #[must_use]
    pub fn new<S: Into<String>>(bodies: impl IntoIterator<Item = S>) -> Self {
        Self {
            bodies: Mutex::new(bodies.into_iter().map(Into::into).collect()),
        }
    }
}

impl Stt for ScriptedStt {
    fn transcribe(&self, _request: &SttRequest) -> Result<Transcript, Error> {
        let body = self
            .bodies
            .lock()
            .expect("response queue poisoned")
            .pop_front()
            .ok_or_else(|| Error::Transport("ScriptedStt ran out of responses".to_owned()))?;
        let value: Value = serde_json::from_str(&body).map_err(|source| Error::Decode {
            source,
            body: crate::error::truncate(&body),
        })?;
        decode_transcript(&value, None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_segments_and_nested_words() {
        let body = r#"{
            "text": "hello world",
            "language": "english",
            "duration": 2.5,
            "segments": [
                {"text": "hello world", "start": 0.0, "end": 2.5,
                 "words": [{"word": "hello", "start": 0.0, "end": 1.0},
                           {"word": "world", "start": 1.0, "end": 2.5}]}
            ]
        }"#;
        let value: Value = serde_json::from_str(body).unwrap();
        let transcript = decode_transcript(&value, None).unwrap();
        assert_eq!(transcript.segments.len(), 1);
        assert_eq!(transcript.segments[0].text, "hello world");
        assert_eq!(transcript.segments[0].words.len(), 2);
        assert_eq!(transcript.segments[0].words[1].word, "world");
        assert_eq!(transcript.language.as_deref(), Some("english"));
    }

    #[test]
    fn attaches_top_level_words_to_their_segment() {
        let body = r#"{
            "text": "a b",
            "segments": [{"text": "a", "start": 0.0, "end": 1.0},
                         {"text": "b", "start": 1.0, "end": 2.0}],
            "words": [{"word": "a", "start": 0.0, "end": 1.0},
                      {"word": "b", "start": 1.0, "end": 2.0}]
        }"#;
        let value: Value = serde_json::from_str(body).unwrap();
        let transcript = decode_transcript(&value, None).unwrap();
        assert_eq!(transcript.segments[0].words.len(), 1);
        assert_eq!(transcript.segments[1].words[0].word, "b");
    }

    #[test]
    fn keeps_top_level_words_when_there_are_no_segments() {
        // Exactly the live `fpl/stt` (Nemotron) shape: words, no segments.
        let body = r#"{
            "text": "hello world",
            "duration": 2.5,
            "words": [{"word": "hello", "start": 0.0, "end": 1.0, "confidence": 1},
                      {"word": "world", "start": 1.0, "end": 2.5, "confidence": 1}]
        }"#;
        let value: Value = serde_json::from_str(body).unwrap();
        let transcript = decode_transcript(&value, None).unwrap();
        assert!(transcript.segments.is_empty());
        assert_eq!(transcript.words.len(), 2);
        assert_eq!(transcript.words[0].word, "hello");
        assert_eq!(transcript.words[1].end, 2.5);
    }

    #[test]
    fn an_api_error_or_an_empty_transcript_fails_loud() {
        let error: Value = serde_json::from_str(r#"{"error":"no such model"}"#).unwrap();
        assert!(matches!(decode_transcript(&error, None), Err(Error::Api(_))));

        let empty: Value = serde_json::from_str(r#"{"text":"  "}"#).unwrap();
        assert!(matches!(
            decode_transcript(&empty, None),
            Err(Error::EmptyTranscript)
        ));
    }

    #[test]
    fn scripted_stt_answers_in_order_and_runs_out_loudly() {
        let client = ScriptedStt::new([r#"{"text":"hi","segments":[]}"#]);
        let transcript = client.transcribe(&SttRequest::wav(vec![0u8; 4])).unwrap();
        assert_eq!(transcript.text, "hi");

        let empty = ScriptedStt::new(Vec::<String>::new());
        assert!(empty.transcribe(&SttRequest::wav(vec![])).is_err());
    }
}
