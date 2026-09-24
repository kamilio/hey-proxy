use super::store::Entry;
use serde::Serialize;
use serde_json::Value;
use std::sync::LazyLock;

pub const BOOK: &str = include_str!("prices.json");
static PRICES: LazyLock<Value> =
    LazyLock::new(|| serde_json::from_str(BOOK).expect("embedded prices"));

#[derive(Default, Serialize)]
pub struct Price {
    pub price_model: Option<String>,
    pub price_version: String,
    pub cost_nano_usd: Option<i64>,
}

pub fn price(entry: &Entry) -> Price {
    let resolve = |name: &str| {
        let name = PRICES["aliases"][name].as_str().unwrap_or(name);
        PRICES["prices"]
            .get(name)
            .map(|rates| (name.to_owned(), rates))
    };
    let selected = entry.routed_model.as_deref().and_then(resolve).or_else(|| {
        if entry
            .routed_model
            .as_deref()
            .is_some_and(|m| m.starts_with("gemini/"))
        {
            None
        } else {
            entry.requested_model.as_deref().and_then(resolve)
        }
    });
    let mut result = Price {
        price_model: selected.as_ref().map(|(name, _)| name.clone()),
        price_version: PRICES["version"].as_str().unwrap().into(),
        cost_nano_usd: None,
    };
    let Some((_, rates)) = selected else {
        return result;
    };
    if !matches!(entry.method.as_str(), "POST" | "SEND")
        || !matches!(
            entry.path.as_str(),
            "/v1/responses" | "/v1/responses/compact" | "/v1/chat/completions" | "/v1/completions"
        )
    {
        return result;
    }
    let (Some(input), Some(output)) = (entry.input_tokens, entry.output_tokens) else {
        return result;
    };
    let cached = entry.cached_input_tokens.unwrap_or(0);
    let writes = entry.cache_write_tokens.unwrap_or(0);
    if cached.saturating_add(writes) > input {
        return result;
    }
    let input_rate = rates[0].as_f64().unwrap();
    let long = rates[4].as_u64().is_some_and(|threshold| input > threshold);
    let cost = ((input - cached - writes) as f64 * input_rate
        + cached as f64 * rates[1].as_f64().unwrap_or(input_rate)
        + writes as f64 * rates[3].as_f64().unwrap_or(input_rate))
        * if long { 2.0 } else { 1.0 }
        + output as f64 * rates[2].as_f64().unwrap() * if long { 1.5 } else { 1.0 };
    let nanos = cost * 1000.0;
    if nanos.is_finite() && nanos < i64::MAX as f64 {
        result.cost_nano_usd = Some(nanos.round() as i64);
    }
    result
}
