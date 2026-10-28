//! The state, as `terraform show -json` writes it.
//!
//! This is the "values representation" of Terraform's JSON output: the root
//! module with its resource instances, and its child module instances
//! recursively. Only the fields the migration needs are deserialised.

use std::path::Path;

use serde::Deserialize;
use serde_json::{Map, Value};

use crate::error::Result;
use crate::terraform::{check_format, read_json};

/// A state file as JSON.
#[derive(Clone, Debug, Deserialize)]
pub struct State {
    /// The format's version. Only `1.x` is read.
    pub format_version: String,
    /// The version of Terraform that wrote the state.
    #[serde(default)]
    pub terraform_version: Option<String>,
    /// The values. Absent when the state is empty.
    #[serde(default)]
    pub values: Option<Values>,
}

/// The values of a state: its root module.
#[derive(Clone, Debug, Deserialize)]
pub struct Values {
    /// The root module, which holds every other module.
    pub root_module: Module,
}

/// A module instance with its resources and its child modules.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct Module {
    /// The module instance's address. Absent for the root.
    #[serde(default)]
    pub address: Option<String>,
    /// The resource instances directly in this module.
    #[serde(default)]
    pub resources: Vec<Instance>,
    /// The module instances called from this one.
    #[serde(default)]
    pub child_modules: Vec<Module>,
}

/// One resource instance: a managed resource, or a data source that was
/// read.
#[derive(Clone, Debug, Deserialize)]
pub struct Instance {
    /// The instance's address, such as `module.vpc.aws_subnet.private[0]`.
    pub address: String,
    /// `managed` or `data`.
    pub mode: String,
    /// The resource type, such as `aws_subnet`.
    #[serde(rename = "type")]
    pub resource_type: String,
    /// The resource's name in its module.
    pub name: String,
    /// The instance key, a number or text, if the resource has `count` or
    /// `for_each`.
    #[serde(default)]
    pub index: Option<Value>,
    /// The provider's full name, such as
    /// `registry.terraform.io/hashicorp/aws`.
    pub provider_name: String,
    /// The version of the provider's schema the values follow.
    #[serde(default)]
    pub schema_version: u64,
    /// The attributes as Terraform last saw them: what the configuration
    /// set, the provider's defaults and what it computed, all in one object
    /// with nothing to tell them apart. The plan's configuration, or
    /// failing that the schema, is what tells them apart.
    #[serde(default)]
    pub values: Map<String, Value>,
    /// Which values are sensitive: an object mirroring the shape of
    /// `values`, with `true` where a value is sensitive. A collection
    /// holding nothing sensitive appears as an empty object or array.
    #[serde(default)]
    pub sensitive_values: Value,
    /// The resources this instance depends on, by their addresses in the
    /// configuration, without instance keys. Terraform records both the
    /// references in the configuration and explicit `depends_on`, and
    /// includes what those depend on in turn.
    #[serde(default)]
    pub depends_on: Vec<String>,
}

impl State {
    /// Reads a state file.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Read`](crate::Error::Read) or
    /// [`Error::Json`](crate::Error::Json) if the file cannot be read as a
    /// state, or [`Error::Format`](crate::Error::Format) if its format is
    /// not `1.x`.
    pub fn load(path: &Path) -> Result<Self> {
        let state: Self = read_json(path)?;
        check_format(path, &state.format_version)?;
        Ok(state)
    }

    /// Every resource instance in the state, in document order: the root
    /// module's first, then each child module's, depth first.
    pub fn instances(&self) -> Vec<&Instance> {
        let mut found = Vec::new();
        if let Some(values) = &self.values {
            collect(&values.root_module, &mut found);
        }
        found
    }
}

fn collect<'a>(module: &'a Module, into: &mut Vec<&'a Instance>) {
    into.extend(module.resources.iter());
    for child in &module.child_modules {
        collect(child, into);
    }
}

impl Instance {
    /// Whether the instance is a managed resource rather than a data
    /// source.
    pub fn is_managed(&self) -> bool {
        self.mode == "managed"
    }

    /// Whether `attribute` holds anything sensitive: it is marked `true`
    /// itself, or it is a collection with a sensitive value somewhere
    /// inside.
    pub fn is_sensitive(&self, attribute: &str) -> bool {
        self.sensitive_values
            .get(attribute)
            .is_some_and(contains_true)
    }
}

fn contains_true(value: &Value) -> bool {
    match value {
        Value::Bool(flag) => *flag,
        Value::Array(items) => items.iter().any(contains_true),
        Value::Object(fields) => fields.values().any(contains_true),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn instance(sensitive: Value) -> Instance {
        serde_json::from_value(serde_json::json!({
            "address": "x.y",
            "mode": "managed",
            "type": "x",
            "name": "y",
            "provider_name": "registry.terraform.io/hashicorp/x",
            "values": {},
            "sensitive_values": sensitive,
        }))
        .unwrap()
    }

    #[test]
    fn sensitivity_is_found_at_the_top_and_inside_collections() {
        let instance = instance(serde_json::json!({
            "password": true,
            "tags": {},
            "settings": [{"secret": true}],
            "ports": [false, false],
            "count": 3,
        }));
        assert!(!instance.is_sensitive("count"));
        assert!(instance.is_sensitive("password"));
        assert!(!instance.is_sensitive("tags"));
        assert!(instance.is_sensitive("settings"));
        assert!(!instance.is_sensitive("ports"));
        assert!(!instance.is_sensitive("absent"));
        assert!(instance.is_managed());
        assert!(instance.depends_on.is_empty());
    }

    #[test]
    fn instances_come_in_document_order_across_modules() {
        let state: State = serde_json::from_value(serde_json::json!({
            "format_version": "1.0",
            "values": {"root_module": {
                "resources": [{"address": "a.b", "mode": "managed", "type": "a", "name": "b", "provider_name": "p"}],
                "child_modules": [
                    {"address": "module.m", "resources": [{"address": "module.m.c.d", "mode": "data", "type": "c", "name": "d", "provider_name": "p"}],
                     "child_modules": [{"address": "module.m.module.n", "resources": [{"address": "module.m.module.n.e.f", "mode": "managed", "type": "e", "name": "f", "provider_name": "p"}]}]}
                ]
            }}
        }))
        .unwrap();
        let addresses: Vec<&str> = state
            .instances()
            .iter()
            .map(|instance| instance.address.as_str())
            .collect();
        assert_eq!(addresses, ["a.b", "module.m.c.d", "module.m.module.n.e.f"]);
        assert!(!state.instances()[1].is_managed());

        let empty: State =
            serde_json::from_value(serde_json::json!({"format_version": "1.0"})).unwrap();
        assert!(empty.instances().is_empty());
    }
}
