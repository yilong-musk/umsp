//! Local model registry: canonical ids, aliases and `/v1/models` payload.

use serde_json::{Value, json};

use crate::config::{Config, Model};

/// Resolve a requested model id: canonical first, then aliases.
pub fn resolve<'a>(cfg: &'a Config, requested: &str) -> Option<&'a Model> {
    resolve_models(&cfg.models, requested)
}

/// Resolve against an explicit model slice (the live catalog, or config models).
pub fn resolve_models<'a>(models: &'a [Model], requested: &str) -> Option<&'a Model> {
    if let Some(model) = models.iter().find(|model| model.id == requested) {
        return Some(model);
    }
    models
        .iter()
        .find(|model| model.aliases.iter().any(|alias| alias == requested))
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
    let mut view = m.clone();
    crate::catalog::normalize_context(&mut view);
    json!({
        "id": m.id,
        "object": "model",
        "owned_by": "umsp",
        "created": m.created,
        "context_length": view.context_length,
        "context_lengths": view.context_lengths,
        "max_completion_tokens": m.max_completion_tokens,
        "capabilities": capabilities_object(&m.capabilities),
        "display_name": m.display_name(),
        "description": m.description.clone().unwrap_or_default(),
        "meta": model_meta(m, &view),
    })
}

/// Gateway metadata envelope for consumers that auto-fill model parameters
/// from the list itself (cc-switch reads entry-level `meta` / `info.meta`,
/// PR #9304). Derived only from the logical model card — never any provider
/// or backend data. The flat top-level fields stay byte-identical for
/// existing readers (Hermes reads `context_length` / `max_completion_tokens`
/// directly); `meta` restates them under the names those consumers parse.
///
/// Honesty rules: numbers are emitted only when published (media cards carry
/// no token context); `vision_source` is asserted only for `vision: true`,
/// so a `false` without a confirmed source keeps consumers fail-open instead
/// of closing the image path on an unconfirmed text-only guess.
fn model_meta(m: &Model, view: &Model) -> Value {
    let has = |cap: &str| m.capabilities.iter().any(|c| c == cap);
    let vision = has("vision");
    let mut meta = serde_json::Map::new();
    if view.context_length > 0 {
        meta.insert("context_length".into(), json!(view.context_length));
    }
    if m.max_completion_tokens > 0 {
        meta.insert("max_output_tokens".into(), json!(m.max_completion_tokens));
    }
    let name = m.display_name();
    if name != m.id {
        meta.insert("display_name".into(), json!(name));
    }
    meta.insert("reasoning".into(), json!(has("reasoning")));
    meta.insert("tool_call".into(), json!(has("tools")));
    let mut caps = serde_json::Map::new();
    caps.insert("vision".into(), json!(vision));
    if vision {
        caps.insert("vision_source".into(), json!("catalog"));
    }
    meta.insert("capabilities".into(), Value::Object(caps));
    if view.context_length > 0 {
        // Chat model: text in, text out; image/video join the input side when
        // the card declares them. Audio is left out: the capability flag does
        // not say which direction it flows.
        let mut input = vec![json!("text")];
        if vision {
            input.push(json!("image"));
        }
        if has("video") {
            input.push(json!("video"));
        }
        meta.insert("input_modalities".into(), Value::Array(input));
        meta.insert("output_modalities".into(), json!(["text"]));
    } else if has("image") {
        // Image-generation card: text in, image out, no published token
        // context. Audio-only cards (TTS/ASR/realtime) get no modality claim:
        // the flag alone cannot tell input from output.
        meta.insert("input_modalities".into(), json!(["text"]));
        meta.insert("output_modalities".into(), json!(["image"]));
    }
    Value::Object(meta)
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
    models_from(&cfg.models)
}

pub fn models_from(models: &[Model]) -> Value {
    let data: Vec<Value> = models.iter().map(model_payload).collect();
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
                context_lengths: Vec::new(),
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
        assert_eq!(model["context_lengths"], json!([1000]));
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

    fn card(id: &str, ctx: u64, max_out: u64, caps: &[&str], name: Option<&str>) -> Model {
        Model {
            id: id.into(),
            backends: vec![],
            aliases: vec![],
            context_length: ctx,
            context_lengths: Vec::new(),
            max_completion_tokens: max_out,
            capabilities: caps.iter().map(|s| s.to_string()).collect(),
            display_name: name.map(str::to_string),
            created: DEFAULT_CREATED,
            description: None,
        }
    }

    #[test]
    fn meta_chat_model_with_vision() {
        let m = card(
            "qwen4-max",
            1_000_000,
            131_072,
            &["text", "vision", "tools", "reasoning"],
            None,
        );
        let payload = model_payload(&m);
        let meta = &payload["meta"];
        assert_eq!(meta["context_length"], 1_000_000);
        assert_eq!(meta["max_output_tokens"], 131_072);
        assert_eq!(meta["reasoning"], true);
        assert_eq!(meta["tool_call"], true);
        assert_eq!(meta["capabilities"]["vision"], true);
        assert_eq!(meta["capabilities"]["vision_source"], "catalog");
        assert_eq!(meta["input_modalities"], json!(["text", "image"]));
        assert_eq!(meta["output_modalities"], json!(["text"]));
        // display_name falls back to the id; the envelope skips the redundancy.
        assert!(meta.get("display_name").is_none());
        // Flat fields stay for existing readers.
        assert_eq!(payload["context_length"], 1_000_000);
        assert_eq!(payload["max_completion_tokens"], 131_072);
        assert_eq!(payload["capabilities"]["vision"], true);
    }

    #[test]
    fn meta_text_only_chat_model_stays_fail_open() {
        let m = card("deepseek-chat", 128_000, 8_192, &["text", "tools"], None);
        let meta = model_payload(&m)["meta"].clone();
        assert_eq!(meta["capabilities"]["vision"], false);
        // No vision_source: consumers must not close the image path on an
        // unconfirmed text-only guess.
        assert!(meta["capabilities"].get("vision_source").is_none());
        assert_eq!(meta["input_modalities"], json!(["text"]));
        assert_eq!(meta["tool_call"], true);
        assert_eq!(meta["reasoning"], false);
    }

    #[test]
    fn meta_video_capability_joins_input_modalities() {
        let m = card(
            "omni-max",
            1_000_000,
            65_536,
            &["text", "vision", "video", "tools"],
            Some("Omni Max"),
        );
        let meta = model_payload(&m)["meta"].clone();
        assert_eq!(meta["input_modalities"], json!(["text", "image", "video"]));
        assert_eq!(meta["display_name"], "Omni Max");
    }

    #[test]
    fn meta_image_card_claims_modalities_but_no_context() {
        let m = card("qwen-image-3.0-pro", 0, 0, &["text", "image"], None);
        let meta = model_payload(&m)["meta"].clone();
        assert!(meta.get("context_length").is_none());
        assert!(meta.get("max_output_tokens").is_none());
        assert_eq!(meta["input_modalities"], json!(["text"]));
        assert_eq!(meta["output_modalities"], json!(["image"]));
    }

    #[test]
    fn meta_audio_card_claims_no_modalities() {
        // TTS vs ASR cannot be told apart from the capability flag alone.
        let m = card("qwen-audio-3.0-tts-plus", 0, 0, &["text", "audio"], None);
        let meta = model_payload(&m)["meta"].clone();
        assert!(meta.get("input_modalities").is_none());
        assert!(meta.get("output_modalities").is_none());
        assert!(meta.get("context_length").is_none());
    }

    #[test]
    fn meta_never_leaks_provider_data() {
        let c = cfg();
        let payload = models_payload(&c);
        let meta = &payload["data"][0]["meta"];
        for key in ["provider", "upstream", "upstream_model", "backends"] {
            assert!(meta.get(key).is_none(), "meta must not expose {key}");
        }
        assert_eq!(meta["context_length"], 1000);
        assert_eq!(meta["max_output_tokens"], 100);
        assert_eq!(meta["display_name"], "Model One");
    }
}
