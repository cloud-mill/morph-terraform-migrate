//! Readers for the JSON that Terraform writes about itself.
//!
//! Terraform describes its state, its plans and the schemas of its
//! providers in a documented JSON format. This module deserialises those
//! documents and nothing more: the HCL that produced them is never read,
//! since Terraform has already evaluated every module, variable, local and
//! expression by the time the documents exist.
//!
//! * [`state::State`] is `terraform show -json`: every resource instance
//!   with its actual values, its dependencies, and which values are
//!   sensitive.
//! * [`plan::Plan`] is `terraform show -json <planfile>`: the configuration
//!   as Terraform read it, which says which attributes were set and which
//!   of them refer to other resources, plus the values of the root
//!   variables.
//! * [`schema::Schemas`] is `terraform providers schema -json`: the type of
//!   every attribute, and whether the user or the provider sets it.
//! * [`address::Address`] parses the addresses the other three use to name
//!   resources.
//!
//! Only the fields the migration needs are deserialised, and unknown fields
//! are ignored, so documents from newer Terraform versions that add fields
//! still load.

use std::fs;
use std::path::Path;

use serde::de::DeserializeOwned;

use crate::error::{Error, Result};

pub mod address;
pub mod plan;
pub mod schema;
pub mod state;

/// Reads the file at `path` as JSON of type `T`.
///
/// # Errors
///
/// Returns [`Error::Read`] if the file cannot be read, or [`Error::Json`]
/// if it is not JSON that `T` accepts.
pub fn read_json<T: DeserializeOwned>(path: &Path) -> Result<T> {
    let text = fs::read_to_string(path).map_err(|source| Error::Read {
        path: path.to_owned(),
        source,
    })?;
    serde_json::from_str(&text).map_err(|source| Error::Json {
        path: path.to_owned(),
        source,
    })
}

/// Checks that `format_version` is one this tool reads.
///
/// Terraform's state, plan and schema formats are all at `1.x`, and a minor
/// version only adds fields, so any `1.` prefix is accepted. `path` names
/// the file in the error.
///
/// # Errors
///
/// Returns [`Error::Format`] for any other version.
pub fn check_format(path: &Path, format_version: &str) -> Result<()> {
    if format_version.starts_with("1.") {
        Ok(())
    } else {
        Err(Error::Format {
            path: path.to_owned(),
            reason: format!("format_version {format_version} is not 1.x"),
        })
    }
}
