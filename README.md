# morph-terraform-migrate

Moves resources that Terraform manages into tables and rows for the Morph PostgreSQL controller, without reading a line
of HCL and without touching the resources.

Terraform is asked for what it knows, as JSON: its state, the schemas of its providers, and a plan. From those three
files this tool writes four:

| File              | What it is                                                                           |
|-------------------|--------------------------------------------------------------------------------------|
| `schema.sql`      | One table per resource type, one typed column per attribute the configuration sets.  |
| `rows.sql`        | One row per resource instance, in an order that respects the references.             |
| `controller.toml` | A configuration for the controller: mappings, references, providers or Pulumi types. |
| `report.md`       | Every decision the user should look at, and the cutover commands.                    |

Nothing is run against a database or a cloud. The user reads the report, loads the SQL, and starts the controller, which
takes charge of each object by the identifier the row carries rather than making it again.

## Why from the state, not the code

Terraform has already evaluated every module, variable, local, `count` and
`for_each` by the time the state exists. Reading HCL again would mean a second Terraform, and a worse one. The state
holds the flat truth about each resource: its actual values, what it depends on, which values are sensitive, and the
identifier the provider knows it by. The plan adds Terraform's own reading of the configuration: which attributes were
set, and which of them refer to other resources. The schema says what type each attribute has and whether the user or
the provider sets it.

This tool is not a Terraform clone, an HCL parser, a configuration language or a provider ecosystem. It consumes
documented JSON and writes SQL and TOML.

## How it works

```mermaid
flowchart LR
    S[terraform show -json<br/>state.json] --> A
    P[terraform show -json tfplan<br/>plan.json, optional] --> A
    H[terraform providers schema -json<br/>schema.json] --> A
    M[pulumi package get-mapping<br/>optional, Pulumi only] --> A
    A[analyse: columns, names,<br/>references, order] --> R[render]
    R --> T[schema.sql]
    R --> W[rows.sql]
    R --> C[controller.toml]
    R --> D[report.md]
    T --> PG[(PostgreSQL)]
    W --> PG
    C --> K[morph-postgres-controller]
    PG --> K
    K -->|import by tf_import_id,<br/>then reconcile| Cloud[(existing objects)]
```

What each Terraform concept becomes:

| Terraform                                        | In the tables and the controller                                               |
|--------------------------------------------------|--------------------------------------------------------------------------------|
| resource type `aws_subnet`                       | table `aws_subnet`, mapping with `resource_type = "aws_subnet"`                |
| instance `module.vpc["a"].aws_subnet.private[0]` | a row named `vpc-a-private-0`; the address kept in `tf_address`                |
| module instance                                  | optionally a stack, with `--stacks-from-modules`                               |
| attribute the configuration sets                 | a column: text, numeric, boolean, or jsonb for collections and blocks          |
| attribute only the provider computes             | not a column; `id` is what the table exports                                   |
| a reference to another resource                  | a column that names the other row, with a foreign key                          |
| a reference in some rows, a value in others      | the attribute column, plus a companion `<attribute>_ref` column that `sets` it |
| the provider's identifier (`id`)                 | `tf_import_id`, the mapping's `import_column`                                  |
| a sensitive value                                | left out of the rows and listed in the report, unless `--include-sensitive`    |
| a data source                                    | not migrated; its values are already constants in the state                    |

## Running it

In the Terraform working directory:

```sh
terraform show -json > state.json
terraform providers schema -json > schema.json
terraform plan -out tfplan && terraform show -json tfplan > plan.json
```

Then:

```sh
morph-terraform-migrate --state state.json --schema schema.json --plan plan.json --out migration
```

The plan is optional but worth having: with it, exactly the attributes the configuration sets become columns, and
references are followed through module inputs and outputs. Without it, the schema decides, cautiously: an optional
attribute that the provider may also compute is left out and reported, because a default cannot be told from a choice,
and references come from the state's
`depends_on` and value matching.

Options:

| Option                    | Meaning                                                                                                                  |
|---------------------------|--------------------------------------------------------------------------------------------------------------------------|
| `--engine morph\|pulumi`  | Which engine the controller will use. `morph` by default.                                                                |
| `--pulumi-mapping FILE`   | A file from `pulumi package get-mapping terraform <provider>`, naming the Pulumi type of each resource type. Repeatable. |
| `--project NAME`          | The Pulumi project. `terraform-migration` by default.                                                                    |
| `--only ADDRESS`          | Migrate only what is at or under this address: `module.vpc`, `aws_vpc.main`. Repeatable.                                 |
| `--pg-schema NAME`        | The PostgreSQL schema the tables go in. `public` by default.                                                             |
| `--stacks-from-modules`   | Make each module instance a stack, reconciled as one unit.                                                               |
| `--include-sensitive`     | Make columns of sensitive attributes too; their values are then in `rows.sql`.                                           |
| `--unprotected`           | Leave out `prevent_destroy`, which every mapping carries by default.                                                     |
| `--guess-references`      | Take a value equal to the id of any resource as a reference to it, with no other evidence.                               |
| `--slot`, `--publication` | The names the configuration uses. `morph_terraform` by default.                                                          |

The files are replaced on every run. Both SQL files can be run more than once:
the schema adds what is missing and drops nothing, and the rows are upserts keyed by the Terraform address. A large
estate can therefore be migrated module by module, with `--only`.

## The rules, in short

**Columns.** With the plan, an attribute is a column if the configuration sets it. Without it: required attributes are,
optional ones that are not computed are if they have a value, optional and computed ones are left out and reported,
computed-only ones never are. Nested blocks are columns when set, as jsonb. Sensitive attributes are left out unless
asked for. An attribute named like a control column is left out and reported.

**Names.** A row is named by the parts of its address that the table does not already say: module names and keys, then
the resource name and key, joined with `-`. Where two rows of a table would share a name, both take a longer form with
the resource type in it, and a number if even that is shared. The full address stays in `tf_address`.

**References.** A string value is a reference to another row when the configuration says the attribute refers to that
resource and the value equals one of the instance's attributes, or, failing that, when the state's
`depends_on` names the resource and the value equals one of its attributes. Only with `--guess-references` does a bare
match on another resource's id count. A column whose every value is such a reference to one table becomes a reference
column with a foreign key. A column that holds references in some rows and values in others keeps the values and gets a
companion column per target, `<attribute>_ref` or `<attribute>_ref_<table>`, with a foreign key and a declared reference
that `sets` the attribute. A value made from a reference and more, such as a template string, stays a constant and is
reported, as does a reference inside a collection or a block.

**Order.** Rows come after the rows they refer to. Terraform's graph has no circles, so only guessed references can make
one, and those are turned back into constants and reported.

**Providers.** Each provider in use gets a `[[providers]]` block for Morph, or its types in `[engine.types]` for Pulumi.
Settings of the provider block that are plain constants, or root variables, are carried over; sensitive ones, and any
whose name says secret, are not. The database URL is left for the user to fill in. Nothing secret is written into any
file but `rows.sql`, and that only with `--include-sensitive`.

## A migration, step by step

```mermaid
flowchart TD
    E[Export: state, schema, plan] --> G[Generate: morph-terraform-migrate]
    G --> V[Review report.md]
    V --> L[Load schema.sql, then rows.sql]
    L --> F[Fill in controller.toml: URL, versions, Pulumi backend]
    F --> O[Start the controller on one table]
    O --> N{First turn of every row<br/>changed=false?}
    N -->|no| V
    N -->|yes| M[Start it on the rest]
    M --> X[terraform state rm, from the report]
```

1. **Export.** Run the three Terraform commands above.
2. **Generate.** Run this tool. It never touches Terraform, the database or the cloud.
3. **Review.** Read `report.md`: what was left out, what stayed a constant, which references were found and on what
   evidence, which rows were renamed, which have no import id.
4. **Load.** Run `schema.sql`, then `rows.sql`, with `psql`. Make sure the publication the controller uses covers the
   new tables, or let the controller make one with `create_publication = true`, as the generated configuration says.
5. **Fill in.** Set `postgres.url`, pin the provider versions Terraform ran, and for Pulumi say where the stacks live.
6. **Verify.** Start the controller on one table, or run it with `--only` from the start. Each mapping names
   `tf_import_id` as its `import_column`, so the engine takes charge of the existing object before its first
   reconciliation:
   Morph through the provider's import, Pulumi through the `import` resource option. The first turn of every row should
   report `changed=false`. One that does not shows where the row and the object differ; a row whose identifier names
   nothing fails rather than makes a copy. Every mapping carries
   `prevent_destroy`, so nothing adopted can be deleted or replaced until that is turned off on purpose.
7. **Cut over.** Take the migrated resources out of Terraform's state with the
   `terraform state rm` lines the report lists, so that two managers never share one object. Until then, rollback is
   stopping the controller and dropping its records: the objects were never changed.

## What stays out, and why

- **Lists of references.** `subnet_ids = [aws_subnet.a.id, aws_subnet.b.id]`
  is a jsonb constant. The controller propagates one attribute through one column; a list has no column to propagate
  through. The report names such values.
- **Composed values.** `"seed ${random_id.seed.hex}"` stays the string it is.
- **Lifecycle meta-arguments.** `ignore_changes`, `prevent_destroy` and
  `create_before_destroy` are not in Terraform's JSON output. Every mapping is protected by default; edit
  `[mappings.lifecycle]` by hand for the rest.
- **Import identifiers.** The `id` attribute is what most providers import by, but some types want a composite. Edit
  `tf_import_id` in `rows.sql` for those; the controller fails the row, and makes nothing, if the identifier names
  nothing.
- **Provider configuration** that is an expression, a nested block or a secret. The report says which settings were not
  carried over.
- **Data sources.** They manage nothing. Values that came from them are constants now.

## Testing

```sh
cargo test                                 # unit, migration and command-line suites, no database
MTM_TEST_DATABASE_URL=postgres://postgres:morph@127.0.0.1:54331/postgres cargo test --test postgres
```

The fixtures under `tests/fixtures` are the real output of Terraform 1.16 for a project of the `local` and `random`
providers, with files whose content comes from other resources, a module called twice with `for_each`, a `count`, a
sensitive file, a data source and a variable. The `migrate` suite checks every decision against them, with and without
the plan. The `rules` suite reaches every rule a small project does not, on states and plans written by hand in the
shapes Terraform writes: nested blocks, reserved and unknown names, references by instance key and through nested
modules, targets outside the migration, provider settings, names that collide three deep, a circle of guessed
references, and values PostgreSQL cannot hold. The `postgres` suite loads the SQL into a database twice, then asks the
controller's own crate to parse and validate the configuration, to discover the references, and to read the rows as it
would when reconciling them; its second test goes to the end, handing the fixture's `random_id` row to the real
controller with the real
`hashicorp/random` provider from Morph's cache, which takes charge of the object, changes nothing, and records the hex
Terraform's state held. That test is skipped unless the provider is under `../morph/target/providers`. The suites cover
99.6 percent of the lines; what is left is one closing brace. The controller's own tests cover the rest: taking charge
of an object by its import id with Morph, with Pulumi and through a real provider, and a reference that sets another
attribute.

The test database is the same as the controller's:

```sh
docker run -d --name morph-pg -p 127.0.0.1:54331:5432 -e POSTGRES_PASSWORD=morph postgres:16-alpine -c wal_level=logical
```

## Layout

```text
src/
  main.rs            the program
  lib.rs             Settings, Inputs, Output, migrate()
  error.rs           Error and Result
  analyse.rs         from Terraform's JSON to a Migration: columns, names, references, order
  model.rs           Migration, Table, Column, Row, Cell, Report
  names.rs           row names from addresses
  pulumi.rs          Pulumi's mapping files
  terraform/         readers for the state, the plan, the schemas, and addresses
  render/            schema.sql, rows.sql, controller.toml, report.md
tests/
  fixtures/          state.json, plan.json, schema.json, pulumi-*.json
  migrate.rs         the migration of the fixtures, every decision checked
  cli.rs             the program end to end
  rules.rs           every rule of the analysis, on states and plans written by hand
  postgres.rs        the files loaded into PostgreSQL and read by the controller
```
