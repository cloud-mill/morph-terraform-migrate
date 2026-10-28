//! A plan, as `terraform show -json <planfile>` writes it.
//!
//! Of a plan, the migration reads the configuration and the root variables.
//! The configuration is Terraform's own reading of the HCL: for each
//! resource, which attributes were set, and for each of those either the
//! constant it was set to or the references its expression makes.
//! [`Configuration::resolve`] follows those references through module
//! inputs and outputs to the resource an attribute refers to, with no need
//! to read HCL.

use std::collections::BTreeMap;
use std::path::Path;

use serde::Deserialize;
use serde_json::{Map, Value};

use crate::error::Result;
use crate::terraform::address::{Key, segments};
use crate::terraform::{check_format, read_json};

/// A plan file as JSON.
#[derive(Clone, Debug, Deserialize)]
pub struct Plan {
    /// The format's version. Only `1.x` is read.
    pub format_version: String,
    /// The root module's variables and their values.
    #[serde(default)]
    pub variables: BTreeMap<String, Variable>,
    /// The configuration the plan was made from.
    #[serde(default)]
    pub configuration: Option<Configuration>,
}

/// The value of one root module variable.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct Variable {
    /// The value.
    #[serde(default)]
    pub value: Value,
}

/// The configuration: the providers and the root module.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct Configuration {
    /// Each provider configuration, by an opaque key that resources name
    /// in their `provider_config_key`.
    #[serde(default)]
    pub provider_config: BTreeMap<String, ProviderConfig>,
    /// The root module.
    #[serde(default)]
    pub root_module: ModuleConfig,
}

/// One `provider` block.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct ProviderConfig {
    /// The provider's short name, such as `aws`.
    #[serde(default)]
    pub name: String,
    /// The provider's full name, such as
    /// `registry.terraform.io/hashicorp/aws`.
    #[serde(default)]
    pub full_name: Option<String>,
    /// The alias, for a provider configured more than once.
    #[serde(default)]
    pub alias: Option<String>,
    /// The module the block is in, for one declared below the root.
    #[serde(default)]
    pub module_address: Option<String>,
    /// The block's arguments, each an expression.
    #[serde(default)]
    pub expressions: Map<String, Value>,
}

/// The configuration of one module: its resources, the modules it calls
/// and its outputs.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct ModuleConfig {
    /// The `resource` and `data` blocks.
    #[serde(default)]
    pub resources: Vec<ResourceConfig>,
    /// The `module` blocks, by name.
    #[serde(default)]
    pub module_calls: BTreeMap<String, ModuleCall>,
    /// The `output` blocks, by name.
    #[serde(default)]
    pub outputs: BTreeMap<String, OutputConfig>,
}

/// One `module` block.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct ModuleCall {
    /// The module's source, as written.
    #[serde(default)]
    pub source: Option<String>,
    /// The arguments handed to the module, each an expression: the
    /// values of the module's variables.
    #[serde(default)]
    pub expressions: Map<String, Value>,
    /// The called module's own configuration.
    #[serde(default)]
    pub module: ModuleConfig,
}

/// One `output` block.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct OutputConfig {
    /// The output's expression.
    #[serde(default)]
    pub expression: Value,
    /// Whether the output is marked sensitive.
    #[serde(default)]
    pub sensitive: bool,
}

/// One `resource` or `data` block.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct ResourceConfig {
    /// The block's address in its module, such as `aws_subnet.private` or
    /// `data.aws_ami.latest`, without instance keys.
    #[serde(default)]
    pub address: String,
    /// `managed` or `data`.
    #[serde(default)]
    pub mode: String,
    /// The resource type.
    #[serde(rename = "type", default)]
    pub resource_type: String,
    /// The resource's name.
    #[serde(default)]
    pub name: String,
    /// The key of the provider configuration the block uses, in
    /// [`Configuration::provider_config`].
    #[serde(default)]
    pub provider_config_key: Option<String>,
    /// The block's arguments: an expression per attribute, and the nested
    /// blocks as objects or arrays of expressions.
    #[serde(default)]
    pub expressions: Map<String, Value>,
    /// The explicit `depends_on`.
    #[serde(default)]
    pub depends_on: Vec<String>,
}

/// An attribute's expression, as the plan's configuration describes it.
///
/// Terraform does not write the expression itself, only whether it was a
/// constant and, if not, which references it makes.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Expression<'a> {
    /// The constant the attribute was set to, if its expression makes no
    /// references.
    pub constant: Option<&'a Value>,
    /// The references the expression makes, each a dotted path such as
    /// `aws_vpc.main.id`. Terraform lists a multi-step reference together
    /// with each of its prefixes, so `aws_vpc.main` appears as well.
    pub references: Vec<&'a str>,
}

/// Reads `value` as an attribute's expression.
///
/// A value that is not an expression object, such as a nested block, is
/// read as an expression with neither constant nor references.
pub fn expression(value: &Value) -> Expression<'_> {
    let Some(object) = value.as_object() else {
        return Expression::default();
    };
    Expression {
        constant: object.get("constant_value"),
        references: object
            .get("references")
            .and_then(Value::as_array)
            .map(|items| items.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default(),
    }
}

/// Every reference anywhere inside `value`: in the expression itself, or
/// in any nested block however deep.
pub fn references_within(value: &Value) -> Vec<&str> {
    let mut found = Vec::new();
    collect_references(value, &mut found);
    found
}

fn collect_references<'a>(value: &'a Value, into: &mut Vec<&'a str>) {
    match value {
        Value::Object(fields) => {
            if let Some(items) = fields.get("references").and_then(Value::as_array) {
                into.extend(items.iter().filter_map(Value::as_str));
            }
            // A constant's own JSON may have a `references` key of its
            // own, so constants are not descended into.
            for (name, field) in fields {
                if name != "references" && name != "constant_value" {
                    collect_references(field, into);
                }
            }
        }
        Value::Array(items) => {
            for item in items {
                collect_references(item, into);
            }
        }
        _ => {}
    }
}

/// A resource block found in the configuration, with the module calls on
/// the way to it.
#[derive(Clone, Debug)]
pub struct Located<'a> {
    /// The modules from the root down to the one that holds the resource.
    /// The first is the root and the last holds the resource.
    pub modules: Vec<&'a ModuleConfig>,
    /// The calls that lead to the resource's module, outermost first, each
    /// with its name. Empty for a resource of the root.
    pub calls: Vec<(&'a str, &'a ModuleCall)>,
    /// The resource block.
    pub resource: &'a ResourceConfig,
}

/// A resource that a reference points at.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Target {
    /// The resource's address in the configuration, without instance
    /// keys, such as `module.notes.local_file.note`.
    pub config_path: String,
    /// The attribute the reference reads, if it names one.
    pub attribute: Option<String>,
    /// The instance key the reference names, if any.
    pub key: Option<Key>,
    /// The module depth of the target: how many module calls lead from the
    /// root to the module it is declared in.
    pub depth: usize,
    /// The module instance the reference goes through when it reads a
    /// module's output, such as `module.notes["a"]`, relative to the module
    /// the output was read from. Nested outputs are joined with `.`.
    pub module_instance: Option<String>,
}

/// What a set of references resolve to.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Resolution {
    /// The resources the references point at, each once.
    pub targets: Vec<Target>,
    /// Whether a reference could not be followed: through a `local`, an
    /// `each` or a `self`, or to something the configuration does not
    /// hold. A resource may hide behind it, so the caller should look for
    /// other evidence rather than conclude there is none.
    pub opaque: bool,
}

/// How many module inputs and outputs a chain of references is followed
/// through before the resolution is given up as opaque.
///
/// Terraform refuses a configuration whose module inputs and outputs form a
/// cycle, but a hand-written or corrupt plan could hold one, and the reader
/// must not recurse forever. Real configurations nest far shallower.
const DEPTH_LIMIT: usize = 16;

impl Plan {
    /// Reads a plan file.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Read`](crate::Error::Read) or
    /// [`Error::Json`](crate::Error::Json) if the file cannot be read as a
    /// plan, or [`Error::Format`](crate::Error::Format) if its format is
    /// not `1.x`.
    pub fn load(path: &Path) -> Result<Self> {
        let plan: Self = read_json(path)?;
        check_format(path, &plan.format_version)?;
        Ok(plan)
    }
}

impl Configuration {
    /// Finds the resource block at `config_path`, an address without
    /// instance keys such as `module.notes.local_file.note`.
    ///
    /// Returns `None` if the path cannot be parsed, if a module call on the
    /// way does not exist, or if the module holds no such block. Data
    /// sources are found as well as managed resources.
    pub fn locate(&self, config_path: &str) -> Option<Located<'_>> {
        let parts = segments(config_path).ok()?;
        let mut modules = vec![&self.root_module];
        let mut calls = Vec::new();
        let mut rest = parts.iter().peekable();
        while rest.peek().is_some_and(|part| part.name == "module") {
            rest.next();
            let name = rest.next()?;
            let module = *modules.last().expect("at least the root");
            let (name, call) = module.module_calls.get_key_value(&name.name)?;
            calls.push((name.as_str(), call));
            modules.push(&call.module);
        }
        let address: Vec<&str> = rest.map(|part| part.name.as_str()).collect();
        let address = address.join(".");
        let module = *modules.last().expect("at least the root");
        let resource = module
            .resources
            .iter()
            .find(|resource| resource.address == address)?;
        Some(Located {
            modules,
            calls,
            resource,
        })
    }

    /// Follows `references`, made by an expression of `located`'s block,
    /// to the managed resources they point at.
    ///
    /// A reference to a resource of the same module is a target at once. A
    /// `var` is followed to the expression the module was called with, in
    /// the calling module. A `module` output is followed into the called
    /// module's output expression. Anything else is not a resource, and
    /// some of it, such as a `local`, may hide one, which makes the
    /// resolution opaque. Each target is listed once, with the attribute
    /// name if any form of the reference gave one.
    pub fn resolve(&self, located: &Located<'_>, references: &[&str]) -> Resolution {
        let mut resolution = Resolution::default();
        let scope = Scope {
            modules: located.modules.clone(),
            calls: located.calls.clone(),
        };
        self.resolve_in(&scope, references, None, 0, &mut resolution);
        dedup(&mut resolution.targets);
        resolution
    }

    fn resolve_in(
        &self,
        scope: &Scope<'_>,
        references: &[&str],
        module_instance: Option<&str>,
        depth: usize,
        into: &mut Resolution,
    ) {
        if depth > DEPTH_LIMIT {
            into.opaque = true;
            return;
        }
        let module = *scope.modules.last().expect("at least the root");
        for reference in references {
            let Ok(parts) = segments(reference) else {
                into.opaque = true;
                continue;
            };
            match parts[0].name.as_str() {
                "var" => {
                    // A variable is followed up to the expression the
                    // enclosing module was called with.
                    let Some(name) = parts.get(1) else {
                        continue;
                    };
                    let Some((_, call)) = scope.calls.last() else {
                        // A root variable has a plain value.
                        continue;
                    };
                    let Some(argument) = call.expressions.get(&name.name) else {
                        // The module's default is a constant.
                        continue;
                    };
                    let parent = Scope {
                        modules: scope.modules[..scope.modules.len() - 1].to_vec(),
                        calls: scope.calls[..scope.calls.len() - 1].to_vec(),
                    };
                    let inner = references_within(argument);
                    self.resolve_in(&parent, &inner, None, depth + 1, into);
                }
                "module" => {
                    // A module output is followed down into the called
                    // module, remembering which instance of it was named.
                    let (Some(name), Some(output)) = (parts.get(1), parts.get(2)) else {
                        continue;
                    };
                    let Some((call_name, call)) = module.module_calls.get_key_value(&name.name)
                    else {
                        into.opaque = true;
                        continue;
                    };
                    let Some(declared) = call.module.outputs.get(&output.name) else {
                        into.opaque = true;
                        continue;
                    };
                    let mut child = scope.clone();
                    child.modules.push(&call.module);
                    child.calls.push((call_name.as_str(), call));
                    let mut instance = String::new();
                    if let Some(outer) = module_instance {
                        instance.push_str(outer);
                        instance.push('.');
                    }
                    instance.push_str("module.");
                    instance.push_str(call_name);
                    if let Some(key) = &name.key {
                        instance.push_str(&key.to_string());
                    }
                    let inner = references_within(&declared.expression);
                    self.resolve_in(&child, &inner, Some(&instance), depth + 1, into);
                }
                // These may stand for a resource, but the plan does not say
                // which: a local's expression is not written out, and
                // `each` and `self` differ per instance.
                "local" | "each" | "self" => into.opaque = true,
                // These never stand for a managed resource.
                "data" | "count" | "path" | "terraform" => {}
                resource_type => {
                    // Anything else should be a managed resource of this
                    // module. A name the module does not declare is treated
                    // as opaque rather than ignored, in case it hides one.
                    let Some(name) = parts.get(1) else {
                        continue;
                    };
                    let known = module.resources.iter().any(|resource| {
                        resource.mode == "managed"
                            && resource.resource_type == resource_type
                            && resource.name == name.name
                    });
                    if !known {
                        into.opaque = true;
                        continue;
                    }
                    let mut config_path = String::new();
                    for (call_name, _) in &scope.calls {
                        config_path.push_str("module.");
                        config_path.push_str(call_name);
                        config_path.push('.');
                    }
                    config_path.push_str(resource_type);
                    config_path.push('.');
                    config_path.push_str(&name.name);
                    into.targets.push(Target {
                        config_path,
                        attribute: parts.get(2).map(|part| part.name.clone()),
                        key: name.key.clone(),
                        depth: scope.calls.len(),
                        module_instance: module_instance.map(str::to_owned),
                    });
                }
            }
        }
    }
}

/// Where a reference is being resolved: the modules from the root down to
/// the current one, and the calls that lead there.
///
/// Following a `var` pops one level; following a module output pushes one.
#[derive(Clone)]
struct Scope<'a> {
    /// The modules from the root down. The last is the current one.
    modules: Vec<&'a ModuleConfig>,
    /// The calls that lead to the current module, each with its name.
    calls: Vec<(&'a str, &'a ModuleCall)>,
}

/// Keeps each resource once, taking the attribute from whichever form of
/// the reference names one, since Terraform lists a reference together
/// with each of its prefixes.
fn dedup(targets: &mut Vec<Target>) {
    let mut kept: Vec<Target> = Vec::new();
    for target in targets.drain(..) {
        if let Some(existing) = kept.iter_mut().find(|existing| {
            existing.config_path == target.config_path
                && existing.key == target.key
                && existing.module_instance == target.module_instance
        }) {
            if existing.attribute.is_none() {
                existing.attribute = target.attribute;
            }
        } else {
            kept.push(target);
        }
    }
    *targets = kept;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn configuration() -> Configuration {
        serde_json::from_value(serde_json::json!({
            "root_module": {
                "resources": [
                    {"address": "random_id.seed", "mode": "managed", "type": "random_id", "name": "seed",
                     "expressions": {"byte_length": {"constant_value": 4}, "prefix": {"references": ["var.prefix"]}}},
                    {"address": "local_file.copy", "mode": "managed", "type": "local_file", "name": "copy",
                     "expressions": {"content": {"references": ["local_file.readme.content", "local_file.readme"]},
                                     "filename": {"references": ["path.module"]},
                                     "extra": {"references": ["local.thing"]},
                                     "lost": {"references": ["ghost.x.id", "ghost.x"]}}},
                    {"address": "local_file.readme", "mode": "managed", "type": "local_file", "name": "readme",
                     "expressions": {"content": {"constant_value": "hi"}}},
                    {"address": "local_file.index", "mode": "managed", "type": "local_file", "name": "index",
                     "expressions": {"content": {"references": ["module.notes[\"a\"].note_id", "module.notes[\"a\"]"]},
                                     "other": {"references": ["module.notes[\"a\"].missing", "module.nope.x"]},
                                     "settings": [{"inner": {"references": ["random_id.seed.hex"]}}]}},
                    {"address": "data.local_file.back", "mode": "data", "type": "local_file", "name": "back"}
                ],
                "module_calls": {"notes": {
                    "source": "./notes",
                    "expressions": {"name": {"references": ["each.key"]}, "seed": {"references": ["random_id.seed.id", "random_id.seed"]}},
                    "module": {
                        "resources": [
                            {"address": "local_file.note", "mode": "managed", "type": "local_file", "name": "note",
                             "expressions": {"content": {"references": ["var.name", "var.seed"]}, "filename": {"references": ["var.missing"]}}},
                            {"address": "random_pet.pet", "mode": "managed", "type": "random_pet", "name": "pet",
                             "expressions": {"keepers": {"references": ["var.seed"]}}}
                        ],
                        "outputs": {"note_id": {"expression": {"references": ["local_file.note.id", "local_file.note"]}}}
                    }
                }}
            }
        }))
        .unwrap()
    }

    #[test]
    fn resources_are_located_through_module_calls() {
        let configuration = configuration();
        let copy = configuration.locate("local_file.copy").unwrap();
        assert_eq!(copy.resource.name, "copy");
        assert!(copy.calls.is_empty());
        assert_eq!(copy.modules.len(), 1);
        let note = configuration
            .locate("module.notes.local_file.note")
            .unwrap();
        assert_eq!(note.calls.len(), 1);
        assert_eq!(note.calls[0].0, "notes");
        assert_eq!(note.modules.len(), 2);
        assert!(
            configuration
                .locate("module.absent.local_file.note")
                .is_none()
        );
        assert!(configuration.locate("local_file.absent").is_none());
        assert!(configuration.locate("module.notes").is_none());
        assert!(configuration.locate("").is_none());
    }

    #[test]
    fn a_reference_in_the_same_module_names_its_attribute() {
        let configuration = configuration();
        let copy = configuration.locate("local_file.copy").unwrap();
        let content = expression(&copy.resource.expressions["content"]);
        assert_eq!(content.constant, None);
        let resolved = configuration.resolve(&copy, &content.references);
        assert!(!resolved.opaque);
        assert_eq!(
            resolved.targets,
            vec![Target {
                config_path: "local_file.readme".into(),
                attribute: Some("content".into()),
                key: None,
                depth: 0,
                module_instance: None,
            }]
        );
        let filename = expression(&copy.resource.expressions["filename"]);
        assert_eq!(
            configuration.resolve(&copy, &filename.references),
            Resolution::default()
        );
        let extra = expression(&copy.resource.expressions["extra"]);
        assert!(configuration.resolve(&copy, &extra.references).opaque);
        let lost = expression(&copy.resource.expressions["lost"]);
        let lost = configuration.resolve(&copy, &lost.references);
        assert!(lost.opaque && lost.targets.is_empty());
        let readme = configuration.locate("local_file.readme").unwrap();
        assert_eq!(
            expression(&readme.resource.expressions["content"]).constant,
            Some(&Value::from("hi"))
        );
        assert_eq!(expression(&Value::Null), Expression::default());
    }

    #[test]
    fn a_reference_through_a_module_output_or_input_reaches_the_resource() {
        let configuration = configuration();
        let index = configuration.locate("local_file.index").unwrap();
        let content = expression(&index.resource.expressions["content"]);
        let resolved = configuration.resolve(&index, &content.references);
        assert!(!resolved.opaque);
        assert_eq!(
            resolved.targets,
            vec![Target {
                config_path: "module.notes.local_file.note".into(),
                attribute: Some("id".into()),
                key: None,
                depth: 1,
                module_instance: Some("module.notes[\"a\"]".into()),
            }]
        );
        let other = expression(&index.resource.expressions["other"]);
        assert!(configuration.resolve(&index, &other.references).opaque);

        let note = configuration
            .locate("module.notes.local_file.note")
            .unwrap();
        let content = expression(&note.resource.expressions["content"]);
        let resolved = configuration.resolve(&note, &content.references);
        assert!(resolved.opaque, "each.key is opaque");
        assert_eq!(
            resolved.targets,
            vec![Target {
                config_path: "random_id.seed".into(),
                attribute: Some("id".into()),
                key: None,
                depth: 0,
                module_instance: None,
            }]
        );
        let filename = expression(&note.resource.expressions["filename"]);
        assert_eq!(
            configuration.resolve(&note, &filename.references),
            Resolution::default()
        );
        let inside = references_within(&index.resource.expressions["settings"]);
        assert_eq!(inside, vec!["random_id.seed.hex"]);
        assert!(references_within(&serde_json::json!(["x", 1, null, true])).is_empty());
        // A reference listed before its longer form still names the
        // attribute once both are seen.
        let reversed = configuration.resolve(
            &index,
            &["module.notes[\"a\"]", "module.notes[\"a\"].note_id"],
        );
        assert_eq!(reversed.targets.len(), 1);
        assert_eq!(reversed.targets[0].attribute.as_deref(), Some("id"));
        let copy = configuration.locate("local_file.copy").unwrap();
        let unseen =
            configuration.resolve(&copy, &["local_file.readme", "local_file.readme.content"]);
        assert_eq!(unseen.targets[0].attribute.as_deref(), Some("content"));
    }
}
