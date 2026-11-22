//! Documented metadata used only when an upstream model list left a field empty.
//!
//! Numbers and capabilities come from the vendor's own model card. A `0` context
//! or output limit means the card does not publish one, so the field stays empty.
//! Vision, image, video, and audio are applied only when the list did not declare
//! modalities. Reasoning is applied only when the list did not declare it.
//! Tool support is not a modality; it is filled when the card says function
//! calling and the list said nothing about tools.

use crate::config::Model;

struct Spec {
    id: &'static str,
    context_length: u64,
    max_completion_tokens: u64,
    capabilities: &'static [&'static str],
    description: Option<&'static str>,
}

const K64: u64 = 64 * 1024;
const K128: u64 = 128 * 1024;
const K384: u64 = 384 * 1024;
const MILLION: u64 = 1_000_000;

/// Sources, checked 2026-11-22:
/// - Qwen text limits: QwenCloud "Text generation and visual understanding"
///   (`qwen4-max` output 131,072; `qwen4-max-preview` Token Plan output 393,216;
///   Qwen3.8 output 128K; Qwen3.7 / Qwen3.6 output 64K; DeepSeek V4 output 384K).
/// - Token Plan capabilities (vision or text-only): Alibaba Cloud Token Plan team overview.
/// - GLM-5.3 / GLM-5.2: Z.ai model cards, text only, 1M context, 128K output.
/// - `decision-model-preview`: Model Studio card, context 65,536, max output 0, text only.
/// - Image, video, and audio cards publish no token context. Capabilities name the
///   modality; `text` stays because the caller still sends a text prompt or receives text.
/// - Grok 4.5 / 4.6 / 4.7: SpaceXSI docs, 500k context, text and image in, tools,
///   no published output cap. The CLI proxy's own `context_window` still wins.
const SPECS: &[Spec] = &[
    spec(
        "qwen4-max",
        MILLION,
        131_072,
        &["text", "vision", "reasoning", "tools"],
        None,
    ),
    spec(
        "qwen4-max-preview",
        MILLION,
        393_216,
        &["text", "vision", "reasoning", "tools"],
        None,
    ),
    spec(
        "qwen3.8-max",
        MILLION,
        K128,
        &["text", "vision", "reasoning", "tools"],
        None,
    ),
    spec(
        "qwen3.8-flash",
        MILLION,
        K128,
        &["text", "vision", "reasoning", "tools"],
        None,
    ),
    spec(
        "qwen3.7-max",
        MILLION,
        K64,
        &["text", "reasoning", "tools"],
        None,
    ),
    spec(
        "qwen3.7-plus",
        MILLION,
        K64,
        &["text", "vision", "reasoning", "tools"],
        None,
    ),
    spec(
        "qwen3.6-flash",
        MILLION,
        K64,
        &["text", "vision", "reasoning", "tools"],
        None,
    ),
    spec(
        "deepseek-v4.1-flash",
        MILLION,
        K384,
        &["text", "vision", "reasoning", "tools"],
        None,
    ),
    spec(
        "deepseek-v4.1-flash-1017",
        MILLION,
        K384,
        &["text", "vision", "reasoning", "tools"],
        None,
    ),
    spec(
        "deepseek-v4-pro",
        MILLION,
        K384,
        &["text", "reasoning", "tools"],
        None,
    ),
    spec(
        "deepseek-v4-pro-0813",
        MILLION,
        K384,
        &["text", "reasoning", "tools"],
        None,
    ),
    spec(
        "deepseek-v4-flash-0731",
        MILLION,
        K384,
        &["text", "reasoning", "tools"],
        None,
    ),
    spec(
        "glm-5.3",
        MILLION,
        K128,
        &["text", "reasoning", "tools"],
        None,
    ),
    spec(
        "glm-5.2",
        MILLION,
        K128,
        &["text", "reasoning", "tools"],
        None,
    ),
    spec(
        "decision-model-preview",
        65_536,
        0,
        &["text"],
        Some(
            "Structured text decisions. The card publishes a 65536 context and no completion budget.",
        ),
    ),
    spec(
        "auto",
        0,
        0,
        &["text", "reasoning"],
        Some(
            "Token Plan router. It picks an underlying model per request and does not accept tool_choice.",
        ),
    ),
    spec(
        "qwen-image-3.0-pro",
        0,
        0,
        &["text", "image"],
        Some("Image generation. The card publishes no token context."),
    ),
    spec(
        "wan2.7-image",
        0,
        0,
        &["text", "image"],
        Some("Image generation. The card publishes no token context."),
    ),
    spec(
        "wan2.7-image-pro",
        0,
        0,
        &["text", "image"],
        Some("Image generation. The card publishes no token context."),
    ),
    spec(
        "wan3.0-video",
        0,
        0,
        &["text", "image", "video"],
        Some("Video generation. The card publishes no token context."),
    ),
    spec(
        "qwen-audio-3.0-asr-flash",
        0,
        0,
        &["text", "audio"],
        Some("Speech recognition. Audio in, text out. The card publishes no token context."),
    ),
    spec(
        "qwen-audio-3.0-tts-plus",
        0,
        0,
        &["text", "audio"],
        Some("Speech synthesis. Text in, audio out. The card publishes no token context."),
    ),
    spec(
        "qwen-audio-3.0-realtime-plus",
        0,
        0,
        &["text", "audio"],
        Some("Realtime speech conversation. The card publishes no token context."),
    ),
    spec(
        "grok-4.7",
        500_000,
        0,
        &["text", "vision", "reasoning", "tools"],
        None,
    ),
    spec(
        "grok-4.6",
        500_000,
        0,
        &["text", "vision", "reasoning", "tools"],
        None,
    ),
    spec(
        "grok-4.5",
        500_000,
        0,
        &["text", "vision", "reasoning", "tools"],
        None,
    ),
    spec(
        "grok-4.7-build-fast",
        500_000,
        0,
        &["text", "vision", "reasoning", "tools"],
        None,
    ),
];

const fn spec(
    id: &'static str,
    context_length: u64,
    max_completion_tokens: u64,
    capabilities: &'static [&'static str],
    description: Option<&'static str>,
) -> Spec {
    Spec {
        id,
        context_length,
        max_completion_tokens,
        capabilities,
        description,
    }
}

pub fn apply(model: &mut Model, modalities_known: bool, reasoning_known: bool) {
    let Some(spec) = SPECS.iter().find(|spec| spec.id == model.id) else {
        return;
    };
    if model.context_length == 0 {
        model.context_length = spec.context_length;
    }
    if model.max_completion_tokens == 0 {
        model.max_completion_tokens = spec.max_completion_tokens;
    }
    if model.description.is_none() {
        model.description = spec.description.map(str::to_string);
    }
    for cap in spec.capabilities {
        let allow = match *cap {
            "vision" | "image" | "video" | "audio" => !modalities_known,
            "reasoning" => !reasoning_known,
            _ => true,
        };
        if allow && !model.capabilities.iter().any(|have| have == *cap) {
            model.capabilities.push((*cap).to_string());
        }
    }
}
