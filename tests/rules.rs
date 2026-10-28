//! Every rule of the analysis, on states and plans made for the purpose.
//!
//! The real fixtures of `tests/migrate.rs` show the common cases. These
//! scenarios are written by hand, in the same JSON shapes Terraform
//! writes, to reach each rule that a small real project does not: nested
//! blocks, reserved and unknown attribute names, references through
//! nested modules and by instance key, targets outside the migration,
//! provider settings, name collisions three deep, a reference circle, a
//! table without attributes, and values PostgreSQL cannot hold. Run with
//! `cargo test --test rules`.

use morph_terraform_migrate::model::{Cell, ConstantReason, Evidence, LeftOutReason};
use morph_terraform_migrate::terraform::plan::{Configuration, Resolution, expression};
use morph_terraform_migrate::{EngineKind, Error, Inputs, Settings, migrate};
use serde_json::{Value, json};

const PROVIDER: &str = "registry.terraform.io/acme/demo";

/// Inputs from JSON values in the shapes Terraform writes.
fn inputs(state: Value, schemas: Value, plan: Option<Value>) -> Inputs {
    Inputs {
        state: serde_json::from_value(state).unwrap(),
        schemas: serde_json::from_value(schemas).unwrap(),
        plan: plan.map(|plan| serde_json::from_value(plan).unwrap()),
    }
}

/// A schema file for the provider `acme/demo` with these resource types,
/// and a provider block with a `region` and a sensitive `token`.
fn schemas(types: Vec<(&str, Value)>) -> Value {
    let mut resource_schemas = serde_json::Map::new();
    for (name, block) in types {
        resource_schemas.insert(name.to_owned(), json!({"version": 0, "block": block}));
    }
    json!({
        "format_version": "1.0",
        "provider_schemas": {
            PROVIDER: {
                "provider": {"version": 0, "block": {"attributes": {
                    "region": {"type": "string", "optional": true},
                    "profile": {"type": "string", "optional": true},
                    "token": {"type": "string", "optional": true, "sensitive": true},
                    "secret_key": {"type": "string", "optional": true},
                    "endpoint": {"type": "string", "optional": true}
                }, "block_types": {"assume_role": {"nesting_mode": "list", "block": {"attributes": {"role_arn": {"type": "string", "optional": true}}}}}}},
                "resource_schemas": resource_schemas
            }
        }
    })
}

/// The schema of `demo_box`: a resource with every kind of attribute.
fn demo_box() -> Value {
    json!({
        "attributes": {
            "id": {"type": "string", "computed": true},
            "name": {"type": "string", "required": true},
            "enabled": {"type": "bool", "optional": true},
            "size": {"type": "number", "optional": true},
            "peer": {"type": "string", "optional": true},
            "peer_ref": {"type": "string", "optional": true},
            "morph_name": {"type": "string", "optional": true},
            "secret": {"type": "string", "optional": true, "sensitive": true},
            "tags": {"type": ["map", "string"], "optional": true}
        },
        "block_types": {
            "disk": {"nesting_mode": "list", "block": {"attributes": {"gb": {"type": "number", "optional": true}}}},
            "volume": {"nesting_mode": "list", "block": {"attributes": {"gb": {"type": "number", "optional": true}}}},
            "extra": {"nesting_mode": "list", "block": {"attributes": {"gb": {"type": "number", "optional": true}}}},
            "net": {"nesting_mode": "single", "block": {"attributes": {"cidr": {"type": "string", "optional": true}}}},
            "weird": {"nesting_mode": "single", "block": {"attributes": {"x": {"type": "string", "optional": true}}}},
            "shape": {"nesting_mode": "single", "block": {"attributes": {"x": {"type": "string", "optional": true}}}},
            "tf_import_id": {"nesting_mode": "list", "block": {"attributes": {"x": {"type": "string", "optional": true}}}}
        }
    })
}

/// A managed instance of `demo_box` at `address` with these values, and
/// whatever `extra` adds to the object: `depends_on`, `sensitive_values`.
fn instance(address: &str, resource_type: &str, values: Value, extra: Value) -> Value {
    let name = address
        .rsplit('.')
        .next()
        .unwrap()
        .split('[')
        .next()
        .unwrap();
    let mut object = json!({
        "address": address,
        "mode": "managed",
        "type": resource_type,
        "name": name,
        "provider_name": PROVIDER,
        "schema_version": 0,
        "values": values,
        "sensitive_values": {}
    });
    if let Value::Object(fields) = extra {
        for (key, value) in fields {
            object[key] = value;
        }
    }
    object
}

/// A resource block of the configuration.
fn block(address: &str, resource_type: &str, expressions: Value) -> Value {
    let name = address.rsplit('.').next().unwrap();
    json!({
        "address": address,
        "mode": "managed",
        "type": resource_type,
        "name": name,
        "provider_config_key": "demo",
        "expressions": expressions,
        "schema_version": 0
    })
}

fn state(root: Vec<Value>, children: Vec<Value>) -> Value {
    json!({
        "format_version": "1.0",
        "terraform_version": "1.16.7",
        "values": {"root_module": {"resources": root, "child_modules": children}}
    })
}

fn child(address: &str, resources: Vec<Value>, children: Vec<Value>) -> Value {
    json!({"address": address, "resources": resources, "child_modules": children})
}

fn plan(root_module: Value, variables: Value) -> Value {
    json!({
        "format_version": "1.0",
        "variables": variables,
        "configuration": {
            "provider_config": {"demo": {"name": "demo", "full_name": PROVIDER, "expressions": {
                "region": {"constant_value": "eu-west-1"},
                "profile": {"references": ["var.profile"]},
                "token": {"constant_value": "t0ken"},
                "secret_key": {"constant_value": "s3cret"},
                "endpoint": {"references": ["local.endpoint"]},
                "assume_role": [{"role_arn": {"constant_value": "arn:role"}}]
            }}},
            "root_module": root_module
        }
    })
}

#[test]
fn blocks_reserved_names_unknown_attributes_and_booleans_follow_the_rules() {
    let values = json!({
        "id": 42,
        "name": "box",
        "enabled": true,
        "size": 2,
        "morph_name": "taken",
        "secret": "hush",
        "tags": null,
        "disk": [{"gb": 10}],
        "volume": [{"gb": 20}],
        "extra": [{"gb": 30}],
        "net": null,
        "shape": {},
        "weird": "not a block at all",
        "tf_import_id": [{"x": "y"}],
        "mystery": "?"
    });
    let sensitive =
        json!({"sensitive_values": {"disk": [{"gb": true}], "secret": true, "size": 7}});
    let configured = plan(
        json!({"resources": [block("demo_box.one", "demo_box", {
            let mut expressions = json!({
                "name": {"constant_value": "box"},
                "enabled": {"constant_value": true},
                "size": {"constant_value": 2},
                "morph_name": {"constant_value": "taken"},
                "secret": {"constant_value": "hush"},
                "disk": [{"gb": {"constant_value": 10}}],
                "volume": [{"gb": {"constant_value": 20}}],
                "weird": {"x": {"constant_value": "?"}},
                "tf_import_id": [{"x": {"constant_value": "y"}}]
            });
            expressions.as_object_mut().unwrap();
            expressions
        })]}),
        json!({"profile": {"value": "dev"}}),
    );
    let loaded = inputs(
        state(
            vec![instance(
                "demo_box.one",
                "demo_box",
                values.clone(),
                sensitive.clone(),
            )],
            vec![],
        ),
        schemas(vec![("demo_box", demo_box())]),
        Some(configured),
    );
    let output = migrate(&loaded, &Settings::default()).unwrap();
    let migration = &output.migration;
    let table = migration.table("demo_box").unwrap();
    let columns: Vec<(&str, &str)> = table
        .columns
        .iter()
        .map(|column| (column.name.as_str(), column.kind.sql()))
        .collect();
    assert_eq!(
        columns,
        [
            ("enabled", "boolean"),
            ("name", "text"),
            ("size", "numeric"),
            ("volume", "jsonb"),
            ("weird", "jsonb")
        ],
        "a block set by the configuration is a jsonb column, whatever it holds; one not set, or empty, is not"
    );
    assert!(output.rows_sql.contains("E'\"not a block at all\"'::jsonb"));
    let left: Vec<(&str, LeftOutReason)> = migration
        .report
        .left_out
        .iter()
        .map(|item| (item.attribute.as_str(), item.reason))
        .collect();
    assert_eq!(
        left,
        [
            ("disk", LeftOutReason::Sensitive),
            ("morph_name", LeftOutReason::ReservedName),
            ("secret", LeftOutReason::Sensitive),
            ("tf_import_id", LeftOutReason::ReservedName),
        ]
    );
    assert_eq!(migration.report.unknown_attributes, ["demo_box.mystery"]);
    let row = &migration.rows[0];
    assert_eq!(
        row.import_id.as_deref(),
        Some("42"),
        "a number names the object by its text"
    );
    assert_eq!(row.cells["enabled"], Cell::Constant(true.into()));
    assert!(output.schema_sql.contains("\"enabled\" boolean"));
    assert!(output.rows_sql.contains(", TRUE, "));
    assert!(output.rows_sql.contains("E'[{\"gb\":20}]'::jsonb"));
    assert!(
        output
            .report_md
            .contains("its name is one of the control columns")
    );
    assert!(
        output
            .report_md
            .contains("## Attributes the schema does not know")
    );
    assert!(output.report_md.contains("- demo_box.mystery"));

    // Provider settings: constants and root variables are carried, the
    // rest is said.
    let demo = &migration.providers[0];
    assert_eq!(demo.source, "acme/demo");
    assert_eq!(demo.name, "demo");
    assert_eq!(demo.config["region"], "eu-west-1");
    assert_eq!(demo.config["profile"], "dev");
    assert_eq!(demo.config.len(), 2);
    assert_eq!(
        migration.report.provider_notes,
        [
            "demo: the setting assume_role is a nested block and was not carried over",
            "demo: the setting endpoint is an expression that could not be evaluated and was not carried over",
            "demo: the setting secret_key is sensitive and was not carried over",
            "demo: the setting token is sensitive and was not carried over",
        ]
    );
    assert!(
        output
            .controller_toml
            .contains("[providers.config]\nprofile = \"dev\"\nregion = \"eu-west-1\"\n")
    );
    assert!(
        !output.controller_toml.contains("t0ken") && !output.controller_toml.contains("s3cret")
    );
    assert!(output.report_md.contains("## Provider settings"));
    let pulumi = migrate(
        &loaded,
        &Settings {
            engine: EngineKind::Pulumi,
            ..Settings::default()
        },
    )
    .unwrap();
    assert!(
        pulumi.controller_toml.contains(
            "[engine.config]\n\"demo:profile\" = \"dev\"\n\"demo:region\" = \"eu-west-1\"\n"
        ),
        "{}",
        pulumi.controller_toml
    );

    // Without the plan, every block with something in it is a column.
    let bare = inputs(
        state(
            vec![instance("demo_box.one", "demo_box", values, sensitive)],
            vec![],
        ),
        schemas(vec![("demo_box", demo_box())]),
        None,
    );
    let output = migrate(&bare, &Settings::default()).unwrap();
    let columns: Vec<&str> = output
        .migration
        .table("demo_box")
        .unwrap()
        .columns
        .iter()
        .map(|column| column.name.as_str())
        .collect();
    assert_eq!(
        columns,
        ["enabled", "extra", "name", "size", "volume", "weird"]
    );
    assert!(output.migration.providers[0].config.is_empty());
    assert!(output.migration.report.provider_notes.is_empty());
}

#[test]
fn references_are_found_by_key_through_nested_modules_and_outside_the_migration() {
    // Root: two boxes under count, a box that names one of them by key, a
    // box that names the resource without a key, one whose target is left
    // out, and one that reads a module's output through another module.
    // The box n[0] names n[1] without a key, which Terraform itself would
    // refuse as a circle, but the rule for passing over the row's own
    // instance holds all the same. The box n[1] refers through a local,
    // which the configuration cannot follow, and its depends_on names
    // only its own resource.
    let root_resources = vec![
        instance(
            "demo_box.n[0]",
            "demo_box",
            json!({"id": "n0", "name": "n0", "peer": "n1"}),
            json!({"index": 0}),
        ),
        instance(
            "demo_box.n[1]",
            "demo_box",
            json!({"id": "n1", "name": "n1", "peer": "nothing-like-it"}),
            json!({"index": 1, "depends_on": ["demo_box.n"]}),
        ),
        instance(
            "demo_box.m",
            "demo_box",
            json!({"id": "m", "name": "m", "peer": "n1"}),
            json!({"depends_on": ["demo_box.n"]}),
        ),
        instance(
            "demo_box.k",
            "demo_box",
            json!({"id": "k", "name": "k", "peer": null}),
            json!({}),
        ),
        instance(
            "demo_box.far",
            "demo_box",
            json!({"id": "far", "name": "far", "peer": "gone", "tags": {"owner": "x"}}),
            json!({"depends_on": ["demo_box.gone"]}),
        ),
        instance(
            "demo_box.gone",
            "demo_box",
            json!({"id": "gone", "name": "gone"}),
            json!({}),
        ),
        instance(
            "demo_box.top",
            "demo_box",
            json!({"id": "top", "name": "top", "peer": "deep", "peer_ref": "mid"}),
            json!({"depends_on": ["module.outer.module.inner.demo_box.deep", "module.outer.demo_box.mid"]}),
        ),
        instance(
            "demo_box.orphan",
            "demo_box",
            json!({"id": "orphan", "name": "orphan", "peer": "n1"}),
            json!({"depends_on": ["demo_box.gone", "demo_box.n"]}),
        ),
        instance(
            "demo_box.loose",
            "demo_box",
            json!({"id": "loose", "name": "loose", "peer": "n1"}),
            json!({}),
        ),
        instance(
            "demo_link.a",
            "demo_link",
            json!({"id": "la", "box": "n0"}),
            json!({"depends_on": ["demo_box.n"]}),
        ),
        instance(
            "demo_link.b",
            "demo_link",
            json!({"id": "lb", "box": "m"}),
            json!({"depends_on": ["demo_box.m"]}),
        ),
    ];
    let outer = child(
        "module.outer",
        vec![instance(
            "module.outer.demo_box.mid",
            "demo_box",
            json!({"id": "mid", "name": "mid", "peer": "deep"}),
            json!({"depends_on": ["module.outer.module.inner.demo_box.deep"]}),
        )],
        vec![child(
            "module.outer.module.inner",
            vec![instance(
                "module.outer.module.inner.demo_box.deep",
                "demo_box",
                json!({"id": "deep", "name": "deep"}),
                json!({}),
            )],
            vec![],
        )],
    );
    let demo_link = json!({"attributes": {
        "id": {"type": "string", "computed": true},
        "box": {"type": "string", "required": true}
    }});
    let configuration = json!({
        "resources": [
            block("demo_box.n", "demo_box", json!({"name": {"references": ["count.index"]}, "peer": {"references": ["demo_box.n", "local.other"]}})),
            block("demo_box.m", "demo_box", json!({"name": {"constant_value": "m"}, "peer": {"references": ["demo_box.n[1].id", "demo_box.n[1]", "demo_box.n"]}})),
            block("demo_box.k", "demo_box", json!({"name": {"constant_value": "k"}})),
            block("demo_box.far", "demo_box", json!({"name": {"constant_value": "far"}, "peer": {"references": ["demo_box.gone.id", "demo_box.gone"]}, "tags": {"references": ["var.tags"]}})),
            block("demo_box.gone", "demo_box", json!({"name": {"constant_value": "gone"}})),
            block("demo_box.top", "demo_box", json!({"name": {"constant_value": "top"}, "peer": {"references": ["module.outer.deep_out", "module.outer"]}, "peer_ref": {"references": ["module.outer"]}})),
            block("demo_link.a", "demo_link", json!({"box": {"references": ["demo_box.n[0].id", "demo_box.n[0]", "demo_box.n"]}})),
            block("demo_link.b", "demo_link", json!({"box": {"references": ["demo_box.m.id", "demo_box.m"]}}))
        ],
        "module_calls": {"outer": {
            "source": "./outer",
            "expressions": {},
            "module": {
                "resources": [block("demo_box.mid", "demo_box", json!({"name": {"constant_value": "mid"}, "peer": {"references": ["module.inner.deep_id", "module.inner"]}}))],
                "outputs": {
                    "deep_out": {"expression": {"references": ["module.inner.deep_id", "module.inner"]}},
                    "mid_out": {"expression": {"references": ["demo_box.mid.id", "demo_box.mid"]}}
                },
                "module_calls": {"inner": {
                    "source": "./inner",
                    "expressions": {},
                    "module": {
                        "resources": [block("demo_box.deep", "demo_box", json!({"name": {"constant_value": "deep"}}))],
                        "outputs": {"deep_id": {"expression": {"references": ["demo_box.deep.id", "demo_box.deep"]}}}
                    }
                }}
            }
        }}
    });
    let loaded = inputs(
        state(root_resources, vec![outer]),
        schemas(vec![("demo_box", demo_box()), ("demo_link", demo_link)]),
        Some(plan(
            configuration,
            json!({"tags": {"value": {"owner": "x"}}}),
        )),
    );
    let settings = Settings {
        only: vec![
            "demo_box.n".into(),
            "demo_box.m".into(),
            "demo_box.k".into(),
            "demo_box.far".into(),
            "demo_box.top".into(),
            "demo_box.orphan".into(),
            "demo_box.loose".into(),
            "demo_link".into(),
            "module.outer".into(),
        ],
        ..Settings::default()
    };
    let output = migrate(&loaded, &settings).unwrap();
    let migration = &output.migration;
    assert_eq!(migration.report.filtered_out, ["demo_box.gone"]);

    let references: Vec<(&str, &str, &str, &str, Evidence)> = migration
        .report
        .references
        .iter()
        .map(|note| {
            (
                note.address.as_str(),
                note.column.as_str(),
                note.target.as_str(),
                note.attribute.as_str(),
                note.evidence,
            )
        })
        .collect();
    assert_eq!(
        references,
        [
            (
                "demo_box.n[0]",
                "peer_ref_x",
                "demo_box.n[1]",
                "id",
                Evidence::Configuration
            ),
            (
                "demo_box.m",
                "peer_ref_x",
                "demo_box.n[1]",
                "id",
                Evidence::Configuration
            ),
            (
                "demo_box.orphan",
                "peer_ref_x",
                "demo_box.n[1]",
                "id",
                Evidence::Dependency
            ),
            (
                "demo_link.a",
                "box",
                "demo_box.n[0]",
                "id",
                Evidence::Configuration
            ),
            (
                "demo_link.b",
                "box",
                "demo_box.m",
                "id",
                Evidence::Configuration
            ),
            (
                "demo_box.top",
                "peer_ref_x",
                "module.outer.module.inner.demo_box.deep",
                "id",
                Evidence::Configuration
            ),
            (
                "module.outer.demo_box.mid",
                "peer_ref_x",
                "module.outer.module.inner.demo_box.deep",
                "id",
                Evidence::Configuration
            ),
        ]
    );
    // The orphan's value equals both n[0]'s peer and n[1]'s id: the id
    // wins. The companion for `peer` could not be called `peer_ref`, since
    // the type has an attribute of that name.
    let table = migration.table("demo_box").unwrap();
    assert!(table.column("peer_ref").unwrap().reference.is_none());
    let companion = table
        .column("peer_ref_x")
        .unwrap()
        .reference
        .as_ref()
        .unwrap();
    assert_eq!(companion.sets.as_deref(), Some("peer"));
    // A column that is a reference in every row stands for itself.
    let link = migration.table("demo_link").unwrap();
    let column = link.column("box").unwrap();
    assert_eq!(column.kind.sql(), "text");
    let reference = column.reference.as_ref().unwrap();
    assert_eq!(
        (
            reference.table.as_str(),
            reference.attribute.as_str(),
            reference.sets.as_deref()
        ),
        ("demo_box", "id", None)
    );
    assert!(
        output.schema_sql.contains(
            "\"box\" text /* names a row of demo_box; the controller puts its id here */"
        )
    );
    assert!(output.report_md.contains("| box | text | demo_box (id) |"));
    let parsed: toml::Table = toml::from_str(&output.controller_toml).unwrap();
    let mappings = parsed["mappings"].as_array().unwrap();
    let link_mapping = mappings
        .iter()
        .find(|mapping| mapping["table"].as_str() == Some("demo_link"))
        .unwrap();
    assert!(
        link_mapping.get("references").is_none(),
        "the foreign key says it all"
    );

    let constants: Vec<(&str, &str, String)> = migration
        .report
        .constants
        .iter()
        .map(|note| {
            (
                note.address.as_str(),
                note.attribute.as_str(),
                note.reason.to_string(),
            )
        })
        .collect();
    assert_eq!(
        constants,
        [(
            "demo_box.far",
            "peer",
            "refers to demo_box.gone, which is not in the migration".to_owned()
        )]
    );
    let n1 = migration
        .rows
        .iter()
        .find(|row| row.address == "demo_box.n[1]")
        .unwrap();
    assert_eq!(n1.cells["peer"], Cell::Constant("nothing-like-it".into()));
    let loose = migration
        .rows
        .iter()
        .find(|row| row.address == "demo_box.loose")
        .unwrap();
    assert_eq!(
        loose.cells["peer"],
        Cell::Constant("n1".into()),
        "a row the configuration does not know, with no depends_on, refers to nothing without guessing"
    );
    let top = migration
        .rows
        .iter()
        .find(|row| row.address == "demo_box.top")
        .unwrap();
    assert_eq!(
        top.cells["peer_ref"],
        Cell::Constant("mid".into()),
        "a module reference without an output is nothing"
    );
    let position = |address: &str| {
        migration
            .rows
            .iter()
            .position(|row| row.address == address)
            .unwrap()
    };
    assert!(position("demo_box.n[1]") < position("demo_box.n[0]"));
    assert!(position("module.outer.module.inner.demo_box.deep") < position("demo_box.top"));
    assert!(position("demo_box.m") < position("demo_link.b"));
}

#[test]
fn names_that_collide_three_deep_are_numbered_and_a_circle_is_broken() {
    // Both modules shorten to `a-b`, and with the type in it both still
    // read `a-b-demo_box-c`.
    let modules = vec![
        child(
            "module.a-b",
            vec![instance(
                "module.a-b.demo_box.c",
                "demo_box",
                json!({"id": "c1", "name": "c1", "peer": "c2"}),
                json!({}),
            )],
            vec![],
        ),
        child(
            "module.a[\"b\"]",
            vec![instance(
                "module.a[\"b\"].demo_box.c",
                "demo_box",
                json!({"id": "c2", "name": "c2", "peer": "c1"}),
                json!({}),
            )],
            vec![],
        ),
    ];
    let loaded = inputs(
        state(
            vec![instance(
                "demo_box.alone",
                "demo_box",
                json!({"id": "alone", "name": "alone"}),
                json!({}),
            )],
            modules,
        ),
        schemas(vec![("demo_box", demo_box())]),
        None,
    );
    let output = migrate(
        &loaded,
        &Settings {
            guess_references: true,
            ..Settings::default()
        },
    )
    .unwrap();
    let migration = &output.migration;
    let names: Vec<(&str, &str)> = migration
        .rows
        .iter()
        .map(|row| (row.address.as_str(), row.name.as_str()))
        .collect();
    assert_eq!(
        names,
        [
            ("demo_box.alone", "alone"),
            ("module.a-b.demo_box.c", "a-b-demo_box-c"),
            ("module.a[\"b\"].demo_box.c", "a-b-demo_box-c-2"),
        ],
        "the row outside the circle comes first"
    );
    assert_eq!(migration.report.renamed.len(), 2);
    // Each names the other by value: a circle, which guessing alone can
    // make, so both stay constants.
    assert!(migration.report.references.is_empty());
    let reasons: Vec<(&str, ConstantReason)> = migration
        .report
        .constants
        .iter()
        .map(|note| (note.address.as_str(), note.reason.clone()))
        .collect();
    assert_eq!(
        reasons,
        [
            ("module.a-b.demo_box.c", ConstantReason::Cycle),
            ("module.a[\"b\"].demo_box.c", ConstantReason::Cycle),
        ]
    );
    assert!(
        output
            .report_md
            .contains("would make a circle of references")
    );
    assert_eq!(migration.rows[1].cells["peer"], Cell::Constant("c2".into()));
}

#[test]
fn a_type_without_attributes_and_a_value_postgres_cannot_hold() {
    let ghost = json!({"attributes": {
        "id": {"type": "string", "computed": true},
        "arn": {"type": "string", "computed": true}
    }});
    let loaded = inputs(
        state(
            vec![instance(
                "demo_ghost.g",
                "demo_ghost",
                json!({"id": "g", "arn": "arn:g"}),
                json!({}),
            )],
            vec![],
        ),
        schemas(vec![("demo_ghost", ghost)]),
        None,
    );
    let output = migrate(&loaded, &Settings::default()).unwrap();
    assert!(
        output
            .migration
            .table("demo_ghost")
            .unwrap()
            .columns
            .is_empty()
    );
    assert!(
        output
            .report_md
            .contains("No attribute columns: nothing of this type is set by the configuration.")
    );
    assert!(output.schema_sql.contains("\"tf_import_id\" text\n);"));
    assert!(output.rows_sql.contains(
        "(\"morph_name\", \"tf_address\", \"tf_import_id\")\nVALUES (E'g', E'demo_ghost.g', E'g')"
    ));

    // A number column whose state value is text with a NUL in it.
    let loaded = inputs(
        state(
            vec![instance(
                "demo_box.one",
                "demo_box",
                json!({"id": "1", "name": "n", "size": "1\u{0}"}),
                json!({}),
            )],
            vec![],
        ),
        schemas(vec![("demo_box", demo_box())]),
        None,
    );
    let error = migrate(&loaded, &Settings::default()).unwrap_err();
    assert!(
        matches!(error, Error::Value { ref attribute, .. } if attribute == "size"),
        "{error}"
    );

    // An identifier with a NUL in it cannot be an import id.
    let loaded = inputs(
        state(
            vec![instance(
                "demo_box.one",
                "demo_box",
                json!({"id": "1\u{0}", "name": "n"}),
                json!({}),
            )],
            vec![],
        ),
        schemas(vec![("demo_box", demo_box())]),
        None,
    );
    let error = migrate(&loaded, &Settings::default()).unwrap_err();
    assert!(
        matches!(error, Error::Value { ref attribute, .. } if attribute == "tf_import_id"),
        "{error}"
    );
}

#[test]
fn following_references_stops_at_a_circle_of_modules_and_at_what_cannot_be_read() {
    // The module's output reads its own input, which the call fills from
    // the output: a circle Terraform would refuse. Following it ends at
    // the depth limit and is opaque.
    let configuration: Configuration = serde_json::from_value(json!({
        "root_module": {
            "resources": [block("demo_box.r", "demo_box", json!({
                "peer": {"references": ["module.loop.o", "module.loop"]},
                "name": {"references": ["demo_box..bad"]},
                "size": {"references": ["var"]},
                "enabled": {"references": ["demo_box"]},
                "tags": {"references": ["module.loop"]}
            }))],
            "module_calls": {"loop": {
                "expressions": {"v": {"references": ["module.loop.o", "module.loop"]}},
                "module": {
                    "resources": [],
                    "outputs": {"o": {"expression": {"references": ["var.v"]}}},
                    "module_calls": {"loop": {
                        "expressions": {"v": {"references": ["var.v"]}},
                        "module": {"outputs": {"o": {"expression": {"references": ["module.loop.o"]}}},
                                   "module_calls": {"loop": {"expressions": {}, "module": {"outputs": {"o": {"expression": {"references": ["var.v"]}}}}}}}
                    }}
                }
            }}
        }
    }))
    .unwrap();
    let located = configuration.locate("demo_box.r").unwrap();
    let peer = expression(&located.resource.expressions["peer"]);
    let resolved = configuration.resolve(&located, &peer.references);
    assert!(
        resolved.opaque && resolved.targets.is_empty(),
        "{resolved:?}"
    );
    let name = expression(&located.resource.expressions["name"]);
    assert!(configuration.resolve(&located, &name.references).opaque);
    let size = expression(&located.resource.expressions["size"]);
    assert_eq!(
        configuration.resolve(&located, &size.references),
        Resolution::default()
    );
    let tags = expression(&located.resource.expressions["tags"]);
    assert_eq!(
        configuration.resolve(&located, &tags.references),
        Resolution::default()
    );
    let enabled = expression(&located.resource.expressions["enabled"]);
    assert_eq!(
        configuration.resolve(&located, &enabled.references),
        Resolution::default()
    );
}
