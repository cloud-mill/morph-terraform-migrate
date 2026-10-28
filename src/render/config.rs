//! The controller's configuration: `controller.toml`.
//!
//! The file is complete enough to parse and to validate, and marks what the
//! user has to fill in: the database URL and, for Pulumi, where the stacks
//! live. Each mapping names the import column, so that the engine takes
//! charge of the existing objects rather than creating them again, and
//! declares the references that a foreign key alone does not describe:
//! those that take an attribute other than `id`, and the companion columns
//! that set an attribute other rows give as a plain value. Nothing secret
//! is ever written here; the database password goes in the environment, as
//! the controller's own documentation says.

use std::fmt::Write as _;

use serde_json::Value;

use crate::model::{ADDRESS_COLUMN, EXPORTS, IMPORT_COLUMN, MODULE_COLUMN, Migration, NAME_COLUMN};
use crate::{EngineKind, Settings};

/// `text` as a TOML basic string, with control characters escaped.
pub fn toml_string(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for character in text.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            control if (control as u32) < 0x20 || control == '\u{7f}' => {
                let _ = write!(out, "\\u{:04X}", control as u32);
            }
            other => out.push(other),
        }
    }
    out.push('"');
    out
}

/// `name` as a TOML key: bare if it can be, quoted otherwise.
fn toml_key(name: &str) -> String {
    let bare = !name.is_empty()
        && name.chars().all(|character| {
            character.is_ascii_alphanumeric() || character == '_' || character == '-'
        });
    if bare {
        name.to_owned()
    } else {
        toml_string(name)
    }
}

/// A JSON value as an inline TOML value, by way of the `toml` crate's own
/// serialiser, or `None` for `null`, which TOML cannot express.
fn toml_value(value: &Value) -> Option<String> {
    if value.is_null() {
        return None;
    }
    toml::Value::try_from(value)
        .ok()
        .map(|value| value.to_string())
}

/// Renders `controller.toml`.
///
/// The sections, in order: `[postgres]` with a placeholder URL,
/// `[replication]`, one `[[mappings]]` per table with its lifecycle and its
/// declared references, the `[engine]` section for the chosen engine with
/// its providers or Pulumi types, and fixed `[reconcile]` and `[logging]`
/// defaults.
///
/// Secrets are never written: provider settings that are sensitive or look
/// secret were already left out by the analysis, and the database password
/// belongs in the environment.
pub fn controller_toml(migration: &Migration, settings: &Settings) -> String {
    let mut out = String::new();
    out.push_str("# controller.toml: configuration for morph-postgres-controller, made by\n");
    out.push_str("# morph-terraform-migrate from Terraform's state.\n");
    out.push_str("#\n");
    out.push_str("# Before use: set postgres.url, and read report.md. Nothing secret belongs\n");
    out.push_str("# in this file; the database password goes in the environment.\n\n");
    out.push_str("[postgres]\n");
    out.push_str("url = \"postgres://USER@HOST:5432/DATABASE\"\n\n");
    out.push_str("[replication]\n");
    let _ = writeln!(out, "slot = {}", toml_string(&settings.slot));
    let _ = writeln!(out, "publication = {}", toml_string(&settings.publication));
    out.push_str("create_publication = true\n\n");
    let public = settings.pg_schema == "public";

    for table in &migration.tables {
        let _ = writeln!(
            out,
            "# {}: {} row{}",
            table.resource_type,
            migration.rows_of(&table.resource_type).count(),
            if migration.rows_of(&table.resource_type).count() == 1 {
                ""
            } else {
                "s"
            }
        );
        out.push_str("[[mappings]]\n");
        if !public {
            let _ = writeln!(out, "schema = {}", toml_string(&settings.pg_schema));
        }
        let _ = writeln!(out, "table = {}", toml_string(&table.resource_type));
        let _ = writeln!(out, "resource_type = {}", toml_string(&table.resource_type));
        let _ = writeln!(out, "id_column = {}", toml_string(NAME_COLUMN));
        let _ = writeln!(out, "import_column = {}", toml_string(IMPORT_COLUMN));
        let _ = writeln!(out, "exclude_columns = [{}]", toml_string(ADDRESS_COLUMN));
        if settings.stacks_from_modules {
            let _ = writeln!(out, "stack_column = {}", toml_string(MODULE_COLUMN));
        }
        let _ = writeln!(out, "exports = {}", toml_string(EXPORTS));
        if settings.protect {
            out.push_str("\n# Adopted resources are protected: the engine refuses to delete or\n");
            out.push_str("# replace them until this is turned off.\n");
            out.push_str("[mappings.lifecycle]\n");
            out.push_str("prevent_destroy = true\n");
        }
        for column in &table.columns {
            let Some(reference) = &column.reference else {
                continue;
            };
            if reference.attribute == EXPORTS && reference.sets.is_none() {
                // A column that takes the exported `id` and sets nothing
                // else is fully described by its foreign key, which the
                // controller finds on its own; declaring it would be noise.
                continue;
            }
            out.push_str("\n[[mappings.references]]\n");
            let _ = writeln!(out, "column = {}", toml_string(&column.name));
            if !public {
                let _ = writeln!(out, "schema = {}", toml_string(&settings.pg_schema));
            }
            let _ = writeln!(out, "table = {}", toml_string(&reference.table));
            if reference.attribute != EXPORTS {
                let _ = writeln!(out, "attribute = {}", toml_string(&reference.attribute));
            }
            if let Some(sets) = &reference.sets {
                let _ = writeln!(out, "sets = {}", toml_string(sets));
            }
        }
        out.push('\n');
    }

    match settings.engine {
        EngineKind::Morph => {
            out.push_str("[engine]\n");
            out.push_str("kind = \"morph\"\n\n");
            out.push_str("[morph]\n");
            out.push_str("state = { backend = \"postgres\" }\n\n");
            for provider in &migration.providers {
                let _ = writeln!(out, "# {}", provider.full_name);
                out.push_str("[[providers]]\n");
                out.push_str("kind = \"terraform\"\n");
                let _ = writeln!(out, "source = {}", toml_string(&provider.source));
                out.push_str(
                    "# version = \"\"   # pin the version Terraform ran: terraform version\n",
                );
                if !provider.config.is_empty() {
                    out.push_str("\n[providers.config]\n");
                    for (key, value) in &provider.config {
                        if let Some(value) = toml_value(value) {
                            let _ = writeln!(out, "{} = {value}", toml_key(key));
                        }
                    }
                }
                out.push('\n');
            }
        }
        EngineKind::Pulumi => {
            out.push_str("[engine]\n");
            out.push_str("kind = \"pulumi\"\n");
            let _ = writeln!(out, "project = {}", toml_string(&settings.project));
            out.push_str("# backend = \"file:///var/lib/morph-controller/pulumi\"\n");
            out.push_str("# work_dir = \"/var/lib/morph-controller/pulumi-work\"\n");
            out.push_str("# passphrase_file = \"/etc/morph-controller/pulumi-passphrase\"\n\n");
            out.push_str("# The Pulumi type of each kind, from pulumi package get-mapping.\n");
            out.push_str("[engine.types]\n");
            for table in &migration.tables {
                match &table.pulumi_type {
                    Some(token) => {
                        let _ = writeln!(
                            out,
                            "{} = {}",
                            toml_key(&table.resource_type),
                            toml_string(token)
                        );
                    }
                    None => {
                        let _ = writeln!(
                            out,
                            "# {} = \"\"   # no mapping file names it; see report.md",
                            toml_key(&table.resource_type)
                        );
                    }
                }
            }
            // Pulumi takes provider settings as `<provider>:<key>` entries
            // of the engine configuration rather than per provider block.
            let mut config = Vec::new();
            for provider in &migration.providers {
                for (key, value) in &provider.config {
                    if let Some(value) = toml_value(value) {
                        config.push(format!(
                            "{} = {value}",
                            toml_string(&format!("{}:{key}", provider.name))
                        ));
                    }
                }
            }
            if !config.is_empty() {
                out.push_str("\n[engine.config]\n");
                for line in config {
                    out.push_str(&line);
                    out.push('\n');
                }
            }
            out.push('\n');
        }
    }
    out.push_str("[reconcile]\n");
    out.push_str("workers = 4\n");
    out.push_str("full_interval = \"5m\"\n\n");
    out.push_str("[logging]\n");
    out.push_str("level = \"info\"\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strings_keys_and_values_are_written_as_toml() {
        assert_eq!(toml_string("plain"), "\"plain\"");
        assert_eq!(toml_string("a\"b\\c\nd\te"), "\"a\\\"b\\\\c\\nd\\te\"");
        assert_eq!(toml_string("bell\u{7}"), "\"bell\\u0007\"");
        assert_eq!(toml_string("\r\u{8}\u{c}\u{7f}"), "\"\\r\\b\\f\\u007F\"");
        assert_eq!(toml_key("region"), "region");
        assert_eq!(toml_key("aws:region"), "\"aws:region\"");
        assert_eq!(toml_value(&Value::Null), None);
        assert_eq!(toml_value(&"x".into()).unwrap(), "\"x\"");
        assert_eq!(toml_value(&7.into()).unwrap(), "7");
        assert_eq!(toml_value(&true.into()).unwrap(), "true");
        assert_eq!(
            toml_value(&serde_json::json!(["a", 1])).unwrap(),
            "[\"a\", 1]"
        );
        assert_eq!(
            toml_value(&serde_json::json!({"k": "v"})).unwrap(),
            "{ k = \"v\" }"
        );
    }
}
