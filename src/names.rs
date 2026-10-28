//! Row names, made from Terraform addresses.
//!
//! The controller names a resource `<kind>.<name>`, where the kind is the
//! table's resource type and the name is the row's
//! [`NAME_COLUMN`](crate::model::NAME_COLUMN), which may hold ASCII
//! letters, digits, `_` and `-`. A Terraform address has dots, brackets and
//! quotes, and repeats the resource type, so a row takes a shorter name made
//! from the parts of its address that the table does not already say: the
//! module calls and the resource name, with their instance keys.
//!
//! [`short_name`] is tried first. Where two rows of a table would share
//! one, [`crate::analyse`] falls back to [`long_name`], and then to a
//! numbered suffix.

use crate::terraform::address::{Address, Key};

/// The short name of a row: the module names and keys, then the resource
/// name and key, joined with `-`.
///
/// `module.notes["a"].local_file.note[0]` becomes `notes-a-note-0`, and
/// `aws_vpc.main` becomes `main`.
pub fn short_name(address: &Address) -> String {
    let mut parts = Vec::new();
    for module in &address.modules {
        parts.push(sanitize(&module.name));
        if let Some(key) = &module.key {
            parts.push(key_part(key));
        }
    }
    parts.push(sanitize(&address.name));
    if let Some(key) = &address.key {
        parts.push(key_part(key));
    }
    parts.join("-")
}

/// The long name of a row: as [`short_name`], but with the resource type
/// before the resource name.
///
/// Used when two addresses of one table share a short name, as
/// `module.a.local_file.b` and `local_file.a-b` do: the first becomes
/// `a-local_file-b`.
pub fn long_name(address: &Address) -> String {
    let mut parts = Vec::new();
    for module in &address.modules {
        parts.push(sanitize(&module.name));
        if let Some(key) = &module.key {
            parts.push(key_part(key));
        }
    }
    parts.push(sanitize(&address.resource_type));
    parts.push(sanitize(&address.name));
    if let Some(key) = &address.key {
        parts.push(key_part(key));
    }
    parts.join("-")
}

/// The name of the stack a module instance makes: its module names and
/// keys joined with `-`, or [`ROOT_STACK`](crate::model::ROOT_STACK) for
/// the root module.
pub fn stack_name(address: &Address) -> String {
    if address.modules.is_empty() {
        return crate::model::ROOT_STACK.to_owned();
    }
    let mut parts = Vec::new();
    for module in &address.modules {
        parts.push(sanitize(&module.name));
        if let Some(key) = &module.key {
            parts.push(key_part(key));
        }
    }
    parts.join("-")
}

/// An instance key as one part of a name.
///
/// `-` separates the parts, so a negative index is spelled `neg1` rather
/// than `-1`, which would read as an empty part.
fn key_part(key: &Key) -> String {
    match key {
        Key::Number(number) => number.to_string().replace('-', "neg"),
        Key::Text(text) => sanitize(text),
    }
}

/// `text` with every run of characters other than ASCII letters, digits
/// and `_` collapsed into a single `-`, and no `-` at either end.
///
/// `-` itself counts as a separator, so `a--b` becomes `a-b`. Text with
/// nothing to keep becomes `x`, so the result is always a valid name.
pub fn sanitize(text: &str) -> String {
    let mut out = String::new();
    let mut pending = false;
    for character in text.chars() {
        if character.is_ascii_alphanumeric() || character == '_' {
            if pending && !out.is_empty() {
                out.push('-');
            }
            pending = false;
            out.push(character);
        } else {
            pending = true;
        }
    }
    if out.is_empty() { "x".to_owned() } else { out }
}

/// Whether `name` is one the controller accepts: non-empty and made only of
/// ASCII letters, digits, `_` and `-`.
pub fn is_valid(name: &str) -> bool {
    !name.is_empty()
        && name.chars().all(|character| {
            character.is_ascii_alphanumeric() || character == '_' || character == '-'
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn address(text: &str) -> Address {
        Address::parse(text).unwrap()
    }

    #[test]
    fn names_leave_out_what_the_table_says_and_keep_the_rest() {
        assert_eq!(short_name(&address("aws_vpc.main")), "main");
        assert_eq!(short_name(&address("aws_subnet.private[0]")), "private-0");
        assert_eq!(
            short_name(&address("module.notes[\"a\"].local_file.note")),
            "notes-a-note"
        );
        assert_eq!(
            short_name(&address("module.outer[1].module.inner.x.y[\"us-east-1a\"]")),
            "outer-1-inner-y-us-east-1a"
        );
        assert_eq!(
            long_name(&address("module.notes[\"a\"].local_file.note")),
            "notes-a-local_file-note"
        );
        assert_eq!(long_name(&address("aws_vpc.main[-1]")), "aws_vpc-main-neg1");
        assert_eq!(stack_name(&address("aws_vpc.main")), "root");
        assert_eq!(
            stack_name(&address("module.notes[\"a\"].module.deep.x.y")),
            "notes-a-deep"
        );
    }

    #[test]
    fn sanitizing_keeps_names_valid() {
        assert_eq!(sanitize("a.b/c d"), "a-b-c-d");
        assert_eq!(sanitize("--odd--"), "odd");
        assert_eq!(sanitize(""), "x");
        assert_eq!(sanitize("...."), "x");
        assert_eq!(sanitize("fine_name-1"), "fine_name-1");
        for name in ["main", "notes-a-note", "x_1"] {
            assert!(is_valid(name));
        }
        for name in ["", "a.b", "a b", "a\"b"] {
            assert!(!is_valid(name));
        }
        assert!(is_valid(&sanitize("module.notes[\"a\"]")));
    }
}
