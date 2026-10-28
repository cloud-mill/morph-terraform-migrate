//! Provider schemas, as `terraform providers schema -json` writes them.
//!
//! The schema says, for every attribute of every resource type, what type
//! it has and who sets it: the user (`required` or `optional`), the
//! provider (`computed`), or either (`optional` and `computed`). That
//! decides whether an attribute becomes a column, and of what SQL type.

use std::collections::BTreeMap;
use std::path::Path;

use serde::Deserialize;
use serde_json::Value;

use crate::error::Result;
use crate::model::ColumnType;
use crate::terraform::{check_format, read_json};

/// The schemas of every provider in use.
#[derive(Clone, Debug, Deserialize)]
pub struct Schemas {
    /// The format's version. Only `1.x` is read.
    pub format_version: String,
    /// Each provider's schemas, by the provider's full name.
    #[serde(default)]
    pub provider_schemas: BTreeMap<String, ProviderSchema>,
}

/// The schemas of one provider.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct ProviderSchema {
    /// The schema of the provider's own configuration block.
    #[serde(default)]
    pub provider: Option<Schema>,
    /// The schema of each resource type, by type.
    #[serde(default)]
    pub resource_schemas: BTreeMap<String, Schema>,
}

/// The schema of one block type: a resource, a provider, or a nested
/// block.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct Schema {
    /// The schema's version, which the state's `schema_version` follows.
    #[serde(default)]
    pub version: u64,
    /// The block itself.
    #[serde(default)]
    pub block: Block,
}

/// The attributes and nested blocks of a block.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct Block {
    /// The attributes, by name.
    #[serde(default)]
    pub attributes: BTreeMap<String, Attribute>,
    /// The nested block types, by name.
    #[serde(default)]
    pub block_types: BTreeMap<String, BlockType>,
}

/// One attribute of a block.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct Attribute {
    /// The type: `"string"`, `"number"`, `"bool"`, `"dynamic"`, or a list
    /// such as `["list", "string"]` or `["object", {...}]`. Absent for an
    /// attribute with a `nested_type`.
    #[serde(rename = "type", default)]
    pub type_spec: Value,
    /// Whether the user has to set it.
    #[serde(default)]
    pub required: bool,
    /// Whether the user may set it.
    #[serde(default)]
    pub optional: bool,
    /// Whether the provider may set it.
    #[serde(default)]
    pub computed: bool,
    /// Whether its value is secret.
    #[serde(default)]
    pub sensitive: bool,
    /// Whether the provider has deprecated it.
    #[serde(default)]
    pub deprecated: bool,
    /// The structure of a nested attribute, which is a collection of
    /// objects with attributes of their own.
    #[serde(default)]
    pub nested_type: Option<Value>,
}

/// One nested block type of a block.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct BlockType {
    /// `single`, `list`, `set` or `map`.
    #[serde(default)]
    pub nesting_mode: String,
    /// The block's own schema.
    #[serde(default)]
    pub block: Block,
}

impl Schemas {
    /// Reads a schema file.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Read`](crate::Error::Read) or
    /// [`Error::Json`](crate::Error::Json) if the file cannot be read as
    /// schemas, or [`Error::Format`](crate::Error::Format) if its format is
    /// not `1.x`.
    pub fn load(path: &Path) -> Result<Self> {
        let schemas: Self = read_json(path)?;
        check_format(path, &schemas.format_version)?;
        Ok(schemas)
    }

    /// The schemas of the provider named `provider`, by full name as the
    /// state writes it.
    ///
    /// If no provider has exactly that name, one whose last two path
    /// segments match is accepted, so that schemas written by OpenTofu
    /// (`registry.opentofu.org/hashicorp/aws`) serve a state written by
    /// Terraform (`registry.terraform.io/hashicorp/aws`), and the reverse.
    pub fn provider(&self, provider: &str) -> Option<&ProviderSchema> {
        if let Some(found) = self.provider_schemas.get(provider) {
            return Some(found);
        }
        let wanted = tail(provider);
        self.provider_schemas
            .iter()
            .find(|(name, _)| tail(name) == wanted)
            .map(|(_, schemas)| schemas)
    }

    /// The schema of `resource_type` of `provider`, found as by
    /// [`Self::provider`].
    pub fn resource(&self, provider: &str, resource_type: &str) -> Option<&Schema> {
        self.provider(provider)?.resource_schemas.get(resource_type)
    }
}

/// The last two segments of a provider's full name: `hashicorp/aws` of
/// `registry.terraform.io/hashicorp/aws`. A name with fewer segments is
/// returned whole.
pub fn tail(provider: &str) -> String {
    let segments: Vec<&str> = provider.rsplit('/').take(2).collect();
    segments.into_iter().rev().collect::<Vec<_>>().join("/")
}

impl Attribute {
    /// The SQL type a column for this attribute has: `text`, `numeric` or
    /// `boolean` for the scalar types, and `jsonb` for every collection,
    /// object, nested or dynamic type.
    pub fn column_type(&self) -> ColumnType {
        match self.type_spec.as_str() {
            Some("string") => ColumnType::Text,
            Some("number") => ColumnType::Numeric,
            Some("bool") => ColumnType::Boolean,
            _ => ColumnType::Jsonb,
        }
    }

    /// Whether only the provider sets the attribute: `computed` without
    /// `optional` or `required`. Such attributes are never columns.
    pub fn is_computed_only(&self) -> bool {
        self.computed && !self.optional && !self.required
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn types_become_column_types() {
        let attribute = |spec: Value| Attribute {
            type_spec: spec,
            ..Attribute::default()
        };
        assert_eq!(attribute("string".into()).column_type(), ColumnType::Text);
        assert_eq!(
            attribute("number".into()).column_type(),
            ColumnType::Numeric
        );
        assert_eq!(attribute("bool".into()).column_type(), ColumnType::Boolean);
        assert_eq!(attribute("dynamic".into()).column_type(), ColumnType::Jsonb);
        assert_eq!(
            attribute(serde_json::json!(["list", "string"])).column_type(),
            ColumnType::Jsonb
        );
        assert_eq!(attribute(Value::Null).column_type(), ColumnType::Jsonb);
    }

    #[test]
    fn a_provider_is_found_by_its_full_name_or_its_tail() {
        let schemas: Schemas = serde_json::from_value(serde_json::json!({
            "format_version": "1.0",
            "provider_schemas": {
                "registry.terraform.io/hashicorp/local": {
                    "resource_schemas": {"local_file": {"version": 0, "block": {"attributes": {
                        "id": {"type": "string", "computed": true},
                        "filename": {"type": "string", "required": true}
                    }}}}
                }
            }
        }))
        .unwrap();
        assert!(
            schemas
                .provider("registry.terraform.io/hashicorp/local")
                .is_some()
        );
        assert!(
            schemas
                .provider("registry.opentofu.org/hashicorp/local")
                .is_some()
        );
        assert!(schemas.provider("hashicorp/local").is_some());
        assert!(schemas.provider("hashicorp/aws").is_none());
        let file = schemas.resource("hashicorp/local", "local_file").unwrap();
        assert!(file.block.attributes["id"].is_computed_only());
        assert!(!file.block.attributes["filename"].is_computed_only());
        assert!(schemas.resource("hashicorp/local", "local_dir").is_none());
        assert_eq!(tail("local"), "local");
    }
}
