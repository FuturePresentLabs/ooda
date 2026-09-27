//! Text-to-speech: text in, audio bytes out — `ooda`'s audio-out capability,
//! against Bifrost's `/v1/audio/speech` route.
//!
//! The request is JSON; the response is the upstream audio byte stream ([`Speech`]
//! carries the bytes plus the format headers Bifrost forwards). Kept separate
//! from [`crate::Stt`] on purpose — opposite directions, unrelated types.

use std::collections::VecDeque;
use std::sync::Mutex;

use crate::error::Error;
use crate::http::HttpClient;

/// Bifrost's text-to-speech route, appended to a client's base URL.
pub const SPEECH_PATH: &str = "/v1/audio/speech";

/// A speech-synthesis request.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct SpeechRequest {
    pub input: String,
    /// Named voice, when the backend offers them.
    pub voice: Option<String>,
    /// Container/encoding hint sent as `response_format` (`"wav"`, `"mp3"`,
    /// `"pcm"`, …).
    pub format: Option<String>,
}

impl SpeechRequest {
    #[must_use]
    pub fn new(input: impl Into<String>) -> Self {
        Self {
            input: input.into(),
            voice: None,
            format: None,
        }
    }

    #[must_use]
    pub fn with_voice(mut self, voice: impl Into<String>) -> Self {
        self.voice = Some(voice.into());
        self
    }

    #[must_use]
    pub fn with_format(mut self, format: impl Into<String>) -> Self {
        self.format = Some(format.into());
        self
    }
}

/// Synthesized audio, plus whatever the endpoint reported about its shape.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Speech {
    pub audio: Vec<u8>,
    pub content_type: Option<String>,
    pub sample_rate: Option<u32>,
    pub channels: Option<u16>,
    pub model: Option<String>,
}

/// Anything that can synthesize speech.
pub trait Tts {
    /// # Errors
    /// See [`Error`].
    fn speak(&self, request: &SpeechRequest) -> Result<Speech, Error>;
}

impl Tts for HttpClient {
    fn speak(&self, request: &SpeechRequest) -> Result<Speech, Error> {
        let body = serde_json::json!({
            "model": self.model(),
            "input": &request.input,
            "voice": &request.voice,
            "response_format": &request.format,
        });
        let (response, resolved_model, ..) =
            self.send_request(&self.url(SPEECH_PATH), |builder| {
                builder
                    .header(reqwest::header::CONTENT_TYPE, "application/json")
                    .json(&body)
            })?;

        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let sample_rate = header_u32(response.headers(), "x-fpl-audio-sample-rate");
        let channels = header_u32(response.headers(), "x-fpl-audio-channels").map(|n| n as u16);
        let audio = response
            .bytes()
            .map_err(|e| Error::Transport(e.to_string()))?
            .to_vec();
        if audio.is_empty() {
            return Err(Error::EmptySpeech);
        }
        Ok(Speech {
            audio,
            content_type,
            sample_rate,
            channels,
            model: resolved_model,
        })
    }
}

fn header_u32(headers: &reqwest::header::HeaderMap, name: &str) -> Option<u32> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse().ok())
}

/// Answers [`Tts::speak`] from a queue of canned clips, in order —
/// [`crate::ScriptedClient`]'s counterpart for [`Tts`].
#[derive(Debug, Default)]
pub struct ScriptedTts {
    clips: Mutex<VecDeque<Vec<u8>>>,
}

impl ScriptedTts {
    /// Queues audio clips, answered in order.
    #[must_use]
    pub fn new(clips: impl IntoIterator<Item = Vec<u8>>) -> Self {
        Self {
            clips: Mutex::new(clips.into_iter().collect()),
        }
    }
}

impl Tts for ScriptedTts {
    fn speak(&self, _request: &SpeechRequest) -> Result<Speech, Error> {
        let audio = self
            .clips
            .lock()
            .expect("clip queue poisoned")
            .pop_front()
            .ok_or_else(|| Error::Transport("ScriptedTts ran out of clips".to_owned()))?;
        if audio.is_empty() {
            return Err(Error::EmptySpeech);
        }
        Ok(Speech {
            audio,
            content_type: None,
            sample_rate: None,
            channels: None,
            model: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn speech_request_builds_with_voice_and_format() {
        assert_eq!(
            SpeechRequest::new("hello").with_voice("af").with_format("wav"),
            SpeechRequest {
                input: "hello".into(),
                voice: Some("af".into()),
                format: Some("wav".into()),
            }
        );
    }

    #[test]
    fn scripted_tts_answers_in_order_and_runs_out_loudly() {
        let client = ScriptedTts::new([vec![1u8, 2, 3]]);
        let speech = client.speak(&SpeechRequest::new("hi")).unwrap();
        assert_eq!(speech.audio, vec![1, 2, 3]);

        let empty = ScriptedTts::new(Vec::<Vec<u8>>::new());
        assert!(empty.speak(&SpeechRequest::new("hi")).is_err());
    }
}
