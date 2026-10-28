//! The `morph-terraform-migrate` command-line program.
//!
//! A thin wrapper around [`morph_terraform_migrate::migrate`]: it parses the
//! arguments into [`Settings`], loads the [`Inputs`], writes the four files
//! and prints a one-line summary. Exit codes follow the usual convention:
//! 0 when the files were written, 1 when the migration could not be made
//! (the error goes to stderr), and 2 when the arguments are invalid.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, ValueEnum};
use morph_terraform_migrate::{EngineKind, Inputs, Settings, migrate, pulumi};

// The doc comments on `Arguments` and its fields are clap's `--help` text.
// They are written for the user at the terminal, not for rustdoc, and the
// `cli` test suite checks that every option appears in the help output.

/// Migrates resources managed by Terraform into tables and rows for the
/// Morph PostgreSQL controller.
///
/// Give it what Terraform wrote and it writes schema.sql, rows.sql,
/// controller.toml and report.md. Nothing is run against a database or a
/// cloud; read report.md, then load the SQL.
#[derive(Debug, Parser)]
#[command(version, about)]
struct Arguments {
    /// The state as JSON: terraform show -json > state.json
    #[arg(long, value_name = "FILE")]
    state: PathBuf,
    /// The provider schemas as JSON: terraform providers schema -json > schema.json
    #[arg(long, value_name = "FILE")]
    schema: PathBuf,
    /// A plan as JSON, which makes the migration exact about which
    /// attributes the configuration sets and what they refer to:
    /// terraform plan -out tfplan && terraform show -json tfplan > plan.json
    #[arg(long, value_name = "FILE")]
    plan: Option<PathBuf>,
    /// The directory the four files are written into.
    #[arg(long, value_name = "DIR")]
    out: PathBuf,
    /// The engine the controller will use.
    #[arg(long, value_enum, default_value_t = Engine::Morph)]
    engine: Engine,
    /// A Pulumi mapping file naming the Pulumi type of each Terraform
    /// resource type: pulumi package get-mapping terraform <provider> -o aws.json.
    /// May be given more than once.
    #[arg(long, value_name = "FILE")]
    pulumi_mapping: Vec<PathBuf>,
    /// The Pulumi project the stacks belong to.
    #[arg(long, default_value = "terraform-migration")]
    project: String,
    /// Migrate only the resources at or under this address: module.vpc,
    /// aws_vpc.main. May be given more than once.
    #[arg(long, value_name = "ADDRESS")]
    only: Vec<String>,
    /// The PostgreSQL schema the tables go in.
    #[arg(long, default_value = "public")]
    pg_schema: String,
    /// Make each module instance a stack, reconciled as one unit.
    #[arg(long)]
    stacks_from_modules: bool,
    /// Make columns of sensitive attributes too. Their values are then in
    /// rows.sql in the clear.
    #[arg(long)]
    include_sensitive: bool,
    /// Leave the adopted resources unprotected: no prevent_destroy.
    #[arg(long)]
    unprotected: bool,
    /// Take a value equal to the id of any resource as a reference to it,
    /// with no other evidence.
    #[arg(long)]
    guess_references: bool,
    /// The replication slot the configuration names.
    #[arg(long, default_value = "morph_terraform")]
    slot: String,
    /// The publication the configuration names.
    #[arg(long, default_value = "morph_terraform")]
    publication: String,
}

/// The engines the configuration can be written for.
///
/// Mirrors [`EngineKind`] so that the library type need not derive clap's
/// `ValueEnum`; [`run`] maps one onto the other.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
enum Engine {
    /// Morph as a library, with providers written for Terraform.
    Morph,
    /// The Pulumi command line.
    Pulumi,
}

fn main() -> ExitCode {
    let arguments = Arguments::parse();
    match run(arguments) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("morph-terraform-migrate: {error}");
            ExitCode::FAILURE
        }
    }
}

/// Runs the migration the arguments describe, stopping at the first error.
///
/// Mapping files are merged in command-line order, so where two name the
/// same resource type the later file wins.
fn run(arguments: Arguments) -> morph_terraform_migrate::Result<()> {
    let mut pulumi_types = BTreeMap::new();
    for path in &arguments.pulumi_mapping {
        pulumi_types.extend(pulumi::load_mapping(path)?);
    }
    let settings = Settings {
        engine: match arguments.engine {
            Engine::Morph => EngineKind::Morph,
            Engine::Pulumi => EngineKind::Pulumi,
        },
        pg_schema: arguments.pg_schema,
        only: arguments.only,
        stacks_from_modules: arguments.stacks_from_modules,
        include_sensitive: arguments.include_sensitive,
        protect: !arguments.unprotected,
        guess_references: arguments.guess_references,
        slot: arguments.slot,
        publication: arguments.publication,
        pulumi_types,
        project: arguments.project,
    };
    let inputs = Inputs::load(
        &arguments.state,
        &arguments.schema,
        arguments.plan.as_deref(),
    )?;
    let output = migrate(&inputs, &settings)?;
    let written = output.write_to(&arguments.out)?;
    let report = &output.migration.report;
    println!(
        "{} rows in {} tables, {} references; {} data sources and {} filtered resources not migrated",
        output.migration.rows.len(),
        output.migration.tables.len(),
        report.references.len(),
        report.data_sources.len(),
        report.filtered_out.len()
    );
    for path in written {
        println!("wrote {}", path.display());
    }
    println!("read the report before loading anything");
    Ok(())
}
