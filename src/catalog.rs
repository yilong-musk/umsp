//! Upstream model catalogs, deduplicated into the logical models callers see.
//!
//! Each provider that answers `GET {base_url}/models` contributes its ids.
//! The same id on several providers is one logical model, with one backend per
//! provider in configuration order. A configured `[[models]]` entry keeps its
//! metadata and any backend whose upstream name differs from the advertised id.
//! A configured model whose id no provider has listed is kept, so a provider
//! without a model list (or whose last fetch failed) does not disappear.

use std::collections::HashMap;

use serde_json::Value;

use crate::config::{Backend, DEFAULT_CREATED, Model};

/// Model ids from an OpenAI-style `{ "data": [ { "id" } ] }` body.
/// `None` means the body is not a list, so the caller should keep the previous
/// snapshot. An empty `data` array is a real empty catalog.
pub fn parse_model_ids(body: &Value) -> Option<Vec<String>> {
    let rows = body.get("data")?.as_array()?;
    let mut ids = Vec::new();
    for row in rows {
        let Some(id) = row.get("id").and_then(Value::as_str) else {
            continue;
        };
        let id = id.trim();
        if id.is_empty() || ids.iter().any(|seen| seen == id) {
            continue;
        }
        ids.push(id.to_string());
    }
    Some(ids)
}

/// `listings` holds the last successful id list for each provider. Providers
/// absent from the map have never listed models.
pub fn merge(
    static_models: &[Model],
    provider_ids: &[String],
    listings: &HashMap<String, Vec<String>>,
) -> Vec<Model> {
    let mut models: Vec<Model> = Vec::new();
    for provider in provider_ids {
        let Some(ids) = listings.get(provider) else {
            continue;
        };
        for id in ids {
            if let Some(existing) = models.iter_mut().find(|model| model.id == *id) {
                push_backend(
                    existing,
                    Backend {
                        provider: provider.clone(),
                        upstream: id.clone(),
                    },
                );
            } else {
                models.push(discovered_model(id, provider));
            }
        }
    }

    let mut configured_only = Vec::new();
    for configured in static_models {
        if let Some(existing) = models.iter_mut().find(|model| model.id == configured.id) {
            overlay(existing, configured);
        } else {
            configured_only.push(configured.clone());
        }
    }
    models.extend(configured_only);
    models
}

fn discovered_model(id: &str, provider: &str) -> Model {
    Model {
        id: id.to_string(),
        aliases: Vec::new(),
        context_length: 0,
        max_completion_tokens: 0,
        // Callers can request it. Richer capabilities come from a static entry.
        capabilities: vec!["text".to_string()],
        display_name: None,
        created: DEFAULT_CREATED,
        backends: vec![Backend {
            provider: provider.to_string(),
            upstream: id.to_string(),
        }],
        description: None,
    }
}

fn overlay(model: &mut Model, configured: &Model) {
    if !configured.aliases.is_empty() {
        model.aliases = configured.aliases.clone();
    }
    if configured.context_length != 0 {
        model.context_length = configured.context_length;
    }
    if configured.max_completion_tokens != 0 {
        model.max_completion_tokens = configured.max_completion_tokens;
    }
    if !configured.capabilities.is_empty() {
        model.capabilities = configured.capabilities.clone();
    }
    if configured.display_name.is_some() {
        model.display_name = configured.display_name.clone();
    }
    if configured.description.is_some() {
        model.description = configured.description.clone();
    }
    if configured.created != DEFAULT_CREATED {
        model.created = configured.created;
    }
    for backend in &configured.backends {
        push_backend(model, backend.clone());
    }
}

fn push_backend(model: &mut Model, backend: Backend) {
    if model.backends.iter().any(|existing| {
        existing.provider == backend.provider && existing.upstream == backend.upstream
    }) {
        return;
    }
    model.backends.push(backend);
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn static_model(id: &str, provider: &str, upstream: &str) -> Model {
        Model {
            id: id.into(),
            aliases: vec!["alias".into()],
            context_length: 1000,
            max_completion_tokens: 100,
            capabilities: vec!["text".into(), "tools".into()],
            display_name: Some(format!("Name {id}")),
            created: 42,
            backends: vec![Backend {
                provider: provider.into(),
                upstream: upstream.into(),
            }],
            description: None,
        }
    }

    #[test]
    fn parses_openai_ids_and_rejects_a_non_list() {
        let body =
            json!({"data": [{"id": "a"}, {"id": " a "}, {"id": ""}, {"name": "x"}, {"id": "b"}]});
        assert_eq!(
            parse_model_ids(&body).unwrap(),
            vec!["a".to_string(), "b".to_string()]
        );
        assert!(parse_model_ids(&json!({"models": []})).is_none());
        assert_eq!(
            parse_model_ids(&json!({"data": []})).unwrap(),
            Vec::<String>::new()
        );
    }

    #[test]
    fn same_id_on_two_providers_is_one_model() {
        let mut listings = HashMap::new();
        listings.insert(
            "open-design".into(),
            vec!["qwen3.6-flash".into(), "only-od".into()],
        );
        listings.insert("qwen".into(), vec!["qwen3.6-flash".into()]);
        let merged = merge(&[], &["open-design".into(), "qwen".into()], &listings);
        assert_eq!(merged.len(), 2);
        assert_eq!(merged[0].id, "qwen3.6-flash");
        assert_eq!(merged[0].backends.len(), 2);
        assert_eq!(merged[0].backends[0].provider, "open-design");
        assert_eq!(merged[0].backends[1].provider, "qwen");
        assert_eq!(merged[0].backends[0].upstream, "qwen3.6-flash");
        assert_eq!(merged[1].id, "only-od");
    }

    #[test]
    fn static_metadata_and_a_differently_named_backend_are_kept() {
        let mut listings = HashMap::new();
        listings.insert("open-design".into(), vec!["deepseek-v4.1-flash".into()]);
        let configured = static_model("deepseek-v4.1-flash", "deepseek", "deepseek-flash");
        let merged = merge(
            &[configured],
            &["open-design".into(), "deepseek".into()],
            &listings,
        );
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].context_length, 1000);
        assert_eq!(merged[0].aliases, vec!["alias".to_string()]);
        assert_eq!(merged[0].backends.len(), 2);
        assert_eq!(merged[0].backends[1].upstream, "deepseek-flash");
    }

    #[test]
    fn a_model_nobody_listed_stays() {
        let grok = static_model("grok-4.7", "grok", "grok-4.7");
        let mut listed = HashMap::new();
        listed.insert("open-design".into(), vec!["qwen3.6-flash".into()]);
        let merged = merge(&[grok], &["open-design".into(), "grok".into()], &listed);
        assert_eq!(merged.len(), 2);
        assert_eq!(merged[0].id, "qwen3.6-flash");
        assert_eq!(merged[1].id, "grok-4.7");
    }
}
