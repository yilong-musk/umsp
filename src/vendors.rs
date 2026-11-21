//! Built-in vendor catalogue.
//!
//! A provider entry is an *instance*: it is created from one of these vendors
//! and stays independent (own id, own key variable, own circuit) even when the
//! same vendor is added several times. Only vendors listed here can be added;
//! supporting a new one is a code change in this file.

pub struct Vendor {
    /// Stable id used by `umsp provider add --vendor` and stored on the entry.
    pub id: &'static str,
    /// Human-readable name.
    pub name: &'static str,
    /// OpenAI-compatible base URL.
    pub base_url: &'static str,
    /// Separate Messages endpoint, when the vendor documents one.
    pub anthropic_base_url: Option<&'static str>,
    /// Protocols this vendor actually serves.
    pub protocols: &'static [&'static str],
    /// Request/response quirks preset (`opendesign`, `deepseek`, …).
    pub quirks: Option<&'static str>,
    /// `api_key` reads `api_key_env`. `oauth` is a per-provider Grok subscription
    /// session (`umsp login <id>`), independent of every other provider.
    pub auth: &'static str,
    /// Official documentation, when known.
    pub docs_url: Option<&'static str>,
}

pub const VENDORS: &[Vendor] = &[
    Vendor {
        id: "open-design",
        name: "OpenDesign",
        base_url: "https://amr-link.open-design.ai/v1",
        anthropic_base_url: None,
        protocols: &["chat", "responses", "messages"],
        quirks: Some("opendesign"),
        auth: "api_key",
        docs_url: None,
    },
    Vendor {
        id: "deepseek",
        name: "DeepSeek",
        base_url: "https://api.deepseek.com",
        anthropic_base_url: Some("https://api.deepseek.com/anthropic"),
        protocols: &["chat", "responses", "messages"],
        quirks: Some("deepseek"),
        auth: "api_key",
        docs_url: Some("https://api-docs.deepseek.com"),
    },
    Vendor {
        id: "grok",
        name: "Grok",
        base_url: "https://cli-chat-proxy.grok.com/v1",
        anthropic_base_url: None,
        protocols: &["responses"],
        quirks: None,
        auth: "oauth",
        docs_url: None,
    },
    Vendor {
        id: "qwen-token-plan",
        name: "Qwen Token Plan",
        base_url: "https://token-plan.maas.qianwenaiapi.com/compatible-mode/v1",
        anthropic_base_url: Some("https://token-plan.maas.qianwenaiapi.com/apps/anthropic"),
        protocols: &["chat", "messages"],
        quirks: None,
        auth: "api_key",
        docs_url: Some("https://help.aliyun.com/zh/model-studio/base-url"),
    },
];

pub fn find(id: &str) -> Option<&'static Vendor> {
    VENDORS.iter().find(|vendor| vendor.id == id)
}

pub fn ids() -> Vec<&'static str> {
    VENDORS.iter().map(|vendor| vendor.id).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lookups_are_exact() {
        assert_eq!(find("deepseek").map(|v| v.id), Some("deepseek"));
        assert!(find("DeepSeek").is_none(), "ids are case-sensitive");
        assert!(find("openrouter").is_none());
    }

    #[test]
    fn catalogue_is_consistent() {
        for vendor in VENDORS {
            assert!(!vendor.id.is_empty());
            assert!(vendor.base_url.starts_with("https://"));
            assert!(!vendor.protocols.is_empty());
            for protocol in vendor.protocols {
                assert!(crate::config::PROTOCOLS.contains(protocol));
            }
            if let Some(quirks) = vendor.quirks {
                assert!(["opendesign", "deepseek"].contains(&quirks));
            }
            assert!(["api_key", "oauth"].contains(&vendor.auth));
        }
    }

    #[test]
    fn qwen_token_plan_uses_the_console_host_not_payg() {
        let vendor = find("qwen-token-plan").expect("catalog entry");
        assert_eq!(
            vendor.base_url,
            "https://token-plan.maas.qianwenaiapi.com/compatible-mode/v1"
        );
        assert_eq!(
            vendor.anthropic_base_url,
            Some("https://token-plan.maas.qianwenaiapi.com/apps/anthropic")
        );
        assert_eq!(vendor.protocols, &["chat", "messages"]);
        assert!(vendor.quirks.is_none());
        assert_eq!(vendor.auth, "api_key");
    }
}
