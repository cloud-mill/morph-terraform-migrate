//! The error type shared by the whole crate.
//!
//! Every failure is an [`Error`](enum@Error), and the first one stops the
//! migration: each variant means the files about to be written could not
//! be trusted, so none of them is written.

use std::path::PathBuf;

// `thiserror::Error` shares its name with the enum below, so intra-doc
// links to the enum are written `enum@Error` to disambiguate.
use thiserror::Error;

/// A reason the migration could not be made.
///
/// The `Display` form is a complete message for the user, which the binary
/// prints to stderr behind its own name.
#[derive(Debug, Error)]
pub enum Error {
    /// A file could not be read.
    #[error("{path}: {source}")]
    Read {
        /// The file that was being read.
        path: PathBuf,
        /// The underlying I/O error.
        #[source]
        source: std::io::Error,
    },
    /// A file is not valid JSON, or not JSON of the shape expected of it.
    #[error("{path}: {source}")]
    Json {
        /// The file that was being parsed.
        path: PathBuf,
        /// The deserialisation error, which names the offending position.
        #[source]
        source: serde_json::Error,
    },
    /// A file is JSON, but its `format_version` is not one this tool reads.
    ///
    /// Only the `1.x` versions of Terraform's state, plan and schema
    /// formats are supported.
    #[error("{path}: {reason}")]
    Format {
        /// The file.
        path: PathBuf,
        /// What is wrong with it.
        reason: String,
    },
    /// A resource address in the state could not be parsed.
    ///
    /// See [`Address::parse`](crate::terraform::address::Address::parse) for
    /// the accepted form.
    #[error("the address {address:?} cannot be read: {reason}")]
    Address {
        /// The address as Terraform wrote it.
        address: String,
        /// What is wrong with it.
        reason: String,
    },
    /// The provider schemas hold nothing for a resource type the state holds.
    ///
    /// Usually the schema file was produced in a different working
    /// directory, or with a different provider version, than the state.
    #[error(
        "no schema for {resource_type} of {provider}: run `terraform providers schema -json` in the same working directory as the state"
    )]
    Schema {
        /// The provider's full name, as the state gives it.
        provider: String,
        /// The resource type without a schema.
        resource_type: String,
    },
    /// A value cannot be written as an SQL literal.
    ///
    /// Only a NUL character causes this: PostgreSQL's `text` cannot hold
    /// one, and there is no faithful substitute.
    #[error("{address}: the attribute {attribute} cannot be written as SQL: {reason}")]
    Value {
        /// The address of the resource the value belongs to.
        address: String,
        /// The attribute, or control column, holding the value.
        attribute: String,
        /// Why it cannot be written.
        reason: String,
    },
    /// No managed resource is left to migrate.
    ///
    /// Either the state holds none, or the address filter
    /// ([`Settings::only`](crate::Settings::only)) admits none of them.
    #[error("nothing to migrate: {reason}")]
    Nothing {
        /// Why there is nothing.
        reason: String,
    },
    /// A file or directory could not be written.
    #[error("{path}: {source}")]
    Write {
        /// The file or directory.
        path: PathBuf,
        /// The underlying I/O error.
        #[source]
        source: std::io::Error,
    },
}

/// A result whose error is an [`Error`](enum@Error).
pub type Result<T> = std::result::Result<T, Error>;
