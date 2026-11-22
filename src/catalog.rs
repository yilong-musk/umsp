//! Upstream model catalogs, deduplicated into the logical models callers see.
//!
//! Each provider that answers `GET {base_url}/models` contributes its ids and
//! whatever metadata that list actually carries. The same id on several
//! providers is one logical model, with one backend per provider in
//! configuration order. A later listing fills only fields the earlier one left
//! empty. A configured `[[models]]` entry does the same for gaps, and keeps any
//! backend whose upstream name differs from the advertised id. A configured
//! model whose id no provider has listed is kept, so a provider without a
//! model list (or whose last fetch failed) does not disappear.

use std::collections::HashMap;

use serde_json::Value;

use crate::config::{Backend, DEFAULT_CREATED, Model};

/// One row of an upstream model list, after the fields we understand are read.
/// Absent numbers stay `0`. An id-only list (Qwen Token Plan) therefore has no
/// context and only the `text` capability.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListedModel {
    pub id: String,
    pub context_length: u64,
    pub max_completion_tokens: u64,
    pub capabilities: Vec<String>,
    pub display_name: Option<String>,
    pub description: Option<String>,
    pub created: i64,
}

/// Model rows from an OpenAI-style `{ "data": [ { "id", ... } ] }` body.
/// `None` means the body is not a list, so the caller should keep the previous
/// snapshot. An empty `data` array is a real empty catalog.
pub fn parse_models(body: &Value) -> Option<Vec<ListedModel>> {
    let rows = body.get("data")?.as_array()?;
    let mut models: Vec<ListedModel> = Vec::new();
    for row in rows {
        let Some(id) = row.get("id").and_then(Value::as_str) else {
            continue;
        };
        let id = id.trim();
        if id.is_empty() || models.iter().any(|seen| seen.id == id) {
            continue;
        }
        models.push(ListedModel {
            id: id.to_string(),
            context_length: first_u64(
                row,
                &["context_length", "context_window"],
                &["context_limit"],
            ),
            max_completion_tokens: first_u64(
                row,
                &["max_completion_tokens", "max_output_tokens"],
                &["output_budget"],
            ),
            capabilities: capabilities_of(row),
            display_name: non_empty_str(row.get("name")),
            description: non_empty_str(row.get("description")),
            created: row
                .get("created")
                .and_then(Value::as_i64)
                .filter(|created| *created > 0)
                .unwrap_or(DEFAULT_CREATED),
        });
    }
    Some(models)
}

/// `listings` holds the last successful list for each provider. Providers
/// absent from the map have never listed models.
pub fn merge(
    static_models: &[Model],
    provider_ids: &[String],
    listings: &HashMap<String, Vec<ListedModel>>,
) -> Vec<Model> {
    let mut models: Vec<Model> = Vec::new();
    for provider in provider_ids {
        let Some(listed) = listings.get(provider) else {
            continue;
        };
        for entry in listed {
            if let Some(existing) = models.iter_mut().find(|model| model.id == entry.id) {
                fill_gaps(existing, entry);
                push_backend(
                    existing,
                    Backend {
                        provider: provider.clone(),
                        upstream: entry.id.clone(),
                    },
                );
            } else {
                models.push(from_listed(entry, provider));
            }
        }
    }

    let mut configured_only = Vec::new();
    for configured in static_models {
        if let Some(existing) = models.iter_mut().find(|model| model.id == configured.id) {
            fill_from_static(existing, configured);
        } else {
            configured_only.push(configured.clone());
        }
    }
    models.extend(configured_only);
    models
}

fn from_listed(entry: &ListedModel, provider: &str) -> Model {
    Model {
        id: entry.id.clone(),
        aliases: Vec::new(),
        context_length: entry.context_length,
        max_completion_tokens: entry.max_completion_tokens,
        capabilities: if entry.capabilities.is_empty() {
            vec!["text".to_string()]
        } else {
            entry.capabilities.clone()
        },
        display_name: entry.display_name.clone(),
        created: entry.created,
        backends: vec![Backend {
            provider: provider.to_string(),
            upstream: entry.id.clone(),
        }],
        description: entry.description.clone(),
    }
}

fn fill_gaps(model: &mut Model, entry: &ListedModel) {
    if model.context_length == 0 {
        model.context_length = entry.context_length;
    }
    if model.max_completion_tokens == 0 {
        model.max_completion_tokens = entry.max_completion_tokens;
    }
    union_caps(&mut model.capabilities, &entry.capabilities);
    if model.display_name.is_none() {
        model.display_name = entry.display_name.clone();
    }
    if model.description.is_none() {
        model.description = entry.description.clone();
    }
    if model.created == DEFAULT_CREATED && entry.created != DEFAULT_CREATED {
        model.created = entry.created;
    }
}

/// Static config fills what no upstream list said. It does not overwrite a
/// number, name, or description the list already carried.
fn fill_from_static(model: &mut Model, configured: &Model) {
    if !configured.aliases.is_empty() {
        model.aliases = configured.aliases.clone();
    }
    if model.context_length == 0 {
        model.context_length = configured.context_length;
    }
    if model.max_completion_tokens == 0 {
        model.max_completion_tokens = configured.max_completion_tokens;
    }
    union_caps(&mut model.capabilities, &configured.capabilities);
    if model.display_name.is_none() {
        model.display_name = configured.display_name.clone();
    }
    if model.description.is_none() {
        model.description = configured.description.clone();
    }
    if model.created == DEFAULT_CREATED && configured.created != DEFAULT_CREATED {
        model.created = configured.created;
    }
    for backend in &configured.backends {
        push_backend(model, backend.clone());
    }
}

fn union_caps(have: &mut Vec<String>, extra: &[String]) {
    for cap in extra {
        if !have.iter().any(|existing| existing == cap) {
            have.push(cap.clone());
        }
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

fn first_u64(row: &Value, top: &[&str], nested: &[&str]) -> u64 {
    for key in top {
        if let Some(number) = number_of(row.get(*key)) {
            return number;
        }
    }
    let metadata = row.get("metadata");
    for key in nested {
        if let Some(number) = number_of(metadata.and_then(|value| value.get(*key))) {
            return number;
        }
    }
    0
}

fn number_of(value: Option<&Value>) -> Option<u64> {
    let value = value?;
    value.as_u64().or_else(|| {
        value
            .as_i64()
            .filter(|number| *number > 0)
            .map(|number| number as u64)
    })
}

fn non_empty_str(value: Option<&Value>) -> Option<String> {
    value
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_string)
}

fn capabilities_of(row: &Value) -> Vec<String> {
    let inputs = modalities(row, "input_modalities");
    let outputs = modalities(row, "output_modalities");
    let mut caps = Vec::new();
    let saw_modality = !inputs.is_empty() || !outputs.is_empty();
    if !saw_modality
        || inputs.iter().any(|mode| mode == "text")
        || outputs.iter().any(|mode| mode == "text")
    {
        caps.push("text".to_string());
    }
    if inputs.iter().any(|mode| mode == "image" || mode == "video") {
        caps.push("vision".to_string());
    }
    if supports_reasoning(row) {
        caps.push("reasoning".to_string());
    }
    caps
}

fn modalities(row: &Value, key: &str) -> Vec<String> {
    let from_architecture = row
        .get("architecture")
        .and_then(|value| value.get(key))
        .and_then(Value::as_array);
    let from_top = row.get(key).and_then(Value::as_array);
    from_architecture
        .or(from_top)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(|mode| mode.to_string())
                .collect()
        })
        .unwrap_or_default()
}

fn supports_reasoning(row: &Value) -> bool {
    if row
        .get("supports_reasoning_effort")
        .and_then(Value::as_bool)
        == Some(true)
    {
        return true;
    }
    row.get("effort")
        .and_then(|effort| effort.get("supported_levels"))
        .and_then(Value::as_array)
        .is_some_and(|levels| !levels.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn listed(id: &str) -> ListedModel {
        ListedModel {
            id: id.into(),
            context_length: 0,
            max_completion_tokens: 0,
            capabilities: vec!["text".into()],
            display_name: None,
            description: None,
            created: DEFAULT_CREATED,
        }
    }

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
        let ids: Vec<_> = parse_models(&body)
            .unwrap()
            .into_iter()
            .map(|model| model.id)
            .collect();
        assert_eq!(ids, vec!["a".to_string(), "b".to_string()]);
        assert!(parse_models(&json!({"models": []})).is_none());
        assert_eq!(parse_models(&json!({"data": []})).unwrap(), Vec::new());
    }

    #[test]
    fn reads_each_upstream_shape() {
        let open_design = json!({"data": [{
            "id": "mimo",
            "architecture": {"input_modalities": ["text", "image"], "output_modalities": ["text"]},
            "metadata": {"context_limit": 1000000, "output_budget": 32000}
        }]});
        let parsed = &parse_models(&open_design).unwrap()[0];
        assert_eq!(parsed.context_length, 1_000_000);
        assert_eq!(parsed.max_completion_tokens, 32_000);
        assert_eq!(
            parsed.capabilities,
            vec!["text".to_string(), "vision".to_string()]
        );

        let deepseek = json!({"data": [{
            "id": "deepseek-flash",
            "name": "DeepSeek-V4.1-Flash",
            "context_window": 1048576,
            "max_output_tokens": 393216,
            "input_modalities": ["text", "image"],
            "output_modalities": ["text"],
            "effort": {"supported_levels": ["low", "high"]}
        }]});
        let parsed = &parse_models(&deepseek).unwrap()[0];
        assert_eq!(parsed.context_length, 1_048_576);
        assert_eq!(parsed.max_completion_tokens, 393_216);
        assert_eq!(parsed.display_name.as_deref(), Some("DeepSeek-V4.1-Flash"));
        assert_eq!(
            parsed.capabilities,
            vec![
                "text".to_string(),
                "vision".to_string(),
                "reasoning".to_string()
            ]
        );

        let grok = json!({"data": [{
            "id": "grok-4.7",
            "name": "Grok 4.7",
            "description": "frontier",
            "context_window": 256000,
            "supports_reasoning_effort": true
        }]});
        let parsed = &parse_models(&grok).unwrap()[0];
        assert_eq!(parsed.context_length, 256_000);
        assert_eq!(parsed.display_name.as_deref(), Some("Grok 4.7"));
        assert_eq!(parsed.description.as_deref(), Some("frontier"));
        assert_eq!(
            parsed.capabilities,
            vec!["text".to_string(), "reasoning".to_string()]
        );

        let qwen = json!({"data": [{
            "id": "qwen3.6-flash",
            "object": "model",
            "owned_by": "system"
        }]});
        let parsed = &parse_models(&qwen).unwrap()[0];
        assert_eq!(parsed.context_length, 0);
        assert_eq!(parsed.capabilities, vec!["text".to_string()]);
        assert!(parsed.display_name.is_none());
    }

    #[test]
    fn same_id_on_two_providers_is_one_model() {
        let mut listings = HashMap::new();
        listings.insert(
            "open-design".into(),
            vec![listed("qwen3.6-flash"), listed("only-od")],
        );
        listings.insert("qwen".into(), vec![listed("qwen3.6-flash")]);
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
    fn a_later_listing_fills_a_gap_but_does_not_replace_upstream_metadata() {
        let mut first = listed("shared");
        first.context_length = 256_000;
        let mut second = listed("shared");
        second.context_length = 999;
        second.max_completion_tokens = 64_000;
        second.capabilities = vec!["text".into(), "vision".into()];
        let mut listings = HashMap::new();
        listings.insert("grok".into(), vec![first]);
        listings.insert("open-design".into(), vec![second]);
        let merged = merge(&[], &["grok".into(), "open-design".into()], &listings);
        assert_eq!(merged[0].context_length, 256_000);
        assert_eq!(merged[0].max_completion_tokens, 64_000);
        assert!(merged[0].capabilities.iter().any(|cap| cap == "vision"));
    }

    #[test]
    fn static_metadata_fills_gaps_and_a_differently_named_backend_is_kept() {
        let mut listings = HashMap::new();
        let mut upstream = listed("deepseek-v4.1-flash");
        upstream.context_length = 1_048_000;
        listings.insert("open-design".into(), vec![upstream]);
        let configured = static_model("deepseek-v4.1-flash", "deepseek", "deepseek-flash");
        let merged = merge(
            &[configured],
            &["open-design".into(), "deepseek".into()],
            &listings,
        );
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].context_length, 1_048_000);
        assert_eq!(merged[0].max_completion_tokens, 100);
        assert_eq!(merged[0].aliases, vec!["alias".to_string()]);
        assert!(merged[0].capabilities.iter().any(|cap| cap == "tools"));
        assert_eq!(merged[0].backends.len(), 2);
        assert_eq!(merged[0].backends[1].upstream, "deepseek-flash");
    }

    #[test]
    fn a_model_nobody_listed_stays() {
        let grok = static_model("grok-4.7", "grok", "grok-4.7");
        let mut listed_map = HashMap::new();
        listed_map.insert("open-design".into(), vec![listed("qwen3.6-flash")]);
        let merged = merge(&[grok], &["open-design".into(), "grok".into()], &listed_map);
        assert_eq!(merged.len(), 2);
        assert_eq!(merged[0].id, "qwen3.6-flash");
        assert_eq!(merged[1].id, "grok-4.7");
    }
}
