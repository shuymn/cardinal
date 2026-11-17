use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::{Deserialize, Serialize};

use super::{Resources, RunSpecSchema};

/// `cardinal.run-spec.v1`: everything the runner needs from the hook, passed as
/// `--spec-b64` (unpadded base64url of this JSON document).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunSpec {
    pub schema: RunSpecSchema,
    pub class: Option<String>,
    pub rule: Option<String>,
    pub resources: Resources,
    pub timeout_ms: u64,
    pub background: bool,
    pub stage_index: u32,
    pub stage: String,
    pub session_id: Option<String>,
    pub agent_id: Option<String>,
    pub tool_use_id: Option<String>,
}

/// A `--spec-b64` value that cannot be decoded.
#[derive(Debug, thiserror::Error)]
pub enum SpecError {
    #[error("spec is not unpadded base64url: {0}")]
    Base64(#[from] base64::DecodeError),
    #[error("spec is not a cardinal.run-spec.v1 document: {0}")]
    Json(#[from] serde_json::Error),
}

impl RunSpec {
    /// Encodes the spec as unpadded base64url, which contains nothing a shell treats specially.
    #[must_use]
    pub fn encode_b64(&self) -> String {
        let json = serde_json::to_vec(self).expect("a run spec always serializes");
        URL_SAFE_NO_PAD.encode(json)
    }

    /// Decodes a `--spec-b64` value.
    pub fn decode_b64(value: &str) -> Result<Self, SpecError> {
        let json = URL_SAFE_NO_PAD.decode(value)?;
        Ok(serde_json::from_slice(&json)?)
    }
}

#[cfg(test)]
mod tests {
    use super::{RunSpec, RunSpecSchema};

    fn spec() -> RunSpec {
        RunSpec {
            schema: RunSpecSchema,
            class: Some("test".into()),
            rule: Some("generic/test".into()),
            resources: [("cpu".to_owned(), std::num::NonZeroU64::new(4).unwrap())]
                .into_iter()
                .collect(),
            timeout_ms: 120_000,
            background: false,
            stage_index: 1,
            stage: "pnpm test 2>&1 | tail -5 # ü".into(),
            session_id: Some("s".into()),
            agent_id: None,
            tool_use_id: Some("toolu_1".into()),
        }
    }

    #[test]
    fn encoding_is_shell_safe_and_round_trips() {
        let encoded = spec().encode_b64();
        assert!(
            encoded
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
            "{encoded}"
        );
        assert_eq!(RunSpec::decode_b64(&encoded).unwrap(), spec());
    }

    #[test]
    fn padded_or_foreign_input_is_rejected() {
        let mut padded = spec().encode_b64();
        padded.push('=');
        assert!(RunSpec::decode_b64(&padded).is_err());
        assert!(
            RunSpec::decode_b64("e30").is_err(),
            "{{}} lacks every field"
        );
    }
}
