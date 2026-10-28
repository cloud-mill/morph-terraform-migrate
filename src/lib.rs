//! Migrates resources managed by Terraform into tables and rows for the
//! Morph PostgreSQL controller.
//!
//! The crate reads only the JSON that Terraform itself writes: the state
//! (`terraform show -json`), the provider schemas (`terraform providers
//! schema -json`) and, optionally, a plan (`terraform show -json <planfile>`).
//! From those it produces four files:
//!
//! * `schema.sql` creates one table per resource type.
//! * `rows.sql` upserts one row per resource instance.
//! * `controller.toml` configures the controller to reconcile the rows.
//! * `report.md` lists every decision the user should review.
//!
//! HCL is never parsed. Terraform has already evaluated every module,
//! variable and expression by the time the state exists, so its JSON holds
//! the flat truth about each resource, and a second evaluator would only be
//! a worse Terraform. Nothing is run against a database or a cloud: the
//! user reviews the report, loads the SQL and starts the controller, which
//! imports each object by the identifier its row carries instead of
//! creating it again.
//!
//! # Architecture
//!
//! The pipeline has three stages, one module each:
//!
//! 1. [`terraform`] deserialises the JSON documents.
//! 2. [`analyse`] turns them into a [`model::Migration`]: which attributes
//!    become columns, what each row is named, which values are references
//!    to other rows, and in what order the rows can be loaded. Every
//!    decision a user may want to revisit is recorded in a
//!    [`model::Report`].
//! 3. [`render`] serialises the migration as SQL, TOML and Markdown without
//!    deciding anything further.
//!
//! [`migrate`] runs the analysis and the renderers in memory;
//! [`Inputs::load`] and [`Output::write_to`] do the file I/O on either side
//! of it. The binary of the same name is a thin command-line wrapper around
//! these three.
//!
//! # Examples
//!
//! ```no_run
//! use std::path::Path;
//!
//! use morph_terraform_migrate::{Inputs, Settings, migrate};
//!
//! # fn example() -> morph_terraform_migrate::Result<()> {
//! let inputs = Inputs::load(
//!     Path::new("state.json"),
//!     Path::new("schema.json"),
//!     Some(Path::new("plan.json")),
//! )?;
//! let output = migrate(&inputs, &Settings::default())?;
//! output.write_to(Path::new("out"))?;
//! println!("{} rows in {} tables", output.migration.rows.len(), output.migration.tables.len());
//! # Ok(())
//! # }
//! ```

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

pub mod analyse;
pub mod error;
pub mod model;
pub mod names;
pub mod pulumi;
pub mod render;
pub mod terraform;

pub use error::{Error, Result};
use model::Migration;
use terraform::plan::Plan;
use terraform::schema::Schemas;
use terraform::state::State;

/// File name of the SQL that creates the tables.
pub const SCHEMA_FILE: &str = "schema.sql";
/// File name of the SQL that loads the rows.
pub const ROWS_FILE: &str = "rows.sql";
/// File name of the controller's configuration.
pub const CONFIG_FILE: &str = "controller.toml";
/// File name of the Markdown report.
pub const REPORT_FILE: &str = "report.md";

/// The engine the controller will reconcile with.
///
/// The choice decides how `controller.toml` describes providers: as
/// `[[providers]]` blocks for Morph, or as Pulumi type tokens under
/// `[engine.types]` for Pulumi.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum EngineKind {
    /// Morph as a library, driving providers written for Terraform.
    #[default]
    Morph,
    /// The Pulumi command line, with a Pulumi type token per resource type
    /// (see [`Settings::pulumi_types`]).
    Pulumi,
}

/// Options that shape a migration.
///
/// [`Settings::default`] matches the binary's defaults: the Morph engine,
/// the `public` schema, everything managed, no stacks, no sensitive
/// columns, `prevent_destroy` on every mapping, and no guessed references.
#[derive(Clone, Debug)]
pub struct Settings {
    /// The engine the configuration is written for.
    pub engine: EngineKind,
    /// The PostgreSQL schema the tables are created in.
    pub pg_schema: String,
    /// Address prefixes selecting what to migrate, such as `module.vpc` or
    /// `aws_vpc.main`. An address is admitted when it equals a prefix or
    /// continues one with `.` or `[`; an empty list admits every managed
    /// resource. What is left out is listed in
    /// [`model::Report::filtered_out`].
    pub only: Vec<String>,
    /// Whether each module instance becomes a stack, so that the controller
    /// reconciles all of its rows as one unit. Adds [`model::MODULE_COLUMN`]
    /// to every table.
    pub stacks_from_modules: bool,
    /// Whether sensitive attributes become columns. Their values are then
    /// written to `rows.sql` in the clear.
    pub include_sensitive: bool,
    /// Whether every mapping carries `prevent_destroy`, so that the engine
    /// refuses to delete or replace what it has adopted.
    pub protect: bool,
    /// Whether a string value equal to the `id` of any other resource counts
    /// as a reference to it, with no other evidence. Such references can
    /// form cycles, which the analysis breaks by turning them back into
    /// constants.
    pub guess_references: bool,
    /// The replication slot the configuration names.
    pub slot: String,
    /// The publication the configuration names.
    pub publication: String,
    /// The Pulumi type token of each Terraform resource type, as read by
    /// [`pulumi::load_mapping`]. Only consulted with [`EngineKind::Pulumi`].
    pub pulumi_types: BTreeMap<String, String>,
    /// The Pulumi project name. Only used with [`EngineKind::Pulumi`].
    pub project: String,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            engine: EngineKind::Morph,
            pg_schema: "public".to_owned(),
            only: Vec::new(),
            stacks_from_modules: false,
            include_sensitive: false,
            protect: true,
            guess_references: false,
            slot: "morph_terraform".to_owned(),
            publication: "morph_terraform".to_owned(),
            pulumi_types: BTreeMap::new(),
            project: "terraform-migration".to_owned(),
        }
    }
}

/// The Terraform JSON documents a migration is made from.
#[derive(Clone, Debug)]
pub struct Inputs {
    /// The state, from `terraform show -json`.
    pub state: State,
    /// The provider schemas, from `terraform providers schema -json`.
    pub schemas: Schemas,
    /// The plan, from `terraform show -json <planfile>`, if given. With it
    /// the migration knows exactly which attributes the configuration sets
    /// and what they refer to.
    pub plan: Option<Plan>,
}

impl Inputs {
    /// Loads the documents from disk.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Read`] if a file cannot be read, [`Error::Json`] if
    /// it is not JSON of the expected shape, or [`Error::Format`] if its
    /// `format_version` is not `1.x`.
    pub fn load(state: &Path, schema: &Path, plan: Option<&Path>) -> Result<Self> {
        Ok(Self {
            state: State::load(state)?,
            schemas: Schemas::load(schema)?,
            plan: plan.map(Plan::load).transpose()?,
        })
    }
}

/// The rendered files, as text, together with the migration they were made
/// from.
#[derive(Clone, Debug)]
pub struct Output {
    /// The text of [`SCHEMA_FILE`].
    pub schema_sql: String,
    /// The text of [`ROWS_FILE`].
    pub rows_sql: String,
    /// The text of [`CONFIG_FILE`].
    pub controller_toml: String,
    /// The text of [`REPORT_FILE`].
    pub report_md: String,
    /// The migration the files were rendered from.
    pub migration: Migration,
}

impl Output {
    /// Writes the four files into `dir`, creating the directory if necessary
    /// and replacing any earlier files of the same names. Returns the paths
    /// written, in the order [`SCHEMA_FILE`], [`ROWS_FILE`], [`CONFIG_FILE`],
    /// [`REPORT_FILE`].
    ///
    /// The files are written one after another, so a failure part-way
    /// leaves the earlier ones in place.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Write`] if the directory or any file cannot be
    /// written.
    pub fn write_to(&self, dir: &Path) -> Result<Vec<PathBuf>> {
        fs::create_dir_all(dir).map_err(|source| Error::Write {
            path: dir.to_owned(),
            source,
        })?;
        let mut written = Vec::new();
        for (name, text) in [
            (SCHEMA_FILE, &self.schema_sql),
            (ROWS_FILE, &self.rows_sql),
            (CONFIG_FILE, &self.controller_toml),
            (REPORT_FILE, &self.report_md),
        ] {
            let path = dir.join(name);
            fs::write(&path, text).map_err(|source| Error::Write {
                path: path.clone(),
                source,
            })?;
            written.push(path);
        }
        Ok(written)
    }
}

/// Analyses the inputs and renders the four files.
///
/// This is pure computation: nothing is read from or written to disk. See
/// [`analyse::analyse`] for the rules applied.
///
/// # Errors
///
/// Returns any error of [`analyse::analyse`], or [`Error::Value`] if a
/// value cannot be written as an SQL literal.
pub fn migrate(inputs: &Inputs, settings: &Settings) -> Result<Output> {
    let migration = analyse::analyse(inputs, settings)?;
    Ok(Output {
        schema_sql: render::sql::schema_sql(&migration, settings),
        rows_sql: render::sql::rows_sql(&migration, settings)?,
        controller_toml: render::config::controller_toml(&migration, settings),
        report_md: render::report::report_md(&migration, settings),
        migration,
    })
}
