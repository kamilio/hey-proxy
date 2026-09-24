//! Provider-independent Responses to native Gemini conversion.
//! HTTP, credential acquisition, retries and persistence belong to the caller.
pub mod gemini;

pub mod credentials;
pub mod fallback;
