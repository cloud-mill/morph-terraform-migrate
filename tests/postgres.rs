//! The files loaded into PostgreSQL and read back by the controller.
//!
//! Needs a database: set `MTM_TEST_DATABASE_URL` to a PostgreSQL
//! connection URL, such as the one the controller's own tests use, and
//! run `cargo test --test postgres`. Without it the test passes without
//! doing anything. The tables go into a schema of the test's own, which
//! is dropped at the end.
//!
//! The migration is made from the fixtures, `schema.sql` and `rows.sql`
//! are run, twice, and then the controller's own crate is asked to parse
//! and validate `controller.toml`, to discover the references between the
//! tables, and to read the rows as it would when reconciling them.
//!
//! The second test goes to the end: the fixture's `random_id.seed` row
//! alone is migrated, loaded, given the prefix in its import id as the
//! report says to, and handed to the real controller with the real
//! `hashicorp/random` provider from Morph's cache. The controller takes
//! charge of the object, changes nothing, and its record holds the hex
//! Terraform's state held. That test also needs the provider in
//! `../morph/target/providers`, and says that it was skipped otherwise.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use morph_controller_core::Source;
use morph_controller_core::mapping::desired::spec_of_row;
use morph_postgres_controller::config::Config;
use morph_postgres_controller::postgres::database::Database;
use morph_postgres_controller::references::discover;
use morph_postgres_controller::{Controller, Event, PostgresSource};
use morph_terraform_migrate::{Inputs, Settings, migrate};

fn fixture(name: &str) -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

#[tokio::test]
async fn the_files_load_twice_and_the_controller_reads_what_was_meant() {
    let Ok(url) = std::env::var("MTM_TEST_DATABASE_URL") else {
        eprintln!("MTM_TEST_DATABASE_URL is not set; nothing to do");
        return;
    };
    let schema = format!("mtm_{}", std::process::id());
    let settings = Settings {
        pg_schema: schema.clone(),
        ..Settings::default()
    };
    let inputs = Inputs::load(
        &fixture("state.json"),
        &fixture("schema.json"),
        Some(&fixture("plan.json")),
    )
    .unwrap();
    let output = migrate(&inputs, &settings).unwrap();

    let (client, connection) = tokio_postgres::connect(&url, tokio_postgres::NoTls)
        .await
        .expect("the database is reachable");
    tokio::spawn(connection);
    let _ = client
        .batch_execute(&format!("DROP SCHEMA IF EXISTS \"{schema}\" CASCADE"))
        .await;

    // Loading twice changes nothing the second time.
    for _ in 0..2 {
        client
            .batch_execute(&output.schema_sql)
            .await
            .expect("schema.sql runs");
        client
            .batch_execute(&output.rows_sql)
            .await
            .expect("rows.sql runs");
    }
    let count = |table: &'static str| {
        let client = &client;
        let schema = schema.clone();
        async move {
            let row = client
                .query_one(
                    &format!("SELECT count(*) FROM \"{schema}\".\"{table}\""),
                    &[],
                )
                .await
                .unwrap();
            row.get::<_, i64>(0)
        }
    };
    assert_eq!(count("local_file").await, 9);
    assert_eq!(count("local_sensitive_file").await, 1);
    assert_eq!(count("random_id").await, 1);
    assert_eq!(count("random_pet").await, 2);

    // The foreign keys hold: a referenced row cannot go while a row names it.
    let refused = client
        .execute(
            &format!("DELETE FROM \"{schema}\".\"local_file\" WHERE morph_name = 'readme'"),
            &[],
        )
        .await
        .unwrap_err();
    assert!(
        refused.to_string().contains("violates foreign key") || refused.as_db_error().is_some(),
        "{refused}"
    );
    let comment: String = client
        .query_one(
            "SELECT obj_description(($1 || '.local_file')::regclass, 'pg_class')",
            &[&format!("\"{schema}\"")],
        )
        .await
        .unwrap()
        .get(0);
    assert!(comment.contains("local_file of registry.terraform.io/hashicorp/local"));

    // The controller accepts the configuration, with the URL filled in.
    let text = output
        .controller_toml
        .replace("postgres://USER@HOST:5432/DATABASE", &url);
    let config = Config::parse(&text).expect("the controller parses its configuration");
    config.validate().expect("the controller validates it");
    assert_eq!(config.mappings.len(), 4);
    let file = config
        .mappings
        .iter()
        .find(|mapping| mapping.table == "local_file")
        .unwrap();
    assert_eq!(file.schema, schema);
    assert_eq!(file.import_column.as_deref(), Some("tf_import_id"));
    assert!(file.lifecycle.prevent_destroy);

    // The controller discovers the references the migration made: the
    // declared companions, with what they set.
    let database = Arc::new(Database::new(&url, None).unwrap());
    let references = discover(&database, &config.mappings).await.unwrap();
    let found: Vec<String> = references.iter().map(ToString::to_string).collect();
    assert_eq!(
        found,
        [
            format!(
                "{schema}.local_file.content_ref_local_file_content -> {schema}.local_file (content) into content, from the mapping"
            ),
            format!(
                "{schema}.local_file.content_ref_local_file_id -> {schema}.local_file (id) into content, from the mapping"
            ),
            format!(
                "{schema}.local_file.content_ref_random_pet -> {schema}.random_pet (id) into content, from the mapping"
            ),
        ]
    );
    assert_eq!(
        references
            .exported_by(&schema, "local_file")
            .into_iter()
            .collect::<Vec<_>>(),
        ["content", "id"]
    );

    // The controller reads the rows as resources named as the migration
    // named them, with the import id and without the control columns.
    let source = PostgresSource::new(&config, Arc::clone(&database), false);
    let mut resources: Vec<String> = source
        .all_resources()
        .await
        .unwrap()
        .iter()
        .map(ToString::to_string)
        .collect();
    resources.sort();
    assert_eq!(resources.len(), 13);
    assert!(resources.contains(&"local_file.notes-a-note".to_owned()));
    assert!(resources.contains(&"random_pet.notes-b-pet".to_owned()));
    let copy = source
        .row(&"local_file.copy".parse().unwrap())
        .await
        .unwrap()
        .expect("the row is there");
    assert_eq!(
        copy.import_id.as_deref(),
        Some("f4cdef6fd6fe02765e97169f9fadb12f2e2fc9c2")
    );
    assert_eq!(copy.json["content_ref_local_file_content"], "readme");
    assert!(copy.json["content"].is_null());
    let seed = source
        .row(&"random_id.seed".parse().unwrap())
        .await
        .unwrap()
        .unwrap();
    let spec = spec_of_row(&seed).unwrap();
    let attributes: Vec<&String> = spec.attributes().keys().collect();
    assert_eq!(attributes, ["byte_length", "prefix"]);

    client
        .batch_execute(&format!("DROP SCHEMA \"{schema}\" CASCADE"))
        .await
        .unwrap();
}

/// How long to wait for the controller's events.
const PATIENCE: Duration = Duration::from_secs(120);

/// The cache that Morph's examples fill, and the newest version of the
/// random provider in it.
fn cached_random() -> Option<(PathBuf, String)> {
    let cache = Path::new(env!("CARGO_MANIFEST_DIR")).join("../morph/target/providers");
    let versions = cache.join("registry.opentofu.org/hashicorp/random");
    let mut found: Vec<String> = std::fs::read_dir(&versions)
        .ok()?
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.path().is_dir())
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    found.sort();
    Some((cache.canonicalize().ok()?, found.pop()?))
}

/// Waits for the end of a reconciliation of `resource`: whether it
/// succeeded, and whether anything changed.
async fn reconciled(
    events: &mut tokio::sync::broadcast::Receiver<Event>,
    resource: &str,
) -> (bool, bool) {
    tokio::time::timeout(PATIENCE, async {
        loop {
            match events.recv().await {
                Ok(Event::Reconciled {
                    resource: done,
                    ok,
                    changed,
                    ..
                }) if done.to_string() == resource => return (ok, changed),
                Ok(_) => {}
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    panic!("the controller stopped sending events")
                }
            }
        }
    })
    .await
    .expect("the reconciliation ends in time")
}

/// Removes everything the adoption test makes, before and after.
async fn cleanup(client: &tokio_postgres::Client, unique: &str) {
    let _ = client
        .batch_execute(&format!(
            "DROP SCHEMA IF EXISTS \"{unique}\" CASCADE; \
             DROP TABLE IF EXISTS \"{unique}_dirty\"; DROP TABLE IF EXISTS \"{unique}_refs\"; \
             DROP PUBLICATION IF EXISTS \"{unique}_pub\"; \
             DELETE FROM morph_state WHERE name LIKE '{unique}/%'"
        ))
        .await;
    for _ in 0..50 {
        let dropped = client
            .batch_execute(&format!(
                "SELECT pg_drop_replication_slot('{unique}_slot') FROM pg_replication_slots WHERE slot_name = '{unique}_slot'"
            ))
            .await;
        if dropped.is_ok() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[tokio::test]
async fn the_real_controller_takes_charge_of_the_random_id_and_changes_nothing() {
    let Ok(url) = std::env::var("MTM_TEST_DATABASE_URL") else {
        eprintln!("MTM_TEST_DATABASE_URL is not set; nothing to do");
        return;
    };
    let Some((cache, version)) = cached_random() else {
        eprintln!("hashicorp/random is not in Morph's cache; nothing to do");
        return;
    };
    let unique = format!("mtm_adopt_{}", std::process::id());
    let settings = Settings {
        pg_schema: unique.clone(),
        only: vec!["random_id.seed".into()],
        slot: format!("{unique}_slot"),
        publication: format!("{unique}_pub"),
        ..Settings::default()
    };
    let inputs = Inputs::load(
        &fixture("state.json"),
        &fixture("schema.json"),
        Some(&fixture("plan.json")),
    )
    .unwrap();
    let output = migrate(&inputs, &settings).unwrap();
    assert_eq!(output.migration.rows.len(), 1);
    assert_eq!(output.migration.providers.len(), 1);

    let (client, connection) = tokio_postgres::connect(&url, tokio_postgres::NoTls)
        .await
        .expect("the database is reachable");
    tokio::spawn(connection);
    cleanup(&client, &unique).await;
    client
        .batch_execute(&output.schema_sql)
        .await
        .expect("schema.sql runs");
    client
        .batch_execute(&output.rows_sql)
        .await
        .expect("rows.sql runs");

    // What the report says to do by hand for a type whose import takes
    // more than the id: the prefix goes in front.
    let edited = client
        .execute(
            &format!(
                "UPDATE \"{unique}\".\"random_id\" SET tf_import_id = 'demo,' || tf_import_id WHERE morph_name = 'seed'"
            ),
            &[],
        )
        .await
        .unwrap();
    assert_eq!(edited, 1);

    // The configuration, with what the user fills in: the URL, the
    // provider's version and cache, a record prefix and tables of this
    // test's own.
    let text = output
        .controller_toml
        .replace("postgres://USER@HOST:5432/DATABASE", &url)
        .replace(
            "source = \"hashicorp/random\"\n# version = \"\"   # pin the version Terraform ran: terraform version\n",
            &format!(
                "source = \"hashicorp/random\"\nversion = \"{version}\"\ncache = \"{}\"\n",
                cache.display()
            ),
        )
        .replace(
            "[morph]\nstate = { backend = \"postgres\" }\n",
            &format!("[morph]\nstate = {{ backend = \"postgres\" }}\nstate_prefix = \"{unique}/\"\n"),
        )
        + &format!("\n[checkpoint]\ntable = \"{unique}_dirty\"\n\n[references]\ntable = \"{unique}_refs\"\n");
    assert!(text.contains(&format!("version = \"{version}\"")), "{text}");
    let config = Config::parse(&text).expect("the edited configuration parses");

    let controller = Controller::start(config)
        .await
        .expect("the controller starts the cached provider");
    let mut events = controller.events();
    let (ok, changed) = reconciled(&mut events, "random_id.seed").await;
    assert!(ok, "the row's first turn succeeds");
    assert!(!changed, "the row describes the object as it is");
    let record: String = client
        .query_one(
            "SELECT state::text FROM morph_state WHERE name = $1",
            &[&format!("{unique}/random_id.seed")],
        )
        .await
        .expect("the controller recorded the resource")
        .get(0);
    assert!(
        record.contains("demo1a083847"),
        "the provider rebuilt the object from the id: {record}"
    );
    assert!(record.contains("Ggg4Rw"), "{record}");
    controller.stop(Duration::from_secs(30)).await;
    cleanup(&client, &unique).await;
}
