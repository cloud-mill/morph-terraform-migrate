//! The SQL: `schema.sql` creates the tables, `rows.sql` loads the rows.
//!
//! Both files can be run more than once. The schema adds tables and
//! columns only where they are missing and never drops anything, so a later
//! migration of further resources extends what is there. The rows are one
//! transaction of upserts keyed by the Terraform address, so a second run
//! brings the rows up to date and adds nothing twice. Every identifier is
//! quoted and every string is an `E''` literal, so that what Terraform held
//! is loaded as it was whatever the server's settings.

use std::fmt::Write as _;

use serde_json::Value;

use crate::Settings;
use crate::error::{Error, Result};
use crate::model::{
    ADDRESS_COLUMN, Cell, Column, ColumnType, IMPORT_COLUMN, MODULE_COLUMN, Migration, NAME_COLUMN,
    Row, Table,
};

/// `name` as a quoted SQL identifier, with any `"` doubled.
pub fn quote_identifier(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// `text` as an `E''` string literal, which reads the same whatever
/// `standard_conforming_strings` is set to. Control characters are written
/// as escapes.
///
/// Returns `None` if the text holds a NUL character, which PostgreSQL text
/// cannot.
pub fn quote_literal(text: &str) -> Option<String> {
    let mut out = String::with_capacity(text.len() + 3);
    out.push_str("E'");
    for character in text.chars() {
        match character {
            '\'' => out.push_str("''"),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\0' => return None,
            control if (control as u32) < 0x20 => {
                let _ = write!(out, "\\x{:02x}", control as u32);
            }
            other => out.push(other),
        }
    }
    out.push('\'');
    Some(out)
}

/// The schema-qualified, quoted name of `table`.
fn qualified(settings: &Settings, table: &str) -> String {
    format!(
        "{}.{}",
        quote_identifier(&settings.pg_schema),
        quote_identifier(table)
    )
}

/// The name of the foreign key constraint on `column` of `table`, cut to
/// PostgreSQL's 63-byte limit on identifiers.
fn constraint_name(table: &str, column: &str) -> String {
    let mut name = format!("{table}_{column}_fkey");
    while name.len() > 63 {
        name.pop();
    }
    name
}

/// Renders `schema.sql`.
///
/// One transaction: the schema, if it is not `public`; then each table as
/// `CREATE TABLE IF NOT EXISTS` followed by `ADD COLUMN IF NOT EXISTS` for
/// every column, so that a later run adds columns to a table that already
/// exists; then every foreign key. The foreign keys come after all the
/// tables because a reference may point at a table created later in the
/// file.
///
/// # Panics
///
/// Panics if `settings.pg_schema`, a resource type, a column name or a
/// provider name holds a NUL character. Terraform never writes one, and a
/// command-line argument cannot carry one.
pub fn schema_sql(migration: &Migration, settings: &Settings) -> String {
    let mut sql = String::new();
    sql.push_str("-- schema.sql: the tables for the resources Terraform manages.\n");
    sql.push_str("-- Made by morph-terraform-migrate. Safe to run more than once: tables and\n");
    sql.push_str("-- columns are added where missing, and nothing is dropped or changed.\n");
    sql.push_str("BEGIN;\n\n");
    if settings.pg_schema != "public" {
        let _ = writeln!(
            sql,
            "CREATE SCHEMA IF NOT EXISTS {};\n",
            quote_identifier(&settings.pg_schema)
        );
    }
    for table in &migration.tables {
        table_sql(&mut sql, migration, settings, table);
    }
    let mut any = false;
    for table in &migration.tables {
        for column in &table.columns {
            let Some(reference) = &column.reference else {
                continue;
            };
            if !any {
                sql.push_str(
                    "-- References: a column that holds the names of rows of another table.\n",
                );
                sql.push_str(
                    "-- The controller makes each such row wait for the row it names, and\n",
                );
                sql.push_str(
                    "-- hands the named row's attribute across, to the column's own attribute\n",
                );
                sql.push_str("-- or to the one a companion column sets.\n");
                any = true;
            }
            // PostgreSQL has no `ADD CONSTRAINT IF NOT EXISTS`, so the
            // catalogue is checked in a DO block to keep the file rerunnable.
            let constraint = constraint_name(&table.resource_type, &column.name);
            let _ = writeln!(
                sql,
                "DO $$ BEGIN\n    IF NOT EXISTS (SELECT 1 FROM pg_constraint WHERE conname = {} AND connamespace = {}::regnamespace) THEN\n        ALTER TABLE {} ADD CONSTRAINT {} FOREIGN KEY ({}) REFERENCES {} ({});\n    END IF;\nEND $$;",
                quote_literal(&constraint).expect("no NUL in a name"),
                quote_literal(&settings.pg_schema).expect("no NUL in a name"),
                qualified(settings, &table.resource_type),
                quote_identifier(&constraint),
                quote_identifier(&column.name),
                qualified(settings, &reference.table),
                quote_identifier(NAME_COLUMN),
            );
        }
    }
    if any {
        sql.push('\n');
    }
    sql.push_str("COMMIT;\n");
    sql
}

/// Appends one table: its `CREATE TABLE`, an `ADD COLUMN IF NOT EXISTS` per
/// column, and comments on the table and its control columns.
fn table_sql(sql: &mut String, migration: &Migration, settings: &Settings, table: &Table) {
    let name = qualified(settings, &table.resource_type);
    let rows = migration.rows_of(&table.resource_type).count();
    let _ = writeln!(
        sql,
        "-- {}: {} of {}, {} row{}",
        table.resource_type,
        table.resource_type,
        table.provider,
        rows,
        if rows == 1 { "" } else { "s" }
    );
    let _ = writeln!(sql, "CREATE TABLE IF NOT EXISTS {name} (");
    let mut lines = vec![
        format!("{} text PRIMARY KEY", quote_identifier(NAME_COLUMN)),
        format!("{} text NOT NULL UNIQUE", quote_identifier(ADDRESS_COLUMN)),
        format!("{} text", quote_identifier(IMPORT_COLUMN)),
    ];
    if settings.stacks_from_modules {
        lines.push(format!("{} text NOT NULL", quote_identifier(MODULE_COLUMN)));
    }
    for column in &table.columns {
        lines.push(column_definition(column));
    }
    let last = lines.len() - 1;
    for (index, line) in lines.iter().enumerate() {
        let _ = writeln!(sql, "    {line}{}", if index == last { "" } else { "," });
    }
    sql.push_str(");\n");
    if settings.stacks_from_modules {
        let _ = writeln!(
            sql,
            "ALTER TABLE {name} ADD COLUMN IF NOT EXISTS {} text NOT NULL DEFAULT 'root';",
            quote_identifier(MODULE_COLUMN)
        );
    }
    for column in &table.columns {
        let _ = writeln!(
            sql,
            "ALTER TABLE {name} ADD COLUMN IF NOT EXISTS {};",
            column_definition(column)
        );
    }
    if settings.stacks_from_modules {
        // Logical replication sends only the replica identity with a
        // DELETE, which by default is the primary key alone. The controller
        // needs the stack column too, to know which stack to reconcile.
        let _ = writeln!(sql, "ALTER TABLE {name} REPLICA IDENTITY FULL;");
    }
    let _ = writeln!(
        sql,
        "COMMENT ON TABLE {name} IS {};",
        quote_literal(&format!(
            "Terraform resource type {} of {}, migrated by morph-terraform-migrate",
            table.resource_type, table.provider
        ))
        .expect("no NUL in a type name")
    );
    let _ = writeln!(
        sql,
        "COMMENT ON COLUMN {name}.{} IS {};",
        quote_identifier(NAME_COLUMN),
        quote_literal(&format!(
            "The name of the resource in the controller: {}.<{NAME_COLUMN}>",
            table.resource_type
        ))
        .expect("no NUL")
    );
    let _ = writeln!(
        sql,
        "COMMENT ON COLUMN {name}.{} IS {};",
        quote_identifier(ADDRESS_COLUMN),
        quote_literal("The address the resource had in Terraform. Not an attribute.")
            .expect("no NUL")
    );
    let _ = writeln!(
        sql,
        "COMMENT ON COLUMN {name}.{} IS {};",
        quote_identifier(IMPORT_COLUMN),
        quote_literal(
            "The identifier the provider knows the object by, for the engine to take charge of it. Not an attribute."
        )
        .expect("no NUL")
    );
    sql.push('\n');
}

/// `"name" type`, followed by an SQL comment describing the reference if
/// the column holds one.
fn column_definition(column: &Column) -> String {
    let mut definition = format!("{} {}", quote_identifier(&column.name), column.kind.sql());
    if let Some(reference) = &column.reference {
        match &reference.sets {
            Some(sets) => {
                let _ = write!(
                    definition,
                    " /* names a row of {}, or is null; the controller puts its {} into {} */",
                    reference.table, reference.attribute, sets
                );
            }
            None => {
                let _ = write!(
                    definition,
                    " /* names a row of {}; the controller puts its {} here */",
                    reference.table, reference.attribute
                );
            }
        }
    }
    definition
}

/// Renders `rows.sql`.
///
/// One transaction of `INSERT ... ON CONFLICT (tf_address) DO UPDATE`
/// statements, one per row, in [`Migration::rows`] order, so that every
/// referenced row exists before the foreign key on a row that names it is
/// checked.
///
/// # Errors
///
/// Returns [`Error::Value`] if a value holds a NUL character, which
/// PostgreSQL text cannot.
///
/// # Panics
///
/// Panics if a row names a table that is not in `migration`, which
/// [`crate::analyse`] never produces.
pub fn rows_sql(migration: &Migration, settings: &Settings) -> Result<String> {
    let mut sql = String::new();
    sql.push_str("-- rows.sql: the rows for the resources Terraform manages, one per resource.\n");
    sql.push_str("-- Made by morph-terraform-migrate. One transaction. Safe to run more than\n");
    sql.push_str("-- once: a row is matched by its Terraform address and brought up to date.\n");
    sql.push_str("-- Rows come after the rows they refer to.\n");
    sql.push_str("BEGIN;\n\n");
    for row in &migration.rows {
        let table = migration
            .table(&row.table)
            .expect("every row's table is in the migration");
        row_sql(&mut sql, settings, table, row)?;
    }
    sql.push_str("COMMIT;\n");
    Ok(sql)
}

/// Appends the upsert of one row. Every column of the table is listed, so a
/// column the row has no cell for is set to `NULL`, and a rerun clears a
/// value the state no longer holds.
fn row_sql(sql: &mut String, settings: &Settings, table: &Table, row: &Row) -> Result<()> {
    let literal = |text: &str, attribute: &str| {
        quote_literal(text).ok_or_else(|| Error::Value {
            address: row.address.clone(),
            attribute: attribute.to_owned(),
            reason: "it holds a NUL character, which PostgreSQL text cannot".to_owned(),
        })
    };
    let mut columns = vec![NAME_COLUMN, ADDRESS_COLUMN, IMPORT_COLUMN];
    let mut values = vec![
        literal(&row.name, NAME_COLUMN)?,
        literal(&row.address, ADDRESS_COLUMN)?,
        match &row.import_id {
            Some(id) => literal(id, IMPORT_COLUMN)?,
            None => "NULL".to_owned(),
        },
    ];
    if settings.stacks_from_modules {
        columns.push(MODULE_COLUMN);
        values.push(literal(
            row.module.as_deref().unwrap_or(crate::model::ROOT_STACK),
            MODULE_COLUMN,
        )?);
    }
    for column in &table.columns {
        columns.push(&column.name);
        values.push(match row.cells.get(&column.name) {
            None => "NULL".to_owned(),
            Some(Cell::Reference { target, .. }) => literal(target, &column.name)?,
            Some(Cell::Constant(value)) => {
                constant_literal(value, column.kind).ok_or_else(|| Error::Value {
                    address: row.address.clone(),
                    attribute: column.name.clone(),
                    reason: "it holds a NUL character, which PostgreSQL text cannot".to_owned(),
                })?
            }
        });
    }
    let _ = writeln!(sql, "-- {}", row.address);
    let _ = writeln!(
        sql,
        "INSERT INTO {} ({})",
        qualified(settings, &table.resource_type),
        columns
            .iter()
            .map(|column| quote_identifier(column))
            .collect::<Vec<_>>()
            .join(", ")
    );
    let _ = writeln!(sql, "VALUES ({})", values.join(", "));
    let _ = writeln!(
        sql,
        "ON CONFLICT ({}) DO UPDATE SET {};\n",
        quote_identifier(ADDRESS_COLUMN),
        columns
            .iter()
            .filter(|column| **column != ADDRESS_COLUMN)
            .map(|column| format!("{0} = EXCLUDED.{0}", quote_identifier(column)))
            .collect::<Vec<_>>()
            .join(", ")
    );
    Ok(())
}

/// `value` as a literal for a column of `kind`.
///
/// A value of another JSON type than the column's is written as text with
/// a cast, so that PostgreSQL, not this tool, reports the mismatch when the
/// file is loaded. Returns `None` if the text holds a NUL character.
fn constant_literal(value: &Value, kind: ColumnType) -> Option<String> {
    match kind {
        ColumnType::Text => match value {
            Value::String(text) => quote_literal(text),
            Value::Null => Some("NULL".to_owned()),
            other => quote_literal(&other.to_string()),
        },
        ColumnType::Numeric => match value {
            Value::Number(number) => Some(number.to_string()),
            Value::Null => Some("NULL".to_owned()),
            Value::String(text) => quote_literal(text).map(|literal| format!("{literal}::numeric")),
            other => quote_literal(&other.to_string()).map(|literal| format!("{literal}::numeric")),
        },
        ColumnType::Boolean => match value {
            Value::Bool(flag) => Some(if *flag { "TRUE" } else { "FALSE" }.to_owned()),
            Value::Null => Some("NULL".to_owned()),
            Value::String(text) => quote_literal(text).map(|literal| format!("{literal}::boolean")),
            other => quote_literal(&other.to_string()).map(|literal| format!("{literal}::boolean")),
        },
        ColumnType::Jsonb => {
            if value.is_null() {
                Some("NULL".to_owned())
            } else {
                quote_literal(&value.to_string()).map(|literal| format!("{literal}::jsonb"))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifiers_and_literals_are_quoted() {
        assert_eq!(quote_identifier("plain"), "\"plain\"");
        assert_eq!(quote_identifier("odd\"name"), "\"odd\"\"name\"");
        assert_eq!(quote_literal("it's").unwrap(), "E'it''s'");
        assert_eq!(quote_literal("a\\b\n\t\r").unwrap(), "E'a\\\\b\\n\\t\\r'");
        assert_eq!(quote_literal("bell\u{7}").unwrap(), "E'bell\\x07'");
        assert_eq!(quote_literal("x\u{e9}").unwrap(), "E'x\u{e9}'");
        assert!(quote_literal("nul\0").is_none());
        assert_eq!(constraint_name("t", "c"), "t_c_fkey");
        assert_eq!(constraint_name(&"t".repeat(60), "c").len(), 63);
    }

    #[test]
    fn constants_are_written_for_their_column_type() {
        let text = |value: Value, kind| constant_literal(&value, kind).unwrap();
        assert_eq!(text("a".into(), ColumnType::Text), "E'a'");
        assert_eq!(text(7.into(), ColumnType::Text), "E'7'");
        assert_eq!(text(Value::Null, ColumnType::Text), "NULL");
        assert_eq!(text(7.into(), ColumnType::Numeric), "7");
        assert_eq!(text("7".into(), ColumnType::Numeric), "E'7'::numeric");
        assert_eq!(text(true.into(), ColumnType::Numeric), "E'true'::numeric");
        assert_eq!(text(Value::Null, ColumnType::Numeric), "NULL");
        assert_eq!(text(true.into(), ColumnType::Boolean), "TRUE");
        assert_eq!(text("f".into(), ColumnType::Boolean), "E'f'::boolean");
        assert_eq!(text(0.into(), ColumnType::Boolean), "E'0'::boolean");
        assert_eq!(text(Value::Null, ColumnType::Boolean), "NULL");
        assert_eq!(
            text(serde_json::json!({"a": [1, "b"]}), ColumnType::Jsonb),
            "E'{\"a\":[1,\"b\"]}'::jsonb"
        );
        assert_eq!(text(Value::Null, ColumnType::Jsonb), "NULL");
        // A NUL inside JSON text is escaped by the JSON writer, so only
        // text columns can meet one.
        assert!(constant_literal(&"\0".into(), ColumnType::Text).is_none());
        assert_eq!(
            text("\0".into(), ColumnType::Jsonb),
            "E'\"\\\\u0000\"'::jsonb"
        );
    }
}
