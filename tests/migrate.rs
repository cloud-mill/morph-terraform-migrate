//! The migration of a real Terraform project, read from its JSON.
//!
//! The fixtures under `tests/fixtures` were written by Terraform 1.16 for
//! a small project of the `local` and `random` providers: files whose
//! content comes from other resources, a module called twice with
//! `for_each`, a resource with `count`, a sensitive file, a data source,
//! and a variable. The state, the plan and the schemas are the real
//! output of `terraform show -json`, `terraform show -json tfplan` and
//! `terraform providers schema -json`. Run with `cargo test --test
//! migrate`.

use std::collections::BTreeMap;
use std::path::Path;

use morph_terraform_migrate::model::{
    ADDRESS_COLUMN, Cell, ConstantReason, Evidence, IMPORT_COLUMN, LeftOutReason, NAME_COLUMN,
};
use morph_terraform_migrate::{EngineKind, Error, Inputs, Settings, migrate, pulumi};

fn fixture(name: &str) -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

fn inputs(with_plan: bool) -> Inputs {
    Inputs::load(
        &fixture("state.json"),
        &fixture("schema.json"),
        with_plan.then(|| fixture("plan.json")).as_deref(),
    )
    .unwrap()
}

fn pulumi_settings() -> Settings {
    let mut types = BTreeMap::new();
    types.extend(pulumi::load_mapping(&fixture("pulumi-local.json")).unwrap());
    types.extend(pulumi::load_mapping(&fixture("pulumi-random.json")).unwrap());
    Settings {
        engine: EngineKind::Pulumi,
        pulumi_types: types,
        ..Settings::default()
    }
}

#[test]
fn with_the_plan_every_set_attribute_is_a_column_and_references_are_exact() {
    let output = migrate(&inputs(true), &Settings::default()).unwrap();
    let migration = &output.migration;
    assert_eq!(migration.terraform_version.as_deref(), Some("1.16.7"));
    let tables: Vec<&str> = migration
        .tables
        .iter()
        .map(|table| table.resource_type.as_str())
        .collect();
    assert_eq!(
        tables,
        [
            "local_file",
            "local_sensitive_file",
            "random_id",
            "random_pet"
        ]
    );
    assert_eq!(migration.rows.len(), 13);
    assert_eq!(
        migration.report.data_sources,
        ["data.local_file.readme_back"]
    );
    assert!(migration.report.filtered_out.is_empty());
    assert!(migration.report.unknown_attributes.is_empty());

    // Only what the configuration sets is a column: the provider's
    // defaults for the permissions are not, nor the computed checksums.
    let file = migration.table("local_file").unwrap();
    let columns: Vec<&str> = file
        .columns
        .iter()
        .map(|column| column.name.as_str())
        .collect();
    assert_eq!(
        columns,
        [
            "content",
            "content_ref_local_file_content",
            "content_ref_local_file_id",
            "content_ref_random_pet",
            "filename"
        ]
    );
    assert!(file.column("content").unwrap().reference.is_none());
    let by_content = file
        .column("content_ref_local_file_content")
        .unwrap()
        .reference
        .as_ref()
        .unwrap();
    assert_eq!(by_content.table, "local_file");
    assert_eq!(by_content.attribute, "content");
    assert_eq!(by_content.sets.as_deref(), Some("content"));
    let by_id = file
        .column("content_ref_local_file_id")
        .unwrap()
        .reference
        .as_ref()
        .unwrap();
    assert_eq!(
        (by_id.table.as_str(), by_id.attribute.as_str()),
        ("local_file", "id")
    );
    let pet = file
        .column("content_ref_random_pet")
        .unwrap()
        .reference
        .as_ref()
        .unwrap();
    assert_eq!(
        (pet.table.as_str(), pet.attribute.as_str()),
        ("random_pet", "id")
    );

    // A number is numeric, a map is jsonb, and a sensitive attribute is
    // left out with the reason.
    let seed = migration.table("random_id").unwrap();
    assert_eq!(seed.column("byte_length").unwrap().kind.sql(), "numeric");
    assert_eq!(seed.column("prefix").unwrap().kind.sql(), "text");
    assert!(
        seed.column("hex").is_none(),
        "computed attributes are not columns"
    );
    let random_pet = migration.table("random_pet").unwrap();
    assert_eq!(random_pet.column("keepers").unwrap().kind.sql(), "jsonb");
    let secret = migration.table("local_sensitive_file").unwrap();
    assert!(secret.column("content").is_none());
    assert_eq!(migration.report.left_out.len(), 1);
    let left = &migration.report.left_out[0];
    assert_eq!(
        (
            left.resource_type.as_str(),
            left.attribute.as_str(),
            left.reason,
            left.rows
        ),
        (
            "local_sensitive_file",
            "content",
            LeftOutReason::Sensitive,
            1
        )
    );

    // Each reference names its row, its attribute and its evidence.
    let references: Vec<(String, String, String, String)> = migration
        .report
        .references
        .iter()
        .map(|note| {
            assert_eq!(note.evidence, Evidence::Configuration);
            (
                note.address.clone(),
                note.column.clone(),
                note.target.clone(),
                note.attribute.clone(),
            )
        })
        .collect();
    assert_eq!(
        references,
        [
            (
                "local_file.copy",
                "content_ref_local_file_content",
                "local_file.readme",
                "content"
            ),
            (
                "local_file.index",
                "content_ref_local_file_id",
                "module.notes[\"a\"].local_file.note",
                "id"
            ),
            (
                "module.notes[\"a\"].local_file.pet_card",
                "content_ref_random_pet",
                "module.notes[\"a\"].random_pet.pet",
                "id"
            ),
            (
                "module.notes[\"b\"].local_file.pet_card",
                "content_ref_random_pet",
                "module.notes[\"b\"].random_pet.pet",
                "id"
            ),
        ]
        .map(|(a, b, c, d)| (a.to_owned(), b.to_owned(), c.to_owned(), d.to_owned()))
    );

    // What refers to something but cannot be a reference is said.
    let constants: Vec<(String, String, String)> = migration
        .report
        .constants
        .iter()
        .map(|note| {
            (
                note.address.clone(),
                note.attribute.clone(),
                match &note.reason {
                    ConstantReason::Composed { targets } => {
                        format!("composed {}", targets.join(","))
                    }
                    ConstantReason::Collection { targets } => {
                        format!("collection {}", targets.join(","))
                    }
                    other => format!("{other:?}"),
                },
            )
        })
        .collect();
    assert_eq!(
        constants,
        [
            ("local_file.readme", "content", "composed random_id.seed"),
            (
                "module.notes[\"a\"].random_pet.pet",
                "keepers",
                "collection random_id.seed"
            ),
            (
                "module.notes[\"b\"].random_pet.pet",
                "keepers",
                "collection random_id.seed"
            ),
        ]
        .map(|(a, b, c)| (a.to_owned(), b.to_owned(), c.to_owned()))
    );

    // Rows: names, import ids, cells, and the order.
    let copy = migration
        .rows
        .iter()
        .find(|row| row.address == "local_file.copy")
        .unwrap();
    assert_eq!(copy.name, "copy");
    assert_eq!(
        copy.import_id.as_deref(),
        Some("f4cdef6fd6fe02765e97169f9fadb12f2e2fc9c2")
    );
    assert!(
        !copy.cells.contains_key("content"),
        "the referring row leaves the attribute null"
    );
    assert_eq!(
        copy.cells["content_ref_local_file_content"],
        Cell::Reference {
            target: "readme".into(),
            target_address: "local_file.readme".into(),
            attribute: "content".into(),
            evidence: Evidence::Configuration,
        }
    );
    assert_eq!(
        copy.cells["filename"],
        Cell::Constant("./out/copy.txt".into())
    );
    let names: Vec<&str> = migration.rows.iter().map(|row| row.name.as_str()).collect();
    assert!(names.contains(&"numbered-0") && names.contains(&"notes-b-pet_card"));
    let position = |address: &str| {
        migration
            .rows
            .iter()
            .position(|row| row.address == address)
            .unwrap()
    };
    assert!(position("local_file.readme") < position("local_file.copy"));
    assert!(position("module.notes[\"a\"].local_file.note") < position("local_file.index"));
    assert!(
        position("module.notes[\"b\"].random_pet.pet")
            < position("module.notes[\"b\"].local_file.pet_card")
    );
    assert!(migration.report.renamed.is_empty());
    assert!(migration.report.without_import_id.is_empty());
    assert!(migration.report.stacks.is_empty());
    assert_eq!(
        migration
            .rows
            .iter()
            .filter(|row| row.module.is_some())
            .count(),
        0
    );

    // Providers.
    let providers: Vec<(&str, &str)> = migration
        .providers
        .iter()
        .map(|provider| (provider.source.as_str(), provider.name.as_str()))
        .collect();
    assert_eq!(
        providers,
        [("hashicorp/local", "local"), ("hashicorp/random", "random")]
    );
    assert!(migration.report.provider_notes.is_empty());
}

#[test]
fn the_files_say_what_the_migration_decided() {
    let output = migrate(&inputs(true), &Settings::default()).unwrap();

    let schema = &output.schema_sql;
    assert!(schema.starts_with("-- schema.sql"));
    assert!(schema.contains("CREATE TABLE IF NOT EXISTS \"public\".\"local_file\" (\n    \"morph_name\" text PRIMARY KEY,\n    \"tf_address\" text NOT NULL UNIQUE,\n    \"tf_import_id\" text,\n    \"content\" text,"));
    assert!(schema.contains("\"byte_length\" numeric"));
    assert!(schema.contains("\"keepers\" jsonb"));
    assert!(schema.contains("ADD CONSTRAINT \"local_file_content_ref_random_pet_fkey\" FOREIGN KEY (\"content_ref_random_pet\") REFERENCES \"public\".\"random_pet\" (\"morph_name\")"));
    assert!(!schema.contains("tf_module"));
    assert!(!schema.contains("REPLICA IDENTITY"));
    assert!(schema.trim_end().ends_with("COMMIT;"));

    let rows = &output.rows_sql;
    assert!(rows.starts_with("-- rows.sql"));
    assert!(rows.contains("-- random_id.seed\nINSERT INTO \"public\".\"random_id\" (\"morph_name\", \"tf_address\", \"tf_import_id\", \"byte_length\", \"prefix\")\nVALUES (E'seed', E'random_id.seed', E'Ggg4Rw', 4, E'demo')\nON CONFLICT (\"tf_address\") DO UPDATE SET"));
    assert!(rows.contains("E'{\"seed\":\"Ggg4Rw\"}'::jsonb"));
    assert!(rows.contains("E'module.notes[\"a\"].local_file.note'"));
    assert!(
        !rows.contains("hush"),
        "the sensitive content is not in the rows"
    );
    assert_eq!(rows.matches("INSERT INTO").count(), 13);

    let toml_text = &output.controller_toml;
    let parsed: toml::Table = toml::from_str(toml_text).expect("valid TOML");
    assert_eq!(parsed["engine"]["kind"].as_str(), Some("morph"));
    let mappings = parsed["mappings"].as_array().unwrap();
    assert_eq!(mappings.len(), 4);
    let file = &mappings[0];
    assert_eq!(file["table"].as_str(), Some("local_file"));
    assert_eq!(file["resource_type"].as_str(), Some("local_file"));
    assert_eq!(file["id_column"].as_str(), Some(NAME_COLUMN));
    assert_eq!(file["import_column"].as_str(), Some(IMPORT_COLUMN));
    assert_eq!(
        file["exclude_columns"].as_array().unwrap(),
        &[toml::Value::from(ADDRESS_COLUMN)]
    );
    assert_eq!(file["lifecycle"]["prevent_destroy"].as_bool(), Some(true));
    let references = file["references"].as_array().unwrap();
    assert_eq!(references.len(), 3);
    assert_eq!(
        references[0]["column"].as_str(),
        Some("content_ref_local_file_content")
    );
    assert_eq!(references[0]["attribute"].as_str(), Some("content"));
    assert_eq!(references[0]["sets"].as_str(), Some("content"));
    assert!(
        references[1].get("attribute").is_none(),
        "the id is the default"
    );
    assert_eq!(references[2]["table"].as_str(), Some("random_pet"));
    let providers = parsed["providers"].as_array().unwrap();
    assert_eq!(providers.len(), 2);
    assert_eq!(providers[0]["kind"].as_str(), Some("terraform"));
    assert_eq!(providers[0]["source"].as_str(), Some("hashicorp/local"));
    assert_eq!(
        parsed["morph"]["state"]["backend"].as_str(),
        Some("postgres")
    );
    assert!(!toml_text.contains("hush"));

    let report = &output.report_md;
    assert!(report.starts_with("# Migration report"));
    for heading in [
        "## Summary",
        "## Tables",
        "### local_file",
        "## Rows",
        "## References",
        "## Kept as constants",
        "## Attributes left out",
        "## Data sources",
        "## Before the controller starts",
        "## Cutover",
    ] {
        assert!(report.contains(heading), "{heading}");
    }
    assert!(report.contains("| Tables | 4 |"));
    assert!(report.contains("| Rows | 13 |"));
    assert!(report.contains(
        "| content_ref_local_file_content | text | local_file (content), into content |"
    ));
    assert!(report.contains("terraform state rm 'module.notes[\"a\"].local_file.note'"));
    assert!(report.contains("`prevent_destroy`"));
    assert!(report.contains("Sensitive attributes were left out"));
    assert!(!report.contains("## Stacks"));
    assert!(!report.contains("## Names"));
}

#[test]
fn without_the_plan_the_schema_and_depends_on_decide_more_cautiously() {
    let output = migrate(&inputs(false), &Settings::default()).unwrap();
    let migration = &output.migration;
    assert_eq!(migration.rows.len(), 13);

    // Optional attributes that the provider may also compute are left
    // out, and reported once per type and attribute.
    let left: Vec<(&str, &str, LeftOutReason, usize)> = migration
        .report
        .left_out
        .iter()
        .map(|item| {
            (
                item.resource_type.as_str(),
                item.attribute.as_str(),
                item.reason,
                item.rows,
            )
        })
        .collect();
    assert_eq!(
        left,
        [
            (
                "local_file",
                "directory_permission",
                LeftOutReason::OptionalComputed,
                9
            ),
            (
                "local_file",
                "file_permission",
                LeftOutReason::OptionalComputed,
                9
            ),
            (
                "local_sensitive_file",
                "content",
                LeftOutReason::Sensitive,
                1
            ),
            (
                "local_sensitive_file",
                "directory_permission",
                LeftOutReason::OptionalComputed,
                1
            ),
            (
                "local_sensitive_file",
                "file_permission",
                LeftOutReason::OptionalComputed,
                1
            ),
            ("random_pet", "length", LeftOutReason::OptionalComputed, 2),
            (
                "random_pet",
                "separator",
                LeftOutReason::OptionalComputed,
                2
            ),
        ]
    );
    let file = migration.table("local_file").unwrap();
    let columns: Vec<&str> = file
        .columns
        .iter()
        .map(|column| column.name.as_str())
        .collect();
    assert_eq!(
        columns,
        [
            "content",
            "content_ref_local_file_content",
            "content_ref_local_file_id",
            "content_ref_random_pet",
            "filename"
        ],
        "depends_on and the values find the same references"
    );
    assert!(
        migration
            .report
            .references
            .iter()
            .all(|note| note.evidence == Evidence::Dependency),
        "{:?}",
        migration.report.references
    );
    assert_eq!(migration.report.references.len(), 4);
    assert!(
        migration.report.constants.is_empty(),
        "without the configuration nothing is known to be composed: {:?}",
        migration.report.constants
    );
    assert!(
        output
            .report_md
            .contains("give a plan to tell whether the configuration sets it")
    );
}

#[test]
fn the_settings_change_what_is_made() {
    // Pulumi: types from the mapping files, stacks from the modules, the
    // sensitive attribute kept, nothing protected, another schema.
    let settings = Settings {
        stacks_from_modules: true,
        include_sensitive: true,
        protect: false,
        pg_schema: "tf".into(),
        slot: "my_slot".into(),
        publication: "my_pub".into(),
        project: "legacy".into(),
        ..pulumi_settings()
    };
    let output = migrate(&inputs(true), &settings).unwrap();
    let migration = &output.migration;
    assert_eq!(
        migration
            .table("local_file")
            .unwrap()
            .pulumi_type
            .as_deref(),
        Some("local:index/file:File")
    );
    assert!(migration.report.unmapped_types.is_empty());
    assert_eq!(
        migration.report.stacks,
        [
            ("notes-a".to_owned(), 3),
            ("notes-b".to_owned(), 3),
            ("root".to_owned(), 7)
        ]
    );
    let note = migration
        .rows
        .iter()
        .find(|row| row.address == "module.notes[\"b\"].local_file.note")
        .unwrap();
    assert_eq!(note.module.as_deref(), Some("notes-b"));
    assert!(migration.report.left_out.is_empty());
    assert_eq!(
        migration
            .table("local_sensitive_file")
            .unwrap()
            .column("content")
            .unwrap()
            .kind
            .sql(),
        "text"
    );
    assert!(output.rows_sql.contains("E'hush'"));

    let schema = &output.schema_sql;
    assert!(schema.contains("CREATE SCHEMA IF NOT EXISTS \"tf\";"));
    assert!(schema.contains("\"tf\".\"local_file\""));
    assert!(schema.contains("\"tf_module\" text NOT NULL,"));
    assert!(schema.contains("ALTER TABLE \"tf\".\"random_pet\" REPLICA IDENTITY FULL;"));
    assert!(schema.contains("connamespace = E'tf'::regnamespace"));
    assert!(
        output
            .rows_sql
            .contains("E'notes-a', E'{\"seed\":\"Ggg4Rw\"}'::jsonb")
    );

    let parsed: toml::Table = toml::from_str(&output.controller_toml).unwrap();
    assert_eq!(parsed["replication"]["slot"].as_str(), Some("my_slot"));
    assert_eq!(
        parsed["replication"]["publication"].as_str(),
        Some("my_pub")
    );
    assert_eq!(parsed["engine"]["kind"].as_str(), Some("pulumi"));
    assert_eq!(parsed["engine"]["project"].as_str(), Some("legacy"));
    assert_eq!(
        parsed["engine"]["types"]["random_pet"].as_str(),
        Some("random:index/randomPet:RandomPet")
    );
    assert!(parsed.get("providers").is_none());
    let file = &parsed["mappings"].as_array().unwrap()[0];
    assert_eq!(file["schema"].as_str(), Some("tf"));
    assert_eq!(file["stack_column"].as_str(), Some("tf_module"));
    assert!(file.get("lifecycle").is_none());
    assert_eq!(
        file["references"].as_array().unwrap()[0]["schema"].as_str(),
        Some("tf")
    );
    assert!(output.report_md.contains("## Stacks"));
    assert!(
        output
            .report_md
            .contains("Pulumi type `local:index/file:File`")
    );
    assert!(!output.report_md.contains("`prevent_destroy`"));

    // Pulumi without the mapping files: the types are missing and said so.
    let bare = Settings {
        engine: EngineKind::Pulumi,
        ..Settings::default()
    };
    let output = migrate(&inputs(true), &bare).unwrap();
    assert_eq!(
        output.migration.report.unmapped_types,
        [
            "local_file",
            "local_sensitive_file",
            "random_id",
            "random_pet"
        ]
    );
    assert!(output.controller_toml.contains("# local_file = \"\""));
    assert!(output.report_md.contains("## Pulumi types missing"));
}

#[test]
fn the_address_filter_keeps_a_part_of_the_estate_and_reports_the_rest() {
    let settings = Settings {
        only: vec!["module.notes".into()],
        ..Settings::default()
    };
    let output = migrate(&inputs(true), &settings).unwrap();
    let migration = &output.migration;
    assert_eq!(migration.rows.len(), 6);
    assert_eq!(migration.report.filtered_out.len(), 7);
    assert!(
        migration
            .report
            .filtered_out
            .contains(&"random_id.seed".to_owned())
    );
    let tables: Vec<&str> = migration
        .tables
        .iter()
        .map(|table| table.resource_type.as_str())
        .collect();
    assert_eq!(tables, ["local_file", "random_pet"]);
    assert_eq!(
        migration.report.references.len(),
        2,
        "the pet cards still refer to their pets"
    );
    assert!(
        output
            .report_md
            .contains("## Left out by the address filter")
    );
    assert!(
        output
            .report_md
            .contains("| Managed resources left out by the address filter | 7 |")
    );

    let nothing = Settings {
        only: vec!["module.absent".into()],
        ..Settings::default()
    };
    let error = migrate(&inputs(true), &nothing).unwrap_err();
    assert!(
        matches!(error, Error::Nothing { ref reason } if reason.contains("admits none of the 13")),
        "{error}"
    );
}

#[test]
fn a_guessed_reference_needs_asking_for_and_a_name_collision_is_resolved() {
    // A state of two instances whose only tie is a value: one's attribute
    // equals the other's id. With nothing else to go on, it is a
    // reference only when guessing is asked for.
    let dir = std::env::temp_dir().join(format!("mtm-guess-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let state = serde_json::json!({
        "format_version": "1.0",
        "values": {"root_module": {"resources": [
            {"address": "local_file.a", "mode": "managed", "type": "local_file", "name": "a",
             "provider_name": "registry.terraform.io/hashicorp/local",
             "values": {"filename": "a.txt", "content": "x", "id": "sha-a"}},
            {"address": "local_file.b", "mode": "managed", "type": "local_file", "name": "b",
             "provider_name": "registry.terraform.io/hashicorp/local",
             "values": {"filename": "sha-a", "content": "y", "id": "sha-b"}},
            {"address": "module.a.local_file.b", "mode": "managed", "type": "local_file", "name": "b",
             "provider_name": "registry.terraform.io/hashicorp/local",
             "values": {"filename": "c.txt", "content": "z"}},
            {"address": "local_file.a-b", "mode": "managed", "type": "local_file", "name": "a-b",
             "provider_name": "registry.terraform.io/hashicorp/local",
             "values": {"filename": "d.txt", "content": "w", "id": "sha-d"}}
        ]}}
    });
    let state_path = dir.join("state.json");
    std::fs::write(&state_path, state.to_string()).unwrap();
    let loaded = Inputs::load(&state_path, &fixture("schema.json"), None).unwrap();

    let plain = migrate(&loaded, &Settings::default()).unwrap();
    assert!(plain.migration.report.references.is_empty());
    assert_eq!(
        plain.migration.report.without_import_id,
        ["module.a.local_file.b"]
    );
    // `module.a.local_file.b` and `local_file.a-b` both shorten to `a-b`.
    let renamed: Vec<(&str, &str, &str)> = plain
        .migration
        .report
        .renamed
        .iter()
        .map(|item| {
            (
                item.address.as_str(),
                item.name.as_str(),
                item.short.as_str(),
            )
        })
        .collect();
    assert_eq!(
        renamed,
        [
            ("module.a.local_file.b", "a-local_file-b", "a-b"),
            ("local_file.a-b", "local_file-a-b", "a-b"),
        ]
    );
    assert!(plain.report_md.contains("## Names"));
    assert!(plain.report_md.contains("## Without an import id"));

    let guessing = Settings {
        guess_references: true,
        ..Settings::default()
    };
    let guessed = migrate(&loaded, &guessing).unwrap();
    let notes = &guessed.migration.report.references;
    assert_eq!(notes.len(), 1);
    assert_eq!(notes[0].address, "local_file.b");
    assert_eq!(notes[0].column, "filename_ref");
    assert_eq!(notes[0].target, "local_file.a");
    assert_eq!(notes[0].evidence, Evidence::Value);
    assert!(guessed.report_md.contains("| value |"));
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn what_cannot_be_read_is_refused_with_the_file_named() {
    let dir = std::env::temp_dir().join(format!("mtm-refuse-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let missing = dir.join("missing.json");
    let error = Inputs::load(&missing, &fixture("schema.json"), None).unwrap_err();
    assert!(matches!(error, Error::Read { .. }), "{error}");
    assert!(error.to_string().contains("missing.json"));

    let bad = dir.join("bad.json");
    std::fs::write(&bad, "{not json").unwrap();
    let error = Inputs::load(&fixture("state.json"), &bad, None).unwrap_err();
    assert!(matches!(error, Error::Json { .. }), "{error}");

    let old = dir.join("old.json");
    std::fs::write(&old, r#"{"format_version": "0.2"}"#).unwrap();
    let error =
        Inputs::load(&fixture("state.json"), &fixture("schema.json"), Some(&old)).unwrap_err();
    assert!(
        matches!(error, Error::Format { ref reason, .. } if reason.contains("0.2")),
        "{error}"
    );

    // A state whose provider the schema does not know.
    let foreign = dir.join("foreign.json");
    std::fs::write(
        &foreign,
        r#"{"format_version": "1.0", "values": {"root_module": {"resources": [
            {"address": "aws_vpc.main", "mode": "managed", "type": "aws_vpc", "name": "main",
             "provider_name": "registry.terraform.io/hashicorp/aws", "values": {"id": "vpc-1"}}]}}}"#,
    )
    .unwrap();
    let loaded = Inputs::load(&foreign, &fixture("schema.json"), None).unwrap();
    let error = migrate(&loaded, &Settings::default()).unwrap_err();
    assert!(
        matches!(error, Error::Schema { ref resource_type, .. } if resource_type == "aws_vpc"),
        "{error}"
    );
    assert!(
        error
            .to_string()
            .contains("terraform providers schema -json")
    );

    // A state with nothing managed.
    let empty = dir.join("empty.json");
    std::fs::write(&empty, r#"{"format_version": "1.0"}"#).unwrap();
    let loaded = Inputs::load(&empty, &fixture("schema.json"), None).unwrap();
    let error = migrate(&loaded, &Settings::default()).unwrap_err();
    assert!(matches!(error, Error::Nothing { .. }), "{error}");

    // A bad address.
    let odd = dir.join("odd.json");
    std::fs::write(
        &odd,
        r#"{"format_version": "1.0", "values": {"root_module": {"resources": [
            {"address": "local_file", "mode": "managed", "type": "local_file", "name": "x",
             "provider_name": "registry.terraform.io/hashicorp/local", "values": {}}]}}}"#,
    )
    .unwrap();
    let loaded = Inputs::load(&odd, &fixture("schema.json"), None).unwrap();
    let error = migrate(&loaded, &Settings::default()).unwrap_err();
    assert!(matches!(error, Error::Address { .. }), "{error}");

    // A value PostgreSQL cannot hold.
    let nul = dir.join("nul.json");
    std::fs::write(
        &nul,
        "{\"format_version\": \"1.0\", \"values\": {\"root_module\": {\"resources\": [
            {\"address\": \"local_file.x\", \"mode\": \"managed\", \"type\": \"local_file\", \"name\": \"x\",
             \"provider_name\": \"registry.terraform.io/hashicorp/local\", \"values\": {\"filename\": \"a\\u0000b\", \"id\": \"1\"}}]}}}",
    )
    .unwrap();
    let loaded = Inputs::load(&nul, &fixture("schema.json"), None).unwrap();
    let error = migrate(&loaded, &Settings::default()).unwrap_err();
    assert!(
        matches!(error, Error::Value { ref attribute, .. } if attribute == "filename"),
        "{error}"
    );

    // Writing into a place that is a file.
    let output = migrate(&inputs(true), &Settings::default()).unwrap();
    let error = output.write_to(&bad).unwrap_err();
    assert!(matches!(error, Error::Write { .. }), "{error}");
    let written = output.write_to(&dir.join("out")).unwrap();
    assert_eq!(written.len(), 4);
    assert!(written.iter().all(|path| path.is_file()));
    // A directory where a file should be.
    let blocked = dir.join("blocked");
    std::fs::create_dir_all(blocked.join("schema.sql")).unwrap();
    let error = output.write_to(&blocked).unwrap_err();
    assert!(
        matches!(error, Error::Write { ref path, .. } if path.ends_with("schema.sql")),
        "{error}"
    );
    std::fs::remove_dir_all(&dir).unwrap();
}
