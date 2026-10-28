//! The mapping files Pulumi writes for Terraform providers.
//!
//! `pulumi package get-mapping terraform <provider>` writes a JSON document
//! whose `resources` object maps each Terraform resource type to an entry
//! whose `tok` is the Pulumi type token, such as `aws:ec2/vpc:Vpc`. With
//! the Pulumi engine the controller needs that token for every resource
//! type, under `engine.types`; [`load_mapping`] reads the tokens into
//! [`Settings::pulumi_types`](crate::Settings::pulumi_types).

use std::collections::BTreeMap;
use std::path::Path;

use serde::Deserialize;

use crate::error::Result;
use crate::terraform::read_json;

/// The part of a mapping file that is read. Everything else is ignored, so
/// any JSON object is a valid, if possibly empty, mapping.
#[derive(Deserialize)]
struct Mapping {
    #[serde(default)]
    resources: BTreeMap<String, Entry>,
}

#[derive(Deserialize)]
struct Entry {
    #[serde(default)]
    tok: Option<String>,
}

/// Reads a mapping file into a map from Terraform resource type to Pulumi
/// type token. Entries without a `tok` are left out.
///
/// Any JSON object is accepted: one without a `resources` field is an
/// empty mapping, not an error.
///
/// # Errors
///
/// Returns [`Error::Read`](crate::Error::Read) if the file cannot be read,
/// or [`Error::Json`](crate::Error::Json) if it is not a JSON object.
pub fn load_mapping(path: &Path) -> Result<BTreeMap<String, String>> {
    let mapping: Mapping = read_json(path)?;
    Ok(mapping
        .resources
        .into_iter()
        .filter_map(|(resource_type, entry)| entry.tok.map(|token| (resource_type, token)))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_are_read_and_entries_without_one_left_out() {
        let dir = std::env::temp_dir().join(format!("mtm-pulumi-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("mapping.json");
        std::fs::write(
            &path,
            r#"{"provider":{},"name":"x","resources":{"x_a":{"tok":"x:index/a:A","fields":{}},"x_b":{"fields":{}}}}"#,
        )
        .unwrap();
        let mapping = load_mapping(&path).unwrap();
        assert_eq!(mapping.len(), 1);
        assert_eq!(mapping["x_a"], "x:index/a:A");
        std::fs::write(&path, "not json").unwrap();
        assert!(load_mapping(&path).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
