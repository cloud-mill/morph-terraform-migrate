//! From Terraform's JSON to a [`Migration`].
//!
//! [`analyse`] takes the state, the schemas and, if given, the plan, and
//! decides four things for every managed resource instance: which of its
//! attributes are columns, what its row is named, which of its values refer
//! to other rows, and in what order the rows can be loaded. Each rule is
//! documented on the function that applies it, and every decision the user
//! may want to revisit goes into the [`Report`].
//!
//! Nothing here reads HCL. With a plan, Terraform's own reading of the
//! configuration says which attributes were set and what they refer to.
//! Without one, the schema and the state's `depends_on` stand in, more
//! cautiously: an optional attribute the provider may also compute is left
//! out, since a default cannot be told from a choice.
//!
//! # Pipeline
//!
//! The analysis runs in fixed phases over a list of `Candidate`s, each
//! phase filling in a field that later phases read:
//!
//! 1. `gather` reads the state, applies the address filter and attaches
//!    each instance's schema and its block in the plan.
//! 2. `select` decides which attributes are columns.
//! 3. `name_rows` names every row.
//! 4. `find_references` decides, for each selected value, whether it is a
//!    reference to another row. It needs the names from step 3.
//! 5. `order_rows` sorts the rows topologically and turns references that
//!    form a cycle back into constants.
//! 6. `decide_columns` builds the tables, moving cells into companion
//!    columns where an attribute is a reference in some rows only. It must
//!    run after step 5, which looks cells up by attribute name.
//! 7. `providers` collects the providers and the settings that can be
//!    carried into the configuration.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use serde_json::Value;

use crate::error::{Error, Result};
use crate::model::{
    CONTROL_COLUMNS, Cell, Column, ColumnReference, ColumnType, ConstantNote, ConstantReason,
    EXPORTS, Evidence, LeftOut, LeftOutReason, Migration, Provider, ReferenceNote, Renamed, Report,
    Row, Table,
};
use crate::names;
use crate::terraform::address::Address;
use crate::terraform::plan::{Configuration, Located, Target, expression, references_within};
use crate::terraform::schema::{Schema, tail};
use crate::terraform::state::Instance;
use crate::{EngineKind, Inputs, Settings};

/// One managed instance on its way to becoming a row.
///
/// The last three fields are empty until the phase named on each has run;
/// see the module documentation for the order.
struct Candidate<'a> {
    /// The instance as the state holds it.
    instance: &'a Instance,
    /// The parsed form of the instance's address.
    address: Address,
    /// The address without instance keys, as `depends_on` and the plan name
    /// resources. See [`Address::config_path`].
    config_path: String,
    /// The module instance the resource is in. See [`Address::module_path`].
    module_path: String,
    /// The schema of the instance's resource type.
    schema: &'a Schema,
    /// The resource block in the plan's configuration, if a plan was given
    /// and the block was found in it.
    located: Option<Located<'a>>,
    /// The attributes that are columns, with their type and value. Filled
    /// by [`select`].
    selected: BTreeMap<String, (ColumnType, Value)>,
    /// One cell per selected attribute. Filled by [`find_references`] and
    /// keyed by attribute name until [`decide_columns`] moves the cells of
    /// companion columns under the companion's name.
    cells: BTreeMap<String, Cell>,
    /// The row's name. Filled by [`name_rows`].
    name: String,
}

impl Candidate<'_> {
    /// The module instance path cut to its first `depth` modules: where to
    /// look for the instance of a resource that a reference resolved at
    /// that depth means.
    fn module_path_to(&self, depth: usize) -> String {
        let mut cut = self.address.clone();
        cut.modules.truncate(depth);
        cut.module_path()
    }

    /// The value of `attribute` as text, if it is a string.
    fn text(&self, attribute: &str) -> Option<&str> {
        self.instance.values.get(attribute).and_then(Value::as_str)
    }
}

/// Turns the inputs into a migration by running the phases described in
/// the module documentation.
///
/// # Errors
///
/// * [`Error::Address`] if the state holds an address that cannot be
///   parsed.
/// * [`Error::Schema`] if a managed resource's type has no schema.
/// * [`Error::Nothing`] if no managed resource is left after the address
///   filter.
pub fn analyse(inputs: &Inputs, settings: &Settings) -> Result<Migration> {
    let configuration = inputs
        .plan
        .as_ref()
        .and_then(|plan| plan.configuration.as_ref());
    let mut report = Report::default();
    let mut candidates = gather(inputs, settings, configuration, &mut report)?;

    let mut left_out: BTreeMap<(String, String, LeftOutReason), usize> = BTreeMap::new();
    let mut unknown: BTreeSet<String> = BTreeSet::new();
    for candidate in &mut candidates {
        candidate.selected = select(candidate, settings, &mut left_out, &mut unknown);
    }
    report.left_out = left_out
        .into_iter()
        .map(|((resource_type, attribute, reason), rows)| LeftOut {
            resource_type,
            attribute,
            reason,
            rows,
        })
        .collect();
    report.unknown_attributes = unknown.into_iter().collect();

    name_rows(&mut candidates, &mut report);
    find_references(&mut candidates, configuration, settings, &mut report);
    let order = order_rows(&mut candidates, &mut report);
    let tables = decide_columns(&mut candidates, settings, &mut report);

    let mut rows = Vec::with_capacity(candidates.len());
    for index in order {
        let candidate = &candidates[index];
        // An `id` is normally a string, but some providers use a number.
        let import_id = match candidate.instance.values.get("id") {
            Some(Value::String(text)) => Some(text.clone()),
            Some(Value::Number(number)) => Some(number.to_string()),
            _ => {
                report
                    .without_import_id
                    .push(candidate.instance.address.clone());
                None
            }
        };
        let module = settings
            .stacks_from_modules
            .then(|| names::stack_name(&candidate.address));
        for (attribute, cell) in &candidate.cells {
            if let Cell::Reference {
                target_address,
                attribute: taken,
                evidence,
                ..
            } = cell
            {
                report.references.push(ReferenceNote {
                    address: candidate.instance.address.clone(),
                    column: attribute.clone(),
                    target: target_address.clone(),
                    attribute: taken.clone(),
                    evidence: *evidence,
                });
            }
        }
        rows.push(Row {
            address: candidate.instance.address.clone(),
            table: candidate.instance.resource_type.clone(),
            name: candidate.name.clone(),
            module,
            import_id,
            cells: candidate.cells.clone(),
        });
    }
    if settings.stacks_from_modules {
        let mut counts: BTreeMap<String, usize> = BTreeMap::new();
        for row in &rows {
            *counts
                .entry(row.module.clone().unwrap_or_default())
                .or_default() += 1;
        }
        report.stacks = counts.into_iter().collect();
    }
    let providers = providers(&candidates, inputs, configuration, &mut report);

    Ok(Migration {
        terraform_version: inputs.state.terraform_version.clone(),
        tables,
        rows,
        providers,
        report,
    })
}

/// Reads every managed instance that the address filter admits, with its
/// schema and its block in the plan's configuration.
///
/// Data sources and filtered-out resources are recorded in the report
/// rather than returned.
fn gather<'a>(
    inputs: &'a Inputs,
    settings: &Settings,
    configuration: Option<&'a Configuration>,
    report: &mut Report,
) -> Result<Vec<Candidate<'a>>> {
    let mut candidates = Vec::new();
    for instance in inputs.state.instances() {
        let address = Address::parse(&instance.address)?;
        if !instance.is_managed() {
            report.data_sources.push(instance.address.clone());
            continue;
        }
        if !admitted(&settings.only, &instance.address) {
            report.filtered_out.push(instance.address.clone());
            continue;
        }
        let schema = inputs
            .schemas
            .resource(&instance.provider_name, &instance.resource_type)
            .ok_or_else(|| Error::Schema {
                provider: instance.provider_name.clone(),
                resource_type: instance.resource_type.clone(),
            })?;
        let config_path = address.config_path();
        let located = configuration.and_then(|configuration| configuration.locate(&config_path));
        candidates.push(Candidate {
            instance,
            module_path: address.module_path(),
            config_path,
            address,
            schema,
            located,
            selected: BTreeMap::new(),
            cells: BTreeMap::new(),
            name: String::new(),
        });
    }
    if candidates.is_empty() {
        let reason = if report.filtered_out.is_empty() {
            "the state has no managed resources".to_owned()
        } else {
            format!(
                "the address filter admits none of the {} managed resources",
                report.filtered_out.len()
            )
        };
        return Err(Error::Nothing { reason });
    }
    Ok(candidates)
}

/// Whether `address` passes the `only` filter: it equals a prefix, or
/// continues one with `.` (a resource inside a module) or `[` (an instance
/// of a resource or module). An empty filter admits everything.
///
/// Matching whole segments is what keeps `module.vpc` from admitting
/// `module.vpcs`.
fn admitted(only: &[String], address: &str) -> bool {
    only.is_empty()
        || only.iter().any(|prefix| {
            address == prefix
                || address.starts_with(&format!("{prefix}."))
                || address.starts_with(&format!("{prefix}["))
        })
}

/// Decides which attributes of one instance are columns.
///
/// An attribute is a column if it has a non-null value and the user set it:
///
/// * With the plan, the configuration says exactly which attributes were
///   set, and only those are columns.
/// * Without it, a required attribute was set; an optional attribute that
///   is not computed was set if it has a value; and an optional attribute
///   that is also computed cannot be told from a provider default, so it is
///   left out as [`LeftOutReason::OptionalComputed`].
/// * A computed-only attribute is never a column.
/// * A nested block is a `jsonb` column if it is not empty and, with the
///   plan, the configuration sets it.
/// * A sensitive attribute is left out unless asked for, and an attribute
///   named like a control column is always left out.
///
/// Attributes the state holds but the schema does not know are collected
/// in `unknown`, since they are a sign of a schema from another provider
/// version.
fn select(
    candidate: &Candidate<'_>,
    settings: &Settings,
    left_out: &mut BTreeMap<(String, String, LeftOutReason), usize>,
    unknown: &mut BTreeSet<String>,
) -> BTreeMap<String, (ColumnType, Value)> {
    let block = &candidate.schema.block;
    let instance = candidate.instance;
    let expressions = candidate
        .located
        .as_ref()
        .map(|located| &located.resource.expressions);
    let mut selected = BTreeMap::new();
    let mut leave = |name: &str, reason: LeftOutReason| {
        *left_out
            .entry((instance.resource_type.clone(), name.to_owned(), reason))
            .or_default() += 1;
    };
    for (name, attribute) in &block.attributes {
        let Some(value) = instance.values.get(name) else {
            continue;
        };
        if value.is_null() || attribute.is_computed_only() {
            continue;
        }
        if CONTROL_COLUMNS.contains(&name.as_str()) {
            leave(name, LeftOutReason::ReservedName);
            continue;
        }
        if (attribute.sensitive || instance.is_sensitive(name)) && !settings.include_sensitive {
            leave(name, LeftOutReason::Sensitive);
            continue;
        }
        match expressions {
            Some(expressions) => {
                if !expressions.contains_key(name) {
                    continue;
                }
            }
            None => {
                if attribute.computed {
                    leave(name, LeftOutReason::OptionalComputed);
                    continue;
                }
            }
        }
        selected.insert(name.clone(), (attribute.column_type(), value.clone()));
    }
    for name in block.block_types.keys() {
        let Some(value) = instance.values.get(name) else {
            continue;
        };
        let empty = match value {
            Value::Null => true,
            Value::Array(items) => items.is_empty(),
            Value::Object(fields) => fields.is_empty(),
            _ => false,
        };
        if empty {
            continue;
        }
        if CONTROL_COLUMNS.contains(&name.as_str()) {
            leave(name, LeftOutReason::ReservedName);
            continue;
        }
        if instance.is_sensitive(name) && !settings.include_sensitive {
            leave(name, LeftOutReason::Sensitive);
            continue;
        }
        if let Some(expressions) = expressions
            && !expressions.contains_key(name)
        {
            continue;
        }
        selected.insert(name.clone(), (ColumnType::Jsonb, value.clone()));
    }
    for name in instance.values.keys() {
        if !block.attributes.contains_key(name) && !block.block_types.contains_key(name) {
            unknown.insert(format!("{}.{name}", instance.resource_type));
        }
    }
    selected
}

/// Names every row, in place.
///
/// A row takes its [`names::short_name`] unless another row of the same
/// table has the same, in which case both take their [`names::long_name`];
/// if even those collide, a counter is appended (`-2`, `-3`, ...). Names
/// are assigned in state order, so the outcome is deterministic. A row
/// that did not get its short name is listed in [`Report::renamed`].
fn name_rows(candidates: &mut [Candidate<'_>], report: &mut Report) {
    let mut by_table: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    for (index, candidate) in candidates.iter().enumerate() {
        by_table
            .entry(candidate.instance.resource_type.clone())
            .or_default()
            .push(index);
    }
    for indexes in by_table.values() {
        // Only ever looked up, never iterated, so a HashMap's order does
        // not affect the outcome.
        let mut shorts: HashMap<String, usize> = HashMap::new();
        for &index in indexes {
            *shorts
                .entry(names::short_name(&candidates[index].address))
                .or_default() += 1;
        }
        let mut taken: BTreeSet<String> = BTreeSet::new();
        for &index in indexes {
            let short = names::short_name(&candidates[index].address);
            let mut name = if shorts[&short] == 1 {
                short.clone()
            } else {
                names::long_name(&candidates[index].address)
            };
            let base = name.clone();
            let mut number = 1;
            while !taken.insert(name.clone()) {
                number += 1;
                name = format!("{base}-{number}");
            }
            debug_assert!(names::is_valid(&name));
            if name != short {
                report.renamed.push(Renamed {
                    address: candidates[index].instance.address.clone(),
                    name: name.clone(),
                    short,
                });
            }
            candidates[index].name = name;
        }
    }
}

/// Decides, for every selected value of every row, whether it is a
/// reference to another row's attribute, filling [`Candidate::cells`].
///
/// Evidence is sought from strongest to weakest, and the first kind that
/// holds decides:
///
/// 1. *Configuration.* The attribute's expression in the plan refers to a
///    resource, and the value equals an attribute of one of that resource's
///    instances. An expression that is a constant, or that refers to
///    resources but equals none of their attributes, makes the value a
///    constant; the latter is reported as composed, or as outside the
///    migration when none of its targets is in the migration.
/// 2. *Dependency.* Only when there is no plan, the block was not found in
///    it, or the expression went through something the plan cannot follow
///    (a `local`, `each` or `self`): the instance's `depends_on` names the
///    resources it may refer to, and the value equals an attribute of one
///    of their instances.
/// 3. *Value.* Only with [`Settings::guess_references`]: the value equals
///    the `id` of any instance at all.
///
/// Where the attribute referred to is not named, `id` is tried across all
/// the candidate instances before any other attribute. A `jsonb` value is
/// never a reference; one whose expression refers to resources is reported
/// as a collection.
fn find_references(
    candidates: &mut [Candidate<'_>],
    configuration: Option<&Configuration>,
    settings: &Settings,
    report: &mut Report,
) {
    let mut by_config_path: HashMap<String, Vec<usize>> = HashMap::new();
    for (index, candidate) in candidates.iter().enumerate() {
        by_config_path
            .entry(candidate.config_path.clone())
            .or_default()
            .push(index);
    }
    for index in 0..candidates.len() {
        let mut cells = BTreeMap::new();
        // Cloned so that every candidate can be read while this one's
        // cells are being decided.
        let selected = candidates[index].selected.clone();
        for (attribute, (kind, value)) in &selected {
            let cell = reference_of(
                candidates,
                index,
                attribute,
                *kind,
                value,
                configuration,
                &by_config_path,
                settings,
                report,
            );
            cells.insert(attribute.clone(), cell);
        }
        candidates[index].cells = cells;
    }
}

/// The cell for one selected value, by the rules of [`find_references`].
///
/// `index` is the referring row, which never matches itself.
// The arguments are the analysis state threaded through; a struct would
// only rename them.
#[allow(clippy::too_many_arguments)]
fn reference_of(
    candidates: &[Candidate<'_>],
    index: usize,
    attribute: &str,
    kind: ColumnType,
    value: &Value,
    configuration: Option<&Configuration>,
    by_config_path: &HashMap<String, Vec<usize>>,
    settings: &Settings,
    report: &mut Report,
) -> Cell {
    let candidate = &candidates[index];
    let constant = Cell::Constant(value.clone());
    let located = candidate.located.as_ref();
    let expression_value = located.and_then(|located| located.resource.expressions.get(attribute));
    if kind == ColumnType::Jsonb {
        // A collection or block cannot be a reference: the controller
        // propagates one attribute through one column. Say so if it refers
        // to something.
        if let Some(expression_value) = expression_value {
            let inside = references_within(expression_value);
            if let (Some(configuration), Some(located), false) =
                (configuration, located, inside.is_empty())
            {
                let resolution = configuration.resolve(located, &inside);
                if !resolution.targets.is_empty() {
                    report.constants.push(ConstantNote {
                        address: candidate.instance.address.clone(),
                        attribute: attribute.to_owned(),
                        reason: ConstantReason::Collection {
                            targets: resolution
                                .targets
                                .iter()
                                .map(|target| target.config_path.clone())
                                .collect(),
                        },
                    });
                }
            }
        }
        return constant;
    }
    let Some(text) = value.as_str() else {
        return constant;
    };
    let reference = |target_index: usize, attribute: String, evidence: Evidence| Cell::Reference {
        target: candidates[target_index].name.clone(),
        target_address: candidates[target_index].instance.address.clone(),
        attribute,
        evidence,
    };

    // Step 1: what the configuration says.
    let mut opaque = false;
    if let (Some(configuration), Some(located), Some(expression_value)) =
        (configuration, located, expression_value)
    {
        let expression = expression(expression_value);
        if expression.constant.is_some() {
            return constant;
        }
        let resolution = configuration.resolve(located, &expression.references);
        let mut outside = Vec::new();
        for target in &resolution.targets {
            let Some(indexes) = by_config_path.get(&target.config_path) else {
                outside.push(target.config_path.clone());
                continue;
            };
            let ordered = ranked(candidates, indexes, candidate, target);
            if let Some((target_index, found)) = best_match(
                candidates,
                &ordered,
                index,
                target.attribute.as_deref(),
                text,
            ) {
                return reference(target_index, found, Evidence::Configuration);
            }
        }
        if !resolution.opaque {
            // Every reference was followed and none matched: the value is
            // derived from the targets, or they are all outside the
            // migration. An opaque resolution falls through instead, since
            // `depends_on` may still know the resource.
            if !resolution.targets.is_empty() {
                let reason = if outside.len() == resolution.targets.len() {
                    ConstantReason::Outside {
                        target: outside.remove(0),
                    }
                } else {
                    ConstantReason::Composed {
                        targets: resolution
                            .targets
                            .iter()
                            .map(|target| target.config_path.clone())
                            .collect(),
                    }
                };
                report.constants.push(ConstantNote {
                    address: candidate.instance.address.clone(),
                    attribute: attribute.to_owned(),
                    reason,
                });
            }
            return constant;
        }
        opaque = true;
    }

    // Step 2: `depends_on`. A dependency names a resource without instance
    // keys, so among its instances the one in the referring row's own
    // module instance at that depth is tried first.
    if configuration.is_none() || located.is_none() || opaque {
        for dependency in &candidate.instance.depends_on {
            let Some(indexes) = by_config_path.get(dependency) else {
                continue;
            };
            let depth = dependency.matches("module.").count();
            let wanted = candidate.module_path_to(depth);
            let mut ordered: Vec<usize> = indexes.clone();
            ordered.sort_by_key(|&other| candidates[other].module_path != wanted);
            if let Some((target_index, found)) = best_match(candidates, &ordered, index, None, text)
            {
                return reference(target_index, found, Evidence::Dependency);
            }
        }
    }

    // Step 3: a bare match on any instance's id, only when asked for.
    if settings.guess_references {
        let everyone: Vec<usize> = (0..candidates.len()).collect();
        if let Some((target_index, found)) =
            best_match(candidates, &everyone, index, Some(EXPORTS), text)
        {
            return reference(target_index, found, Evidence::Value);
        }
    }
    constant
}

/// The instances of a target's resource, most likely first.
///
/// Instances in the module instance the reference points at come first:
/// the one the reference named on its way through a module output, placed
/// under the referring row's own module path, or, for a reference that
/// stays within a module, the referring row's module instance at the
/// target's depth. Among those, instances with the key the reference names
/// come first. The order only breaks ties between instances with equal
/// values; a value that matches an instance elsewhere still matches.
fn ranked(
    candidates: &[Candidate<'_>],
    indexes: &[usize],
    from: &Candidate<'_>,
    target: &Target,
) -> Vec<usize> {
    let wanted_module = match &target.module_instance {
        Some(instance) => {
            let mut path = from.module_path_to(target.depth.saturating_sub(1));
            if !path.is_empty() {
                path.push('.');
            }
            path.push_str(instance);
            path
        }
        None => from.module_path_to(target.depth),
    };
    let mut ordered: Vec<usize> = indexes.to_vec();
    ordered.sort_by_key(|&index| {
        let other = &candidates[index];
        let module_differs = other.module_path != wanted_module
            && !other.module_path.starts_with(&format!("{wanted_module}."));
        let key_differs = match &target.key {
            Some(key) => other.address.key.as_ref() != Some(key),
            None => false,
        };
        (module_differs, key_differs)
    });
    ordered
}

/// The first of `ordered`, other than `skip`, with an attribute equal to
/// `text`, together with the attribute's name.
///
/// With `attribute` given, only that attribute is compared. Without it,
/// the `id` of every candidate is tried first, and only then any other
/// string attribute, so that the identifier of a later instance beats a
/// lesser attribute of an earlier one.
fn best_match(
    candidates: &[Candidate<'_>],
    ordered: &[usize],
    skip: usize,
    attribute: Option<&str>,
    text: &str,
) -> Option<(usize, String)> {
    let others = ordered.iter().copied().filter(|&index| index != skip);
    if let Some(attribute) = attribute {
        return others
            .filter(|&index| candidates[index].text(attribute) == Some(text))
            .map(|index| (index, attribute.to_owned()))
            .next();
    }
    if let Some(index) = others
        .clone()
        .find(|&index| candidates[index].text(EXPORTS) == Some(text))
    {
        return Some((index, EXPORTS.to_owned()));
    }
    others
        .filter_map(|index| {
            candidates[index]
                .instance
                .values
                .iter()
                .filter(|(name, _)| name.as_str() != EXPORTS)
                .find(|(_, value)| value.as_str() == Some(text))
                .map(|(name, _)| (index, name.clone()))
        })
        .next()
}

/// Builds the tables: decides for every column whether it is a reference
/// column, and makes companion columns where an attribute is a reference
/// in some rows and a plain value in others.
///
/// A column is a *reference column* when every cell it has, across all the
/// rows of its table, refers to one other table and takes the same
/// attribute. It is then `text`, holds row names, and stands for the
/// attribute of its own name. Otherwise the column keeps its type and its
/// plain values, with `NULL` where a row referred to something, and each
/// distinct target (a table and an attribute) gets a *companion column*:
/// `text`, named `<attribute>_ref`, or `<attribute>_ref_<table>` when
/// several tables are targeted, or `<attribute>_ref_<table>_<attribute>`
/// when one table is targeted through several attributes, with `_x`
/// appended until the name is free. The companion holds the row names and
/// `sets` the attribute in the controller.
///
/// The cells of the rows that referred to something are moved from the
/// attribute's name to the companion's name, so after this phase
/// [`Candidate::cells`] is keyed by column rather than by attribute.
///
/// With the Pulumi engine, each table takes its type token from
/// [`Settings::pulumi_types`], and a type without one is reported.
fn decide_columns(
    candidates: &mut [Candidate<'_>],
    settings: &Settings,
    report: &mut Report,
) -> Vec<Table> {
    let mut by_table: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    for (index, candidate) in candidates.iter().enumerate() {
        by_table
            .entry(candidate.instance.resource_type.clone())
            .or_default()
            .push(index);
    }
    let table_of: HashMap<String, String> = candidates
        .iter()
        .map(|candidate| {
            (
                candidate.instance.address.clone(),
                candidate.instance.resource_type.clone(),
            )
        })
        .collect();
    let mut tables = Vec::new();
    for (resource_type, indexes) in &by_table {
        let mut kinds: BTreeMap<String, ColumnType> = BTreeMap::new();
        for &index in indexes {
            for (name, (kind, _)) in &candidates[index].selected {
                kinds.entry(name.clone()).or_insert(*kind);
            }
        }
        let mut columns = Vec::new();
        for (name, kind) in &kinds {
            let mut groups: BTreeMap<(String, String), Vec<usize>> = BTreeMap::new();
            let mut constants = 0;
            for &index in indexes {
                match candidates[index].cells.get(name) {
                    Some(Cell::Reference {
                        target_address,
                        attribute,
                        ..
                    }) => groups
                        .entry((table_of[target_address].clone(), attribute.clone()))
                        .or_default()
                        .push(index),
                    Some(Cell::Constant(_)) => constants += 1,
                    None => {}
                }
            }
            if groups.is_empty() {
                columns.push(Column {
                    name: name.clone(),
                    kind: *kind,
                    reference: None,
                });
                continue;
            }
            if constants == 0 && groups.len() == 1 {
                let (table, attribute) = groups.into_keys().next().expect("one group");
                columns.push(Column {
                    name: name.clone(),
                    kind: ColumnType::Text,
                    reference: Some(ColumnReference {
                        table,
                        attribute,
                        sets: None,
                    }),
                });
                continue;
            }
            columns.push(Column {
                name: name.clone(),
                kind: *kind,
                reference: None,
            });
            let mut per_table: BTreeMap<&str, usize> = BTreeMap::new();
            for (table, _) in groups.keys() {
                *per_table.entry(table.as_str()).or_default() += 1;
            }
            let single = groups.len() == 1;
            for ((table, attribute), members) in &groups {
                let mut companion = if single {
                    format!("{name}_ref")
                } else if per_table[table.as_str()] == 1 {
                    format!("{name}_ref_{table}")
                } else {
                    format!("{name}_ref_{table}_{attribute}")
                };
                // The resource may have an attribute of that very name, as
                // `peer_ref` beside `peer`, or an earlier companion may have
                // taken it.
                while kinds.contains_key(&companion)
                    || columns
                        .iter()
                        .any(|column: &Column| column.name == companion)
                {
                    companion.push_str("_x");
                }
                for &index in members {
                    let cell = candidates[index]
                        .cells
                        .remove(name)
                        .expect("a reference cell");
                    candidates[index].cells.insert(companion.clone(), cell);
                }
                columns.push(Column {
                    name: companion,
                    kind: ColumnType::Text,
                    reference: Some(ColumnReference {
                        table: table.clone(),
                        attribute: attribute.clone(),
                        sets: Some(name.clone()),
                    }),
                });
            }
        }
        let pulumi_type = match settings.engine {
            EngineKind::Pulumi => {
                let token = settings.pulumi_types.get(resource_type).cloned();
                if token.is_none() {
                    report.unmapped_types.push(resource_type.clone());
                }
                token
            }
            EngineKind::Morph => None,
        };
        tables.push(Table {
            resource_type: resource_type.clone(),
            provider: candidates[indexes[0]].instance.provider_name.clone(),
            columns,
            pulumi_type,
        });
    }
    tables
}

/// Orders the rows so that every row comes after the rows it refers to,
/// keeping state order among rows that are otherwise unconstrained.
///
/// A topological sort that always takes the lowest-indexed ready row, so
/// the result is deterministic. References found through the configuration
/// or `depends_on` cannot form a cycle, since Terraform's own graph has
/// none, but guessed ones can. Every row the sort leaves unplaced, whether
/// in a cycle or merely downstream of one, has *all* of its references
/// turned back into constants, each reported as [`ConstantReason::Cycle`],
/// and is appended in state order.
///
/// Must run before [`decide_columns`]: it looks a cell's original value up
/// in [`Candidate::selected`] by the cell's name, which only matches while
/// the cells are still keyed by attribute.
fn order_rows(candidates: &mut [Candidate<'_>], report: &mut Report) -> Vec<usize> {
    let by_address: HashMap<String, usize> = candidates
        .iter()
        .enumerate()
        .map(|(index, candidate)| (candidate.instance.address.clone(), index))
        .collect();
    let mut dependents: Vec<Vec<usize>> = vec![Vec::new(); candidates.len()];
    let mut remaining: Vec<usize> = vec![0; candidates.len()];
    for (index, candidate) in candidates.iter().enumerate() {
        for cell in candidate.cells.values() {
            if let Cell::Reference { target_address, .. } = cell {
                let target = by_address[target_address];
                dependents[target].push(index);
                remaining[index] += 1;
            }
        }
    }
    let mut ready: BTreeSet<usize> = (0..candidates.len())
        .filter(|&index| remaining[index] == 0)
        .collect();
    let mut order = Vec::with_capacity(candidates.len());
    while let Some(index) = ready.pop_first() {
        order.push(index);
        for &dependent in &dependents[index] {
            remaining[dependent] -= 1;
            if remaining[dependent] == 0 {
                ready.insert(dependent);
            }
        }
    }
    if order.len() < candidates.len() {
        let placed: BTreeSet<usize> = order.iter().copied().collect();
        for (index, candidate) in candidates.iter_mut().enumerate() {
            if placed.contains(&index) {
                continue;
            }
            let names: Vec<String> = candidate
                .cells
                .iter()
                .filter(|(_, cell)| matches!(cell, Cell::Reference { .. }))
                .map(|(name, _)| name.clone())
                .collect();
            for name in names {
                let value = candidate.selected[&name].1.clone();
                candidate.cells.insert(name.clone(), Cell::Constant(value));
                report.constants.push(ConstantNote {
                    address: candidate.instance.address.clone(),
                    attribute: name,
                    reason: ConstantReason::Cycle,
                });
            }
            order.push(index);
        }
    }
    order
}

/// The providers the rows belong to, each with the settings of its
/// provider block that can be carried into `controller.toml`.
///
/// Only the default block declared at the root, without an alias, is read,
/// and only when a plan was given. A setting is carried when it is a
/// constant, or a single root variable whose value the plan holds.
/// Settings the schema marks sensitive or whose name [`looks_secret`],
/// nested blocks, and expressions that cannot be evaluated are not
/// carried, and [`Report::provider_notes`] says so for each.
fn providers(
    candidates: &[Candidate<'_>],
    inputs: &Inputs,
    configuration: Option<&Configuration>,
    report: &mut Report,
) -> Vec<Provider> {
    let names: BTreeSet<&str> = candidates
        .iter()
        .map(|candidate| candidate.instance.provider_name.as_str())
        .collect();
    let mut providers = Vec::new();
    for full_name in names {
        let source = tail(full_name);
        let name = source.rsplit('/').next().unwrap_or(&source).to_owned();
        let mut config = BTreeMap::new();
        if let Some(configuration) = configuration {
            let schema = inputs
                .schemas
                .provider(full_name)
                .and_then(|provider| provider.provider.as_ref());
            for block in configuration.provider_config.values() {
                if block.full_name.as_deref() != Some(full_name)
                    || block.alias.is_some()
                    || block.module_address.is_some()
                {
                    continue;
                }
                for (key, expression_value) in &block.expressions {
                    let note = |why: &str| format!("{name}: the setting {key} {why}");
                    let sensitive = schema.is_some_and(|schema| {
                        schema
                            .block
                            .attributes
                            .get(key)
                            .is_some_and(|attribute| attribute.sensitive)
                    });
                    if sensitive || looks_secret(key) {
                        report
                            .provider_notes
                            .push(note("is sensitive and was not carried over"));
                        continue;
                    }
                    let expression = expression(expression_value);
                    if let Some(constant) = expression.constant {
                        config.insert(key.clone(), constant.clone());
                    } else if let [variable] = expression.references.as_slice()
                        && let Some(variable) = variable.strip_prefix("var.")
                        && let Some(value) = inputs
                            .plan
                            .as_ref()
                            .and_then(|plan| plan.variables.get(variable))
                    {
                        config.insert(key.clone(), value.value.clone());
                    } else if expression_value.is_array() || !expression_value.is_object() {
                        // An attribute's expression is an object; a nested
                        // block of list or set mode is an array of them. A
                        // single-mode block is an object too, and is
                        // reported below as an expression instead.
                        report
                            .provider_notes
                            .push(note("is a nested block and was not carried over"));
                    } else {
                        report.provider_notes.push(note(
                            "is an expression that could not be evaluated and was not carried over",
                        ));
                    }
                }
            }
        }
        providers.push(Provider {
            full_name: full_name.to_owned(),
            source,
            name,
            config,
        });
    }
    report.provider_notes.sort();
    providers
}

/// Whether a provider setting's name suggests it holds a secret. A
/// safeguard beside the schema's `sensitive` flag, which not every provider
/// sets on its credentials.
fn looks_secret(key: &str) -> bool {
    [
        "secret",
        "password",
        "token",
        "credential",
        "private_key",
        "access_key",
    ]
    .iter()
    .any(|word| key.contains(word))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_address_filter_admits_instances_and_resources_under_a_prefix() {
        let only = vec!["module.vpc".to_owned(), "aws_vpc.main".to_owned()];
        assert!(admitted(&only, "module.vpc.aws_subnet.a"));
        assert!(admitted(&only, "module.vpc[\"x\"].aws_subnet.a"));
        assert!(admitted(&only, "aws_vpc.main"));
        assert!(admitted(&only, "aws_vpc.main[0]"));
        assert!(!admitted(&only, "module.vpcs.aws_subnet.a"));
        assert!(!admitted(&only, "aws_vpc.main2"));
        assert!(admitted(&[], "anything.at.all"));
    }

    #[test]
    fn secret_looking_settings_are_told_by_name() {
        assert!(looks_secret("secret_key"));
        assert!(looks_secret("password"));
        assert!(looks_secret("access_token"));
        assert!(!looks_secret("region"));
    }
}
