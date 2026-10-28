//! The in-memory form of a migration, between reading Terraform's JSON and
//! writing the files.
//!
//! [`Migration`] holds the tables, the rows in an order they can be loaded
//! in, the providers in use, and a [`Report`] of every decision the user
//! should look at. [`crate::analyse`] builds it and [`crate::render`]
//! serialises it; nothing in this module decides anything.
//!
//! The constants name the *control columns*, which tables have in addition
//! to their attribute columns, and the attribute every table exports.

use std::collections::BTreeMap;
use std::fmt;

use serde_json::Value;

/// The primary key column, holding the row's name.
///
/// This is the controller's `id_column`: together with the table it names
/// the resource as `<resource_type>.<name>`. See [`crate::names`] for how
/// names are made from addresses.
pub const NAME_COLUMN: &str = "morph_name";
/// The column holding the row's Terraform address.
///
/// It is `UNIQUE`, and `rows.sql` upserts on it, so loading the rows again
/// updates them in place. The controller is told to exclude it from the
/// resource's attributes.
pub const ADDRESS_COLUMN: &str = "tf_address";
/// The column holding the identifier the provider knows the object by.
///
/// It is the mapping's `import_column`: on a row's first reconciliation the
/// engine imports the existing object by this value instead of creating a
/// new one.
pub const IMPORT_COLUMN: &str = "tf_import_id";
/// The column naming the row's stack.
///
/// Only present when
/// [`Settings::stacks_from_modules`](crate::Settings::stacks_from_modules)
/// is set.
pub const MODULE_COLUMN: &str = "tf_module";
/// The stack of the root module's rows when module instances are stacks.
pub const ROOT_STACK: &str = "root";
/// The attribute every table exports to the rows that refer to it.
///
/// Every Terraform resource has an `id`, and it is what most references
/// read, so each mapping declares it as its `exports`.
pub const EXPORTS: &str = "id";

/// Column names reserved for the control columns.
///
/// An attribute with one of these names can never be a column; it is left
/// out and reported as [`LeftOutReason::ReservedName`].
pub const CONTROL_COLUMNS: [&str; 4] = [NAME_COLUMN, ADDRESS_COLUMN, IMPORT_COLUMN, MODULE_COLUMN];

/// The SQL type of an attribute column.
///
/// Terraform's scalar types map to `text`, `numeric` and `boolean`; every
/// collection, object, nested or dynamic type, and every nested block,
/// becomes `jsonb`. See
/// [`Attribute::column_type`](crate::terraform::schema::Attribute::column_type).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ColumnType {
    /// `text`.
    Text,
    /// `numeric`.
    Numeric,
    /// `boolean`.
    Boolean,
    /// `jsonb`, for collections, objects and blocks.
    Jsonb,
}

impl ColumnType {
    /// The type's name as written in `CREATE TABLE`.
    pub fn sql(self) -> &'static str {
        match self {
            ColumnType::Text => "text",
            ColumnType::Numeric => "numeric",
            ColumnType::Boolean => "boolean",
            ColumnType::Jsonb => "jsonb",
        }
    }
}

/// Everything the four files are made from.
#[derive(Clone, Debug, Default)]
pub struct Migration {
    /// The version of Terraform that wrote the state, if the state says.
    pub terraform_version: Option<String>,
    /// One table per resource type, sorted by resource type.
    pub tables: Vec<Table>,
    /// One row per resource instance, in an order that respects references:
    /// every row comes after the rows it refers to, and rows that are
    /// otherwise unconstrained keep the state's order.
    pub rows: Vec<Row>,
    /// The providers the rows' resources belong to, sorted by full name.
    pub providers: Vec<Provider>,
    /// Every decision the user should look at.
    pub report: Report,
}

impl Migration {
    /// The table named `resource_type`, if there is one.
    pub fn table(&self, resource_type: &str) -> Option<&Table> {
        self.tables
            .iter()
            .find(|table| table.resource_type == resource_type)
    }

    /// The rows of `table`, in their load order.
    pub fn rows_of<'a>(&'a self, table: &'a str) -> impl Iterator<Item = &'a Row> + 'a {
        self.rows.iter().filter(move |row| row.table == table)
    }
}

/// One table: one Terraform resource type.
#[derive(Clone, Debug)]
pub struct Table {
    /// The resource type, which is also the table's name and the
    /// controller's `resource_type`.
    pub resource_type: String,
    /// The full name of the provider the type belongs to, such as
    /// `registry.terraform.io/hashicorp/aws`.
    pub provider: String,
    /// The attribute columns and their companion reference columns, in
    /// attribute-name order with each companion directly after the
    /// attribute it sets. The control columns are not listed.
    pub columns: Vec<Column>,
    /// The Pulumi type token of the resource, when the engine is Pulumi and
    /// a mapping file names it. A type without one is listed in
    /// [`Report::unmapped_types`].
    pub pulumi_type: Option<String>,
}

impl Table {
    /// The column named `name`, if the table has it.
    pub fn column(&self, name: &str) -> Option<&Column> {
        self.columns.iter().find(|column| column.name == name)
    }
}

/// One column of a table, other than the control columns.
#[derive(Clone, Debug)]
pub struct Column {
    /// The column's name: the attribute's name, or for a companion the
    /// attribute's name followed by `_ref` and, where several targets need
    /// telling apart, the target table and attribute.
    pub name: String,
    /// The SQL type. A column with a [`reference`](Self::reference) is
    /// always [`ColumnType::Text`], since it holds row names.
    pub kind: ColumnType,
    /// `Some` when the column holds names of rows of another table rather
    /// than plain values: either every value of the attribute turned out to
    /// be a reference, or the column is a companion made for an attribute
    /// that is a reference in some rows only (see [`ColumnReference::sets`]).
    pub reference: Option<ColumnReference>,
}

/// What a reference column points at.
///
/// Rendered as a foreign key on [`NAME_COLUMN`] of the referenced table
/// and, unless the foreign key alone describes it, as a
/// `[[mappings.references]]` entry in `controller.toml`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct ColumnReference {
    /// The referenced table.
    pub table: String,
    /// The attribute of the referenced resource whose value the controller
    /// substitutes for the row name.
    pub attribute: String,
    /// For a companion column, the attribute of the referring resource that
    /// receives the value; the companion itself is not an attribute. `None`
    /// when the column stands for the attribute of its own name.
    pub sets: Option<String>,
}

/// One row: one resource instance.
#[derive(Clone, Debug)]
pub struct Row {
    /// The Terraform address of the instance, written to [`ADDRESS_COLUMN`].
    pub address: String,
    /// The table, which is the resource type.
    pub table: String,
    /// The row's name, written to [`NAME_COLUMN`].
    pub name: String,
    /// The stack, written to [`MODULE_COLUMN`] when module instances are
    /// stacks. `None` otherwise.
    pub module: Option<String>,
    /// The provider's identifier for the object, written to
    /// [`IMPORT_COLUMN`]. Taken from the `id` attribute, with a number
    /// written as text. `None` when the state has no string or number
    /// `id`, in which case the row is listed in
    /// [`Report::without_import_id`].
    pub import_id: Option<String>,
    /// The cells with a value, by column name. A companion column's cell is
    /// keyed by the companion's name; a column with no cell is `NULL`.
    pub cells: BTreeMap<String, Cell>,
}

/// The value of one cell.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Cell {
    /// A plain value, exactly as the state holds it.
    Constant(Value),
    /// The name of a row of the column's referenced table. The controller
    /// reconciles that row first and substitutes its attribute.
    Reference {
        /// The referenced row's name.
        target: String,
        /// The referenced row's address: for the report, for ordering the
        /// rows, and for finding the target's table.
        target_address: String,
        /// The attribute of the referenced resource whose value is taken.
        attribute: String,
        /// How the reference was found.
        evidence: Evidence,
    },
}

/// How a reference was found, from strongest to weakest.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Evidence {
    /// The plan's configuration says the attribute's expression refers to
    /// the resource, and the value equals one of the instance's attributes.
    Configuration,
    /// The state's `depends_on` names the resource, and the value equals
    /// one of the instance's attributes. Used when there is no plan or the
    /// configuration could not be followed.
    Dependency,
    /// The value equals the `id` of a resource, and nothing else connects
    /// them. Only produced with
    /// [`Settings::guess_references`](crate::Settings::guess_references).
    Value,
}

impl fmt::Display for Evidence {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Evidence::Configuration => "configuration",
            Evidence::Dependency => "dependency",
            Evidence::Value => "value",
        })
    }
}

/// A provider the migrated resources belong to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Provider {
    /// The full name, such as `registry.terraform.io/hashicorp/aws`.
    pub full_name: String,
    /// The source as a controller provider block names it: the last two
    /// segments of the full name, such as `hashicorp/aws`.
    pub source: String,
    /// The short name, such as `aws`, which prefixes Pulumi configuration
    /// keys.
    pub name: String,
    /// The settings of the provider block that could be carried over,
    /// plain constants and root variables, by setting name. Sensitive and
    /// secret-looking settings are never here; see
    /// [`Report::provider_notes`].
    pub config: BTreeMap<String, Value>,
}

/// Every decision the user should look at, and everything left aside.
///
/// Rendered as `report.md`. Lists are in the order the analysis met the
/// items unless noted otherwise.
#[derive(Clone, Debug, Default)]
pub struct Report {
    /// Data sources, which are read and not managed, by address.
    pub data_sources: Vec<String>,
    /// Managed resources the address filter left out, by address.
    pub filtered_out: Vec<String>,
    /// Attributes with a value in the state that did not become columns,
    /// each with the reason and the number of rows affected. Sorted by
    /// type, attribute and reason.
    pub left_out: Vec<LeftOut>,
    /// Every reference between rows that was made, in row order.
    pub references: Vec<ReferenceNote>,
    /// Values that refer to other resources in Terraform but stay
    /// constants, with why.
    pub constants: Vec<ConstantNote>,
    /// Rows that could not take the short form of their name.
    pub renamed: Vec<Renamed>,
    /// Rows whose resource has no `id` to import by, by address.
    pub without_import_id: Vec<String>,
    /// Resource types that no Pulumi mapping file names. Only filled with
    /// the Pulumi engine.
    pub unmapped_types: Vec<String>,
    /// Provider settings that could not be carried over, as sentences,
    /// sorted.
    pub provider_notes: Vec<String>,
    /// Attributes the state holds but the schema does not know, as
    /// `type.attribute`, sorted. Usually a sign that the schema file comes
    /// from another version of the provider. They are never columns.
    pub unknown_attributes: Vec<String>,
    /// The stacks made from module instances, each with its row count,
    /// sorted by name. Only filled when module instances are stacks.
    pub stacks: Vec<(String, usize)>,
}

/// An attribute with values in the state that is not a column.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LeftOut {
    /// The resource type.
    pub resource_type: String,
    /// The attribute.
    pub attribute: String,
    /// Why it is not a column.
    pub reason: LeftOutReason,
    /// How many rows had a value for it.
    pub rows: usize,
}

/// Why an attribute with values is not a column.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum LeftOutReason {
    /// The schema or the state marks it sensitive, and sensitive columns
    /// were not asked for.
    Sensitive,
    /// It is optional and computed, and without a plan there is no telling
    /// whether the user or the provider set it.
    OptionalComputed,
    /// Its name is one of the [`CONTROL_COLUMNS`].
    ReservedName,
}

impl fmt::Display for LeftOutReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            LeftOutReason::Sensitive => "sensitive",
            LeftOutReason::OptionalComputed => {
                "optional and computed; give a plan to tell whether the configuration sets it"
            }
            LeftOutReason::ReservedName => "its name is one of the control columns",
        })
    }
}

/// A reference that was made.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReferenceNote {
    /// The referring row's address.
    pub address: String,
    /// The column holding the reference.
    pub column: String,
    /// The referenced row's address.
    pub target: String,
    /// The attribute taken from the referenced resource.
    pub attribute: String,
    /// How the reference was found.
    pub evidence: Evidence,
}

/// A value that refers to another resource in Terraform but stays a
/// constant.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConstantNote {
    /// The row's address.
    pub address: String,
    /// The attribute.
    pub attribute: String,
    /// Why it stays a constant.
    pub reason: ConstantReason,
}

/// Why a value that refers to another resource stays a constant.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConstantReason {
    /// The value is made from the reference and more, such as a template
    /// string, so no single attribute of the target equals it.
    Composed {
        /// The resources referred to, by address in the configuration.
        targets: Vec<String>,
    },
    /// The resource referred to is not in the migration.
    Outside {
        /// Its address in the configuration.
        target: String,
    },
    /// The reference is inside a collection or a block, which the
    /// controller takes as it is.
    Collection {
        /// The resources referred to, by address in the configuration.
        targets: Vec<String>,
    },
    /// The reference would make a cycle, which cannot be loaded.
    Cycle,
}

impl fmt::Display for ConstantReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConstantReason::Composed { targets } => write!(
                f,
                "composed from {} and more; no single attribute equals it",
                targets.join(", ")
            ),
            ConstantReason::Outside { target } => {
                write!(f, "refers to {target}, which is not in the migration")
            }
            ConstantReason::Collection { targets } => write!(
                f,
                "refers to {} inside a collection or a block, which is kept as it is",
                targets.join(", ")
            ),
            ConstantReason::Cycle => f.write_str("would make a circle of references"),
        }
    }
}

/// A row that did not take the short form of its name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Renamed {
    /// The row's address.
    pub address: String,
    /// The name it took.
    pub name: String,
    /// The short name that another row of the table also had.
    pub short: String,
}
