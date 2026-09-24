//! Embeddings: audio in, a fixed-dimension vector out — `ooda`'s third
//! capability against Bifrost, beside [`crate::Client::decide`] (bounded) and
//! [`crate::Complete::complete`] (open text).
//!
//! Unlike those two, the request carries *binary* input. Bifrost's routes are
//! JSON, so the audio travels base64-encoded inside the body; a caller
//! supplies raw bytes and never sees the encoding.
//!
//! The canonical use is a local-first music library (deadwax): a track's
//! audio becomes a 1280-d Discogs-EffNet vector, computed by the gateway
//! (drop-in with the model's own Essentia mel front-end as the oracle), so
//! similarity search is a local cosine over vectors this client fetched —
//! no model, no ONNX runtime, and no DSP in the caller.
//!
//! An embeddings call usually wants a *different* model than a decision or a
//! completion: the caller builds a second [`HttpClient`] with
//! [`crate::HttpClient::with_model`], exactly as [`crate::Complete`]'s docs
//! already describe. There is still one place a model is configured.
//!
//! The wire shape below is the client's half of the contract with the
//! gateway route; the response is OpenAI-shaped so it reads like every other
//! embeddings client:
//!
//! ```json
//! // request
//! { "model": "fpl/embed", "input": [ { "audio": "<base64>", "format": "wav" } ] }
//! // response
//! { "data": [ { "embedding": [/* 1280 floats */], "index": 0 } ],
//!   "model": "mtg/discogs-effnet" }
//! ```

use std::collections::VecDeque;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::Error;
use crate::http::HttpClient;
use crate::question::Usage;

/// Bifrost's embeddings route, appended to a client's base URL — the same
/// base [`HttpClient::endpoint`] and [`crate::complete::CHAT_PATH`] resolve
/// against, since every capability is a route on one gateway.
pub const EMBEDDINGS_PATH: &str = "/v1/embeddings";

/// A request for one audio embedding.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EmbedRequest {
    /// Raw audio bytes. Mono 16 kHz PCM WAV is the canonical input the
    /// gateway's front-end expects.
    pub audio: Vec<u8>,
    /// Container hint for those bytes, e.g. `"wav"`.
    pub format: String,
}

impl EmbedRequest {
    /// A request for WAV audio.
    #[must_use]
    pub fn wav(audio: impl Into<Vec<u8>>) -> Self {
        EmbedRequest {
            audio: audio.into(),
            format: "wav".to_owned(),
        }
    }
}

/// One embedding: the vector, plus whatever the endpoint reported about how
/// it was produced.
#[derive(Clone, Debug, PartialEq)]
pub struct Embedding {
    /// The embedding vector, in the model's own dimension (1280 for
    /// Discogs-EffNet).
    pub vector: Vec<f32>,
    /// The backend model that actually answered, when reported (useful
    /// behind a routing alias like `fpl/embed`).
    pub model: Option<String>,
    /// Token accounting, when the endpoint reported it.
    pub usage: Option<Usage>,
}

/// Anything that can embed audio.
///
/// A separate trait from [`crate::Client`] and [`crate::Complete`], not
/// another method on either: the three have unrelated wire shapes and
/// unrelated response types (a bounded [`crate::Outcome`], a `String`, a
/// [`Embedding`]), so a caller that only ever decides should never need to
/// know this trait exists.
pub trait Embed {
    /// # Errors
    /// See [`Error`].
    fn embed(&self, request: &EmbedRequest) -> Result<Embedding, Error>;
}

#[derive(Serialize)]
struct EmbedBody<'a> {
    model: &'a str,
    input: [EmbedInput<'a>; 1],
}

#[derive(Serialize)]
struct EmbedInput<'a> {
    /// Base64-encoded audio.
    audio: &'a str,
    format: &'a str,
}

impl Embed for HttpClient {
    fn embed(&self, request: &EmbedRequest) -> Result<Embedding, Error> {
        let encoded = encode_base64(&request.audio);
        let body = serde_json::to_value(EmbedBody {
            model: self.model(),
            input: [EmbedInput {
                audio: &encoded,
                format: &request.format,
            }],
        })
        .map_err(|source| Error::Decode {
            source,
            body: "<embed request serialization failed>".to_owned(),
        })?;
        let (value, resolved_model, ..) = self.post(&self.url(EMBEDDINGS_PATH), &body)?;
        decode_embedding(&value, resolved_model)
    }
}

/// The OpenAI-shaped response, decoded just far enough to pull the first
/// vector out. Unknown fields are ignored, so a backend that grows the
/// payload does not break this client.
#[derive(Deserialize)]
struct RawEmbedding {
    #[serde(default)]
    data: Vec<RawEmbeddingData>,
    #[serde(default)]
    model: Option<String>,
}

#[derive(Deserialize)]
struct RawEmbeddingData {
    #[serde(default)]
    embedding: Vec<f32>,
}

/// Decodes a full embeddings response body — the shared core of
/// [`HttpClient::embed`] and [`ScriptedEmbeddings::embed`], so the two can
/// never drift on how a response is read.
pub(crate) fn decode_embedding(
    value: &Value,
    resolved_model: Option<String>,
) -> Result<Embedding, Error> {
    if let Some(message) = crate::http::extract_error_message(value) {
        return Err(Error::Api(message));
    }
    let raw: RawEmbedding =
        serde_json::from_value(value.clone()).map_err(|source| Error::Decode {
            source,
            body: crate::error::truncate(&value.to_string()),
        })?;
    let vector = raw
        .data
        .into_iter()
        .next()
        .map(|entry| entry.embedding)
        .unwrap_or_default();
    if vector.is_empty() {
        return Err(Error::EmptyEmbedding);
    }
    Ok(Embedding {
        vector,
        model: resolved_model.or(raw.model),
        usage: crate::http::raw_usage(value),
    })
}

/// Answers [`Embed::embed`] from a queue of canned response bodies, in
/// order — [`crate::ScriptedClient`]'s counterpart for [`Embed`], kept
/// separate for the same reason [`crate::ScriptedComplete`] is: the three
/// capabilities have unrelated wire shapes, and mixing their canned data
/// into one queue would make a test harder to read.
#[derive(Debug, Default)]
pub struct ScriptedEmbeddings {
    bodies: Mutex<VecDeque<String>>,
}

impl ScriptedEmbeddings {
    /// Queues response bodies, answered in order.
    #[must_use]
    pub fn new<S: Into<String>>(bodies: impl IntoIterator<Item = S>) -> Self {
        Self {
            bodies: Mutex::new(bodies.into_iter().map(Into::into).collect()),
        }
    }
}

impl Embed for ScriptedEmbeddings {
    fn embed(&self, _request: &EmbedRequest) -> Result<Embedding, Error> {
        let body = self
            .bodies
            .lock()
            .expect("response queue poisoned")
            .pop_front()
            .ok_or_else(|| {
                Error::Transport("ScriptedEmbeddings ran out of responses".to_owned())
            })?;
        let value: Value = serde_json::from_str(&body).map_err(|source| Error::Decode {
            source,
            body: crate::error::truncate(&body),
        })?;
        decode_embedding(&value, None)
    }
}

/// Standard base64 (RFC 4648) encoding, so audio can ride inside the JSON
/// body. Kept local rather than pulled in as a dependency: this is the only
/// place in the crate that needs an encoding, and a dependency gate is worth
/// more than sixteen lines.
fn encode_base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let triple = (b0 << 16) | (b1 << 8) | b2;
        out.push(ALPHABET[((triple >> 18) & 0x3f) as usize] as char);
        out.push(ALPHABET[((triple >> 12) & 0x3f) as usize] as char);
        if chunk.len() > 1 {
            out.push(ALPHABET[((triple >> 6) & 0x3f) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(ALPHABET[(triple & 0x3f) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_the_rfc_4648_vectors() {
        assert_eq!(encode_base64(b""), "");
        assert_eq!(encode_base64(b"f"), "Zg==");
        assert_eq!(encode_base64(b"fo"), "Zm8=");
        assert_eq!(encode_base64(b"foo"), "Zm9v");
        assert_eq!(encode_base64(b"foob"), "Zm9vYg==");
        assert_eq!(encode_base64(b"fooba"), "Zm9vYmE=");
        assert_eq!(encode_base64(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn a_response_decodes_to_its_first_vector() {
        let body = r#"{"data":[{"embedding":[0.1,0.2,0.3],"index":0}],
            "model":"mtg/discogs-effnet"}"#;
        let value: Value = serde_json::from_str(body).unwrap();
        let embedding = decode_embedding(&value, None).unwrap();
        assert_eq!(embedding.vector, vec![0.1, 0.2, 0.3]);
        assert_eq!(embedding.model.as_deref(), Some("mtg/discogs-effnet"));
    }

    #[test]
    fn the_resolved_model_header_wins_over_the_body() {
        let body = r#"{"data":[{"embedding":[1.0]}],"model":"body-model"}"#;
        let value: Value = serde_json::from_str(body).unwrap();
        let embedding = decode_embedding(&value, Some("header-model".to_owned())).unwrap();
        assert_eq!(embedding.model.as_deref(), Some("header-model"));
    }

    #[test]
    fn an_api_error_or_an_empty_vector_fails_loud() {
        let error: Value = serde_json::from_str(r#"{"error":"no such model"}"#).unwrap();
        assert!(matches!(decode_embedding(&error, None), Err(Error::Api(_))));

        let empty: Value = serde_json::from_str(r#"{"data":[]}"#).unwrap();
        assert!(matches!(
            decode_embedding(&empty, None),
            Err(Error::EmptyEmbedding)
        ));
    }

    #[test]
    fn scripted_embeddings_answer_in_queue_order_and_run_out_loudly() {
        let client = ScriptedEmbeddings::new([r#"{"data":[{"embedding":[1.0,2.0]}]}"#]);
        let embedding = client.embed(&EmbedRequest::wav(vec![0u8; 8])).unwrap();
        assert_eq!(embedding.vector, vec![1.0, 2.0]);

        let empty = ScriptedEmbeddings::new(Vec::<String>::new());
        assert!(empty.embed(&EmbedRequest::wav(vec![])).is_err());
    }
}
