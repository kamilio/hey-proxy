//! Validate known native structures without rejecting future native extensions.
use anyhow::{Result, bail, ensure};
use serde_json::Value;

fn fields(value: &Value, strings: &[&str], booleans: &[&str]) -> Result<()> {
    for key in strings {
        if let Some(v) = value.get(key) {
            ensure!(v.is_string(), "Gemini {key} must be a string");
        }
    }
    for key in booleans {
        if let Some(v) = value.get(key) {
            ensure!(v.is_boolean(), "Gemini {key} must be a boolean");
        }
    }
    Ok(())
}

pub(super) fn content(value: &Value, require_role: bool) -> Result<()> {
    ensure!(value.is_object(), "Gemini content must be an object");
    if require_role || value.get("role").is_some() {
        ensure!(
            matches!(value["role"].as_str(), Some("user" | "model")),
            "Invalid Gemini content role"
        );
    }
    let Some(parts) = value.get("parts").and_then(Value::as_array) else {
        bail!("Gemini content.parts must be an array");
    };
    for part in parts {
        ensure!(part.is_object(), "Gemini part must be an object");
        fields(part, &["text", "thoughtSignature"], &["thought"])?;
        let payloads = [
            "text",
            "functionCall",
            "functionResponse",
            "inlineData",
            "fileData",
            "executableCode",
            "codeExecutionResult",
        ];
        ensure!(
            payloads
                .iter()
                .filter(|key| part.get(**key).is_some())
                .count()
                <= 1,
            "Gemini part has conflicting payloads"
        );
        if let Some(call) = part.get("functionCall") {
            ensure!(call.is_object(), "Gemini functionCall must be an object");
            fields(call, &["name", "id"], &["willContinue"])?;
            if let Some(args) = call.get("args") {
                ensure!(
                    args.is_object(),
                    "Gemini functionCall.args must be an object"
                );
            }
            if let Some(args) = call.get("partialArgs") {
                let Some(args) = args.as_array() else {
                    bail!("Gemini partialArgs must be an array")
                };
                for arg in args {
                    ensure!(arg.is_object(), "Gemini partial argument must be an object");
                    fields(arg, &["jsonPath"], &["willContinue"])?;
                }
            }
        }
    }
    Ok(())
}

pub(super) fn response(value: &Value) -> Result<()> {
    ensure!(value.is_object(), "Gemini response must be an object");
    if let Some(candidates) = value.get("candidates") {
        let Some(candidates) = candidates.as_array() else {
            bail!("Gemini candidates must be an array")
        };
        ensure!(
            candidates.len() <= 1,
            "Cannot discard multiple Gemini candidates"
        );
        for candidate in candidates {
            ensure!(candidate.is_object(), "Gemini candidate must be an object");
            if let Some(index) = candidate.get("index") {
                ensure!(
                    index.as_u64() == Some(0),
                    "Gemini candidate index must be zero"
                );
            }
            fields(candidate, &["finishReason"], &[])?;
            if let Some(c) = candidate.get("content") {
                content(c, false)?;
                ensure!(
                    c.get("role").is_none() || c["role"] == "model",
                    "Gemini response role must be model"
                );
            }
        }
    }
    if let Some(usage) = value.get("usageMetadata") {
        ensure!(usage.is_object(), "Gemini usageMetadata must be an object");
        for key in [
            "promptTokenCount",
            "candidatesTokenCount",
            "thoughtsTokenCount",
            "cachedContentTokenCount",
            "totalTokenCount",
        ] {
            if let Some(count) = usage.get(key) {
                ensure!(
                    count.as_u64().is_some(),
                    "Gemini {key} must be a nonnegative integer"
                );
            }
        }
    }
    if let Some(feedback) = value.get("promptFeedback") {
        ensure!(
            feedback.is_object(),
            "Gemini promptFeedback must be an object"
        );
        fields(feedback, &["blockReason"], &[])?;
    }
    Ok(())
}
