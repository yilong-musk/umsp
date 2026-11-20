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
        docs_url: None,
    },
    Vendor {
        id: "deepseek",
        name: "DeepSeek",
        base_url: "https://api.deepseek.com",
        anthropic_base_url: Some("https://api.deepseek.com/anthropic"),
        protocols: &["chat", "responses", "messages"],
        quirks: Some("deepseek"),
        docs_url: Some("https://api-docs.deepseek.com"),
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
        }
    }
}
