//! The report: `report.md`, what the user should read before loading
//! anything.
//!
//! The report has a fixed sequence of sections, and those with nothing to
//! say are left out. It ends with the `terraform state rm` commands of the
//! cutover, quoted for a POSIX shell.

use std::fmt::Write as _;

use crate::model::{Cell, Migration};
use crate::{EngineKind, Settings};

/// `text` as a Markdown table cell: pipes escaped and newlines flattened.
fn cell(text: &str) -> String {
    text.replace('|', "\\|").replace('\n', " ")
}

/// `text` in single quotes for a POSIX shell.
fn shell(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

/// Renders `report.md` from [`Migration::report`] and the migration's
/// tables and rows.
pub fn report_md(migration: &Migration, settings: &Settings) -> String {
    let report = &migration.report;
    let mut out = String::new();
    out.push_str("# Migration report\n\n");
    let _ = writeln!(
        out,
        "Made by morph-terraform-migrate from the state Terraform {} wrote, for the {} engine.",
        migration
            .terraform_version
            .as_deref()
            .unwrap_or("(unknown version)"),
        match settings.engine {
            EngineKind::Morph => "Morph",
            EngineKind::Pulumi => "Pulumi",
        }
    );
    out.push_str("Read it through before loading `schema.sql` and `rows.sql`: every decision that may want revisiting is listed here.\n\n");

    let reference_columns = migration
        .tables
        .iter()
        .flat_map(|table| table.columns.iter())
        .filter(|column| column.reference.is_some())
        .count();
    out.push_str("## Summary\n\n| What | How many |\n| --- | --- |\n");
    let _ = writeln!(out, "| Tables | {} |", migration.tables.len());
    let _ = writeln!(out, "| Rows | {} |", migration.rows.len());
    let _ = writeln!(out, "| Reference columns | {reference_columns} |");
    let _ = writeln!(
        out,
        "| References between rows | {} |",
        report.references.len()
    );
    let _ = writeln!(
        out,
        "| Values kept as constants although they refer to something | {} |",
        report.constants.len()
    );
    let _ = writeln!(
        out,
        "| Attributes with values left out | {} |",
        report.left_out.len()
    );
    let _ = writeln!(
        out,
        "| Data sources, not migrated | {} |",
        report.data_sources.len()
    );
    let _ = writeln!(
        out,
        "| Managed resources left out by the address filter | {} |",
        report.filtered_out.len()
    );
    out.push('\n');

    out.push_str("## Tables\n\n");
    for table in &migration.tables {
        let rows = migration.rows_of(&table.resource_type).count();
        let _ = writeln!(out, "### {}\n", table.resource_type);
        let _ = writeln!(
            out,
            "Provider {}. {rows} row{}.{}\n",
            table.provider,
            if rows == 1 { "" } else { "s" },
            match &table.pulumi_type {
                Some(token) => format!(" Pulumi type `{token}`."),
                None => String::new(),
            }
        );
        if table.columns.is_empty() {
            out.push_str(
                "No attribute columns: nothing of this type is set by the configuration.\n\n",
            );
        } else {
            out.push_str("| Column | Type | Refers to |\n| --- | --- | --- |\n");
            for column in &table.columns {
                let _ = writeln!(
                    out,
                    "| {} | {} | {} |",
                    cell(&column.name),
                    column.kind.sql(),
                    match &column.reference {
                        Some(reference) => match &reference.sets {
                            Some(sets) => format!(
                                "{} ({}), into {sets}",
                                reference.table, reference.attribute
                            ),
                            None => format!("{} ({})", reference.table, reference.attribute),
                        },
                        None => String::new(),
                    }
                );
            }
            out.push('\n');
        }
    }

    out.push_str(
        "## Rows\n\n| Table | Name | Terraform address | Import id |\n| --- | --- | --- | --- |\n",
    );
    for row in &migration.rows {
        let _ = writeln!(
            out,
            "| {} | {} | {} | {} |",
            cell(&row.table),
            cell(&row.name),
            cell(&row.address),
            cell(row.import_id.as_deref().unwrap_or("(none)"))
        );
    }
    out.push('\n');

    if !report.stacks.is_empty() {
        out.push_str("## Stacks\n\nEach module instance is a stack, reconciled as one unit.\n\n| Stack | Rows |\n| --- | --- |\n");
        for (stack, rows) in &report.stacks {
            let _ = writeln!(out, "| {} | {rows} |", cell(stack));
        }
        out.push('\n');
    }

    out.push_str("## References\n\n");
    if report.references.is_empty() {
        out.push_str("No row refers to another.\n\n");
    } else {
        out.push_str("Each is a column that names a row of another table. The controller reconciles the named row first and puts its attribute in place of the name, or into the attribute a companion column sets. The evidence is `configuration` when the plan says so, `dependency` when the state's `depends_on` and the value agree, and `value` when only the value matches.\n\n");
        out.push_str("| Row | Column | Refers to | Attribute | Evidence |\n| --- | --- | --- | --- | --- |\n");
        for note in &report.references {
            let _ = writeln!(
                out,
                "| {} | {} | {} | {} | {} |",
                cell(&note.address),
                cell(&note.column),
                cell(&note.target),
                cell(&note.attribute),
                note.evidence
            );
        }
        out.push('\n');
    }

    if !report.constants.is_empty() {
        out.push_str("## Kept as constants\n\nThese values refer to other resources in Terraform but are loaded as the values the state holds. They are right today and will not follow the resource they came from.\n\n| Row | Attribute | Why |\n| --- | --- | --- |\n");
        for note in &report.constants {
            let _ = writeln!(
                out,
                "| {} | {} | {} |",
                cell(&note.address),
                cell(&note.attribute),
                cell(&note.reason.to_string())
            );
        }
        out.push('\n');
    }

    if !report.left_out.is_empty() {
        out.push_str("## Attributes left out\n\nThese have values in the state but are not columns.\n\n| Type | Attribute | Rows | Why |\n| --- | --- | --- | --- |\n");
        for item in &report.left_out {
            let _ = writeln!(
                out,
                "| {} | {} | {} | {} |",
                cell(&item.resource_type),
                cell(&item.attribute),
                item.rows,
                item.reason
            );
        }
        out.push('\n');
    }

    if !report.renamed.is_empty() {
        out.push_str("## Names\n\nThese rows could not take the short form of their name, because another row of the table took it.\n\n| Row | Name | Short form taken by another |\n| --- | --- | --- |\n");
        for item in &report.renamed {
            let _ = writeln!(
                out,
                "| {} | {} | {} |",
                cell(&item.address),
                cell(&item.name),
                cell(&item.short)
            );
        }
        out.push('\n');
    }

    if !report.without_import_id.is_empty() {
        out.push_str("## Without an import id\n\nThese resources have no `id` in the state, so the engine has nothing to take charge of them by.\n\n");
        for address in &report.without_import_id {
            let _ = writeln!(out, "- {address}");
        }
        out.push('\n');
    }

    if !report.unmapped_types.is_empty() {
        out.push_str("## Pulumi types missing\n\nNo mapping file given names a Pulumi type for these. Make one with `pulumi package get-mapping terraform <provider>` and give it with `--pulumi-mapping`, or fill in `engine.types` by hand.\n\n");
        for resource_type in &report.unmapped_types {
            let _ = writeln!(out, "- {resource_type}");
        }
        out.push('\n');
    }

    if !report.provider_notes.is_empty() {
        out.push_str("## Provider settings\n\n");
        for note in &report.provider_notes {
            let _ = writeln!(out, "- {note}");
        }
        out.push('\n');
    }

    if !report.unknown_attributes.is_empty() {
        out.push_str("## Attributes the schema does not know\n\nThe state holds these, the schema does not: the schema may come from another version of the provider. They are not columns.\n\n");
        for name in &report.unknown_attributes {
            let _ = writeln!(out, "- {name}");
        }
        out.push('\n');
    }

    if !report.data_sources.is_empty() {
        out.push_str("## Data sources\n\nRead by Terraform, managed by nobody, not migrated. Where their values flowed into attributes, those are now constants.\n\n");
        for address in &report.data_sources {
            let _ = writeln!(out, "- {address}");
        }
        out.push('\n');
    }

    if !report.filtered_out.is_empty() {
        out.push_str("## Left out by the address filter\n\n");
        for address in &report.filtered_out {
            let _ = writeln!(out, "- {address}");
        }
        out.push('\n');
    }

    out.push_str("## Before the controller starts\n\n");
    out.push_str("Each mapping names `tf_import_id` as its import column. On a row's first reconciliation the engine takes charge of the object the column names, Morph through the provider's import and Pulumi through the `import` resource option, and then brings it in line with the row. Start with one row, or one table, and check that its first turn reports `changed=false`: a turn that changes something shows where the row and the object differ, and a row whose identifier names nothing fails rather than makes a copy.\n\n");
    if settings.protect {
        out.push_str("Every mapping carries `prevent_destroy`, so that nothing adopted can be deleted or replaced until that is turned off on purpose.\n\n");
    }
    let sensitive = report
        .left_out
        .iter()
        .filter(|item| item.reason == crate::model::LeftOutReason::Sensitive)
        .count();
    if sensitive > 0 {
        out.push_str("Sensitive attributes were left out of the rows. The resources keep their current values until a row sets them; decide where those values will live before the controller takes over.\n\n");
    }

    out.push_str("## Cutover\n\n");
    out.push_str("Once the controller has taken charge, take the resources out of Terraform's state so that two managers never share one object. Run these in the Terraform working directory, after a backup of the state:\n\n```sh\n");
    for row in &migration.rows {
        let _ = writeln!(out, "terraform state rm {}", shell(&row.address));
    }
    out.push_str("```\n");
    let constants = migration
        .rows
        .iter()
        .flat_map(|row| row.cells.values())
        .filter(|cell| matches!(cell, Cell::Constant(_)))
        .count();
    let _ = writeln!(
        out,
        "\nRows hold {constants} constant values and {} references in all.",
        report.references.len()
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cells_and_shell_words_are_escaped() {
        assert_eq!(cell("a|b\nc"), "a\\|b c");
        assert_eq!(shell("module.x[\"a\"]"), "'module.x[\"a\"]'");
        assert_eq!(shell("it's"), "'it'\\''s'");
    }
}
