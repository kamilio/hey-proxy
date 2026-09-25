use aes_gcm_siv::{
    Aes256GcmSiv, KeyInit, Nonce,
    aead::{Aead, Payload},
};
use anyhow::{Result, anyhow, bail};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde_json::Value;

/// Authenticated, stateless carrier for the exact native model turn. Persist the
/// caller-supplied key to replay conversations across process restarts.
pub struct ReasoningCodec(Aes256GcmSiv);
impl ReasoningCodec {
    pub fn new(key: &[u8; 32]) -> Self {
        Self(Aes256GcmSiv::new(key.into()))
    }
    /// Recover the authenticated visible output when adapting a flattened chat
    /// message back to Responses. Native signatures remain inside the carrier.
    pub fn replay_items(&self, model: &str, carrier: &str) -> Result<Vec<Value>> {
        self.open(model, carrier)?["items"]
            .as_array()
            .cloned()
            .ok_or_else(|| anyhow!("Invalid Gemini replay items"))
    }
    pub(crate) fn seal(&self, model: &str, parts: &Value, items: &Value) -> Result<String> {
        let nonce: [u8; 12] = rand::random();
        let data = serde_json::to_vec(&serde_json::json!({"parts":parts,"items":items}))?;
        let encrypted = self
            .0
            .encrypt(
                Nonce::from_slice(&nonce),
                Payload {
                    msg: &data,
                    aad: model.as_bytes(),
                },
            )
            .map_err(|_| anyhow!("Cannot encode Gemini reasoning"))?;
        let mut bytes = nonce.to_vec();
        bytes.extend(encrypted);
        Ok(format!("hey_gemini_v1.{}", URL_SAFE_NO_PAD.encode(bytes)))
    }
    pub(crate) fn open(&self, model: &str, carrier: &str) -> Result<Value> {
        let encoded = carrier.strip_prefix("hey_gemini_v1.").ok_or_else(|| {
            anyhow!("Reasoning belongs to another provider or codec; cannot discard it")
        })?;
        let bytes = URL_SAFE_NO_PAD.decode(encoded)?;
        if bytes.len() < 28 {
            bail!("Invalid Gemini reasoning carrier");
        }
        let decoded = self.0.decrypt(Nonce::from_slice(&bytes[..12]), Payload {msg:&bytes[12..],aad:model.as_bytes()})
            .map_err(|_| anyhow!("Gemini reasoning authentication failed (different model, key or altered carrier)"))?;
        Ok(serde_json::from_slice(&decoded)?)
    }
}
