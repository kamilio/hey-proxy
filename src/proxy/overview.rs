//! A local, allowlisted catalog. No upstream discovery or credential resolution.
use super::*;
use serde::Serialize;
use std::collections::BTreeSet;

#[derive(Serialize)]
struct Model {
    id: String,
    routes: Vec<ModelRoute>,
}

#[derive(Serialize)]
struct ModelRoute {
    target: String,
    when: String,
}

#[derive(Serialize)]
struct Api {
    id: &'static str,
    name: &'static str,
    description: &'static str,
    base_path: &'static str,
    routes: Vec<(&'static str, &'static str, &'static str)>,
    configured: bool,
    models: Vec<Model>,
}

// Destinations remain usable as direct model names even when their alias belongs
// to a different API shape. Alias names themselves must respect their scope.
fn destinations(config: &Config) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    for alias in &config.aliases {
        names.extend(alias.to.iter().cloned());
        names.extend(alias.reasoning_routes.values().map(|r| r.to.clone()));
    }
    for (source, targets) in &config.fallbacks {
        names.insert(source.clone());
        names.extend(targets.iter().cloned());
    }
    names
}

fn models(config: &Config, path: &str, supports_gemini: bool) -> Vec<Model> {
    let mut names = destinations(config);
    names.extend(
        config
            .aliases
            .iter()
            .filter(|a| a.matches_shape(path))
            .map(|a| a.from.clone()),
    );
    let available = |target: &str| {
        if target.starts_with("gemini/") {
            supports_gemini && config.gemini.is_some()
        } else {
            !config.api_keys.is_empty()
        }
    };
    names
        .into_iter()
        .filter_map(|id| {
            let alias = config.alias_for(&id, path);
            let target = alias.and_then(|a| a.to.as_deref()).unwrap_or(&id);
            let mut routes = Vec::new();
            if available(target) {
                routes.push(ModelRoute {
                    target: target.to_owned(),
                    when: if alias.is_some_and(|a| !a.reasoning_routes.is_empty()) {
                        "Default".into()
                    } else {
                        String::new()
                    },
                });
            }
            if let Some(alias) = alias {
                for (effort, route) in &alias.reasoning_routes {
                    if available(&route.to) {
                        routes.push(ModelRoute {
                            target: route.to.clone(),
                            when: format!("Reasoning: {effort}"),
                        });
                    }
                }
            }
            (!routes.is_empty()).then_some(Model { id, routes })
        })
        .collect()
}

fn native_models(config: &Config) -> Vec<Model> {
    if config.gemini.is_none() {
        return Vec::new();
    }
    let mut names: BTreeSet<String> = destinations(config)
        .iter()
        .chain(config.aliases.iter().map(|a| &a.from))
        .filter_map(|s| s.strip_prefix("gemini/"))
        .map(|s| s.strip_prefix("models/").unwrap_or(s).to_owned())
        .collect();
    names.extend(
        config
            .aliases
            .iter()
            .filter(|a| {
                a.api_shape.is_none()
                    && a.to.as_deref().is_some_and(|t| t.starts_with("gemini/"))
                    && !a.from.starts_with("gemini/")
            })
            .map(|a| a.from.strip_prefix("models/").unwrap_or(&a.from).to_owned()),
    );
    names
        .into_iter()
        .filter_map(|id| {
            // Match the native handler's URL alias lookup and cross-provider rule.
            let alias = config.aliases.iter().find(|a| {
                a.api_shape.is_none() && (a.from == id || a.from == format!("models/{id}"))
            });
            let target = match alias.and_then(|a| a.to.as_deref()) {
                Some(target) => target.strip_prefix("gemini/")?,
                None => &id,
            };
            let target = target.strip_prefix("models/").unwrap_or(target).to_owned();
            Some(Model {
                id,
                routes: vec![ModelRoute {
                    target,
                    when: String::new(),
                }],
            })
        })
        .collect()
}

fn catalog(config: &Config) -> Value {
    let relay = config.mode == Mode::Client;
    let openai = !relay && !config.api_keys.is_empty();
    let gemini = config.gemini.is_some();
    let apis = vec![
        Api {
            id: "responses",
            name: "Responses",
            description: "Responses requests for OpenAI and Gemini. Supports HTTP streaming, tools and reasoning. Gemini uses the Responses converter.",
            base_path: "/v1",
            routes: vec![
                ("POST", "/v1/responses", ""),
                ("POST", "/v1/responses/compact", "OpenAI models only"),
            ],
            configured: openai || gemini,
            models: models(config, "/v1/responses", true),
        },
        Api {
            id: "custom-chat",
            name: "Custom Chat Completions",
            description: "Chat Completions translated through Responses. Supports OpenAI and Gemini, streaming, function tools and reasoning replay.",
            base_path: "/v1/custom",
            routes: vec![("POST", "/v1/custom/chat/completions", "")],
            configured: openai || gemini,
            models: models(config, "/v1/custom/chat/completions", true),
        },
        Api {
            id: "chat",
            name: "Chat Completions",
            description: "Chat Completions forwarded to the OpenAI upstream with your overwrite rules. The upstream model must support this API.",
            base_path: "/v1",
            routes: vec![("POST", "/v1/chat/completions", "")],
            configured: openai,
            models: models(config, "/v1/chat/completions", false),
        },
        Api {
            id: "completions",
            name: "Completions",
            description: "Legacy text completions forwarded to the OpenAI upstream. The upstream model must support this API.",
            base_path: "/v1",
            routes: vec![("POST", "/v1/completions", "")],
            configured: openai,
            models: models(config, "/v1/completions", false),
        },
        Api {
            id: "gemini",
            name: "Gemini native",
            description: "Native Gemini requests and responses. Use the model name in the URL; streaming uses alt=sse. Other native Gemini paths are also forwarded.",
            base_path: "/v1beta",
            routes: vec![
                ("POST", "/v1beta/models/{model}:generateContent", ""),
                (
                    "POST",
                    "/v1beta/models/{model}:streamGenerateContent?alt=sse",
                    "",
                ),
                ("POST", "/v1beta/models/{model}:countTokens", ""),
            ],
            configured: gemini,
            models: native_models(config),
        },
    ];
    // Construct fields explicitly: Config contains credential sources and host keys.
    json!({"mode":config.mode,"relay":relay,"apis":apis})
}

pub(super) async fn page() -> Response {
    axum::response::Html(include_str!("overview.html")).into_response()
}

pub(super) async fn script() -> Response {
    (
        [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
        include_str!("overview.js"),
    )
        .into_response()
}

pub(super) async fn data(State(service): State<Arc<Service>>) -> Response {
    let proxy = service.snapshot();
    (
        [(header::CACHE_CONTROL, "no-store")],
        axum::Json(catalog(&proxy.config)),
    )
        .into_response()
}

#[cfg(test)]
mod tests;
