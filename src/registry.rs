//! Local model registry: canonical ids, aliases and `/v1/models` payload.

use serde_json::{Value, json};

use crate::config::{Config, Model};

/// Resolve a requested model id: canonical first, then aliases.
pub fn resolve<'a>(cfg: &'a Config, requested: &str) -> Option<&'a Model> {
    if let Some(m) = cfg.models.iter().find(|m| m.id == requested) {
        return Some(m);
    }
    cfg.models
        .iter()
        .find(|m| m.aliases.iter().any(|a| a == requested))
}

/// `(alias, canonical)` pairs, sorted, for `umsp model list --aliases`.
pub fn alias_table(cfg: &Config) -> Vec<(String, String)> {
    let mut rows: Vec<(String, String)> = cfg
        .models
        .iter()
        .flat_map(|m| {
            m.aliases
                .iter()
                .map(move |a| (a.clone(), m.id.clone()))
                .collect::<Vec<_>>()
        })
        .collect();
    rows.sort();
    rows
}

/// Flattened model metadata for `GET /v1/models`.
///
/// Hermes reads top-level `context_length` / `max_completion_tokens`; the
/// OpenDesign raw shape is `metadata.context_limit`, which is flattened by
/// config authors / `umsp model add`. Providers and upstream ids stay out of
/// this payload on purpose: a model is one logical entry, and callers should
/// not have to care which backend serves it.
pub fn model_payload(m: &Model) -> Value {
    json!({
        "id": m.id,
        "object": "model",
        "owned_by": "umsp",
        "created": m.created,
        "context_length": m.context_length,
        "max_completion_tokens": m.max_completion_tokens,
        "capabilities": capabilities_object(&m.capabilities),
        "display_name": m.display_name(),
        "description": m.description.clone().unwrap_or_default(),
    })
}

/// Control-plane view: same metadata plus the ordered backends, for
/// `umsp model list` (never served over HTTP).
pub fn model_admin_payload(m: &Model) -> Value {
    let mut payload = model_payload(m);
    payload["aliases"] = json!(m.aliases);
    payload["backends"] = json!(
        m.backends
            .iter()
            .map(|b| json!({"provider": b.provider, "upstream": b.upstream}))
            .collect::<Vec<_>>()
    );
    payload
}

pub fn models_payload(cfg: &Config) -> Value {
    let data: Vec<Value> = cfg.models.iter().map(model_payload).collect();
    json!({ "object": "list", "data": data })
}

fn capabilities_object(caps: &[String]) -> Value {
    let mut obj = serde_json::Map::new();
    for key in ["text", "vision", "tools", "reasoning"] {
        obj.insert(key.to_string(), Value::Bool(caps.iter().any(|c| c == key)));
    }
    for cap in caps {
        if !obj.contains_key(cap) {
            obj.insert(cap.clone(), Value::Bool(true));
        }
    }
    Value::Object(obj)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Backend, Config, DEFAULT_CREATED, Model, ServerConfig};

    fn cfg() -> Config {
        Config {
            server: ServerConfig::default(),
            providers: vec![],
            models: vec![Model {
                id: "od/m1".into(),
                backends: vec![Backend {
                    provider: "od".into(),
                    upstream: "m1-up".into(),
                }],
                aliases: vec!["m1".into()],
                context_length: 1000,
                max_completion_tokens: 100,
                capabilities: vec!["text".into(), "tools".into()],
                display_name: Some("Model One".into()),
                created: DEFAULT_CREATED,
                description: None,
            }],
            clients: vec![],
        }
    }

    #[test]
    fn resolves_canonical_and_alias() {
        let c = cfg();
        assert_eq!(resolve(&c, "od/m1").unwrap().backends[0].upstream, "m1-up");
        assert_eq!(resolve(&c, "m1").unwrap().backends[0].upstream, "m1-up");
        assert!(resolve(&c, "nope").is_none());
    }

    #[test]
    fn payload_is_flat() {
        let c = cfg();
        let payload = models_payload(&c);
        let model = &payload["data"][0];
        assert_eq!(model["context_length"], 1000);
        assert_eq!(model["max_completion_tokens"], 100);
        assert_eq!(model["capabilities"]["tools"], true);
        assert_eq!(model["capabilities"]["vision"], false);
        assert_eq!(model["owned_by"], "umsp");
        assert!(
            model.get("provider").is_none() && model.get("upstream_model").is_none(),
            "the HTTP payload must not leak providers or upstream ids"
        );
        assert_eq!(model["display_name"], "Model One");
        assert!(model.get("metadata").is_none());
    }

    #[test]
    fn alias_table_sorted() {
        let c = cfg();
        assert_eq!(
            alias_table(&c),
            vec![("m1".to_string(), "od/m1".to_string())]
        );
    }
}
