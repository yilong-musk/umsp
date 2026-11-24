//! Comment-preserving config edits (`toml_edit`) with atomic replace.

use std::path::{Path, PathBuf};

use toml_edit::{Array, ArrayOfTables, DocumentMut, Item, Table, Value};

use crate::config::Config;
use crate::error::{Result, UmspError};

pub struct ConfigDoc {
    pub path: PathBuf,
    pub doc: DocumentMut,
}

impl ConfigDoc {
    pub fn load(path: &Path) -> Result<Self> {
        let raw = std::fs::read_to_string(path).map_err(|e| {
            UmspError::config(format!("cannot read config {}: {e}", path.display()))
        })?;
        let doc: DocumentMut = raw.parse().map_err(|e| {
            UmspError::config(format!("cannot parse config {}: {e}", path.display()))
        })?;
        Ok(Self {
            path: path.to_path_buf(),
            doc,
        })
    }

    pub fn ensure_table(&mut self, key: &str) -> &mut ArrayOfTables {
        if !self.doc.contains_key(key) {
            self.doc
                .insert(key, Item::ArrayOfTables(ArrayOfTables::new()));
        }
        self.doc[key]
            .as_array_of_tables_mut()
            .expect("just ensured array of tables")
    }

    pub fn ensure_root_table(&mut self, key: &str) -> &mut Table {
        if !self.doc.contains_key(key) {
            self.doc.insert(key, Item::Table(Table::new()));
        }
        self.doc[key].as_table_mut().expect("just ensured table")
    }

    /// Validate the edited document before writing it back.
    pub fn save(&self) -> Result<()> {
        let text = self.doc.to_string();
        let cfg: Config = toml::from_str(&text)
            .map_err(|e| UmspError::config(format!("edited config no longer parses: {e}")))?;
        let errors: Vec<String> = cfg
            .validate()
            .into_iter()
            .filter(|i| i.level == "error")
            .map(|i| format!("{}: {}", i.code, i.message))
            .collect();
        if !errors.is_empty() {
            return Err(UmspError::config(format!(
                "refusing to write invalid config: {}",
                errors.join("; ")
            )));
        }

        // Owner-only: the file holds client key hashes and prefixes.
        crate::util::atomic_write(&self.path, text.as_bytes(), 0o600)
            .map_err(|e| UmspError::config(format!("cannot write {}: {e}", self.path.display())))?;
        Ok(())
    }
}

pub fn str_item(value: &str) -> Item {
    Item::Value(Value::from(value))
}

pub fn int_item(value: i64) -> Item {
    Item::Value(Value::from(value))
}

pub fn str_array_item(values: &[String]) -> Item {
    let mut array = Array::new();
    for v in values {
        array.push(v.as_str());
    }
    Item::Value(Value::Array(array))
}

/// Remove a table from an array-of-tables by matching `field == wanted`.
/// Returns true when something was removed.
pub fn remove_from_table_array(
    doc: &mut DocumentMut,
    key: &str,
    field: &str,
    wanted: &str,
) -> bool {
    let Some(array) = doc.get_mut(key).and_then(Item::as_array_of_tables_mut) else {
        return false;
    };
    let index = array.iter().position(|table| {
        table
            .get(field)
            .and_then(Item::as_str)
            .is_some_and(|v| v == wanted)
    });
    match index {
        Some(index) => {
            array.remove(index);
            true
        }
        None => false,
    }
}

/// Set a string field inside an array-of-tables entry.
pub fn set_field_in_table_array(
    doc: &mut DocumentMut,
    key: &str,
    match_field: &str,
    match_value: &str,
    field: &str,
    value: Item,
) -> bool {
    let Some(array) = doc.get_mut(key).and_then(Item::as_array_of_tables_mut) else {
        return false;
    };
    for table in array.iter_mut() {
        let matches = table
            .get(match_field)
            .and_then(Item::as_str)
            .is_some_and(|v| v == match_value);
        if matches {
            table.insert(field, value);
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::CONFIG_TEMPLATE;

    fn temp_path(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("umsp-edit-{}-{tag}.toml", std::process::id()))
    }

    #[test]
    fn add_provider_preserves_comments_and_validates() {
        let path = temp_path("provider");
        std::fs::write(&path, CONFIG_TEMPLATE).unwrap();
        let mut doc = ConfigDoc::load(&path).unwrap();
        let mut table = Table::new();
        table.insert("id", str_item("openrouter"));
        table.insert("base_url", str_item("https://openrouter.ai/api/v1"));
        table.insert("api_key_env", str_item("OPENROUTER_API_KEY"));
        table.insert(
            "protocols",
            str_array_item(&["chat".to_string(), "messages".to_string()]),
        );
        doc.ensure_table("providers").push(table);
        doc.save().unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("umsp — AI Gateway configuration"));
        let cfg = Config::load(&path).unwrap();
        assert_eq!(cfg.providers.len(), 3);
        assert_eq!(cfg.providers[2].id, "openrouter");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn invalid_edit_is_rejected() {
        let path = temp_path("invalid");
        std::fs::write(&path, CONFIG_TEMPLATE).unwrap();
        let mut doc = ConfigDoc::load(&path).unwrap();
        let mut table = Table::new();
        table.insert("id", str_item("bad"));
        table.insert("base_url", str_item("https://x.test"));
        table.insert("api_key_env", str_item("K"));
        doc.ensure_table("providers").push(table);
        // Duplicate provider id is an error.
        let before = std::fs::read_to_string(&path).unwrap();
        let mut table2 = Table::new();
        table2.insert("id", str_item("open-design"));
        table2.insert("base_url", str_item("https://x.test"));
        table2.insert("api_key_env", str_item("K"));
        doc.ensure_table("providers").push(table2);
        assert!(doc.save().is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn removes_entries() {
        let path = temp_path("remove");
        std::fs::write(&path, CONFIG_TEMPLATE).unwrap();
        let mut doc = ConfigDoc::load(&path).unwrap();
        assert!(remove_from_table_array(
            &mut doc.doc,
            "models",
            "id",
            "deepseek-v4.1-flash"
        ));
        assert!(remove_from_table_array(
            &mut doc.doc,
            "models",
            "id",
            "claude-haiku-5.5"
        ));
        assert!(!remove_from_table_array(
            &mut doc.doc,
            "models",
            "id",
            "nope"
        ));
        doc.save().unwrap();
        let cfg = Config::load(&path).unwrap();
        assert!(cfg.models.is_empty());
        std::fs::remove_file(&path).ok();
    }
}
