//! Resource addresses, as Terraform writes them.
//!
//! An address names one resource instance: the module instances it is in,
//! the resource type and name, and the instance key if the resource has
//! `count` or `for_each`. `module.notes["a"].local_file.note[0]` is one.
//! The plan's configuration and the state's `depends_on` use the same form
//! without instance keys, `module.notes.local_file.note`, which names a
//! resource rather than an instance; see [`Address::config_path`].

use std::fmt;

use crate::error::{Error, Result};

/// The key of one instance of a resource or module with `count` or
/// `for_each`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Key {
    /// A `count` index.
    Number(i64),
    /// A `for_each` key.
    Text(String),
}

impl fmt::Display for Key {
    /// Writes the key in brackets as Terraform does, `[0]` or `["a"]`, with
    /// `"` and `\` escaped inside the quotes.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Key::Number(number) => write!(f, "[{number}]"),
            Key::Text(text) => {
                write!(f, "[\"")?;
                for character in text.chars() {
                    match character {
                        '"' => write!(f, "\\\"")?,
                        '\\' => write!(f, "\\\\")?,
                        other => write!(f, "{other}")?,
                    }
                }
                write!(f, "\"]")
            }
        }
    }
}

/// One dotted step of an address or a reference: a name with an optional
/// key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Segment {
    /// The name.
    pub name: String,
    /// The key, if the step has one.
    pub key: Option<Key>,
}

/// Splits `text` at its dots into segments, reading keys in brackets.
///
/// This is the lexical form shared by addresses and by the references in a
/// plan's expressions, such as `module.notes["a"].note_id`. A name may hold
/// ASCII letters, digits, `_` and `-`. A dot inside a quoted key is part of
/// the key, and `\"` and `\\` inside one are unescaped.
///
/// # Errors
///
/// Returns [`Error::Address`] for an empty name, an unclosed key or
/// bracket, a key that is neither a number nor quoted text, or any other
/// character.
pub fn segments(text: &str) -> Result<Vec<Segment>> {
    let failed = |reason: &str| Error::Address {
        address: text.to_owned(),
        reason: reason.to_owned(),
    };
    let mut segments = Vec::new();
    let mut characters = text.chars().peekable();
    loop {
        let mut name = String::new();
        while let Some(&character) = characters.peek() {
            if character.is_ascii_alphanumeric() || character == '_' || character == '-' {
                name.push(character);
                characters.next();
            } else {
                break;
            }
        }
        if name.is_empty() {
            return Err(failed("a name is missing"));
        }
        let mut key = None;
        if characters.peek() == Some(&'[') {
            characters.next();
            key = Some(if characters.peek() == Some(&'"') {
                characters.next();
                let mut value = String::new();
                loop {
                    match characters.next() {
                        Some('"') => break,
                        Some('\\') => match characters.next() {
                            Some(escaped) => value.push(escaped),
                            None => return Err(failed("a key is not closed")),
                        },
                        Some(other) => value.push(other),
                        None => return Err(failed("a key is not closed")),
                    }
                }
                Key::Text(value)
            } else {
                let mut digits = String::new();
                while let Some(&character) = characters.peek() {
                    if character.is_ascii_digit() || (character == '-' && digits.is_empty()) {
                        digits.push(character);
                        characters.next();
                    } else {
                        break;
                    }
                }
                let number = digits
                    .parse()
                    .map_err(|_| failed("a key is neither a number nor quoted text"))?;
                Key::Number(number)
            });
            if characters.next() != Some(']') {
                return Err(failed("a key is not closed by a bracket"));
            }
        }
        segments.push(Segment { name, key });
        match characters.next() {
            None => return Ok(segments),
            Some('.') => {}
            Some(other) => return Err(failed(&format!("unexpected character {other:?}"))),
        }
    }
}

/// Whether an address names a resource that is managed or one that is only
/// read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// A `resource` block: Terraform creates, updates and deletes it.
    Managed,
    /// A `data` block: Terraform reads it and manages nothing.
    Data,
}

/// One module instance on the way to a resource.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModuleStep {
    /// The module call's name.
    pub name: String,
    /// The instance key, if the call has `count` or `for_each` and the
    /// address names an instance.
    pub key: Option<Key>,
}

/// The address of one resource or resource instance.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Address {
    /// The module instances, outermost first. Empty at the root.
    pub modules: Vec<ModuleStep>,
    /// Managed or data.
    pub mode: Mode,
    /// The resource type, such as `aws_instance`.
    pub resource_type: String,
    /// The resource's name in its module.
    pub name: String,
    /// The instance key, if the resource has `count` or `for_each` and the
    /// address names an instance.
    pub key: Option<Key>,
}

impl Address {
    /// Parses an address of the form
    /// `[module.<name>[<key>].]* [data.]<type>.<name>[<key>]`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Address`], with a reason, if `text` does not have
    /// that form.
    pub fn parse(text: &str) -> Result<Self> {
        let failed = |reason: &str| Error::Address {
            address: text.to_owned(),
            reason: reason.to_owned(),
        };
        let mut rest = segments(text)?.into_iter().peekable();
        let mut modules = Vec::new();
        while rest.peek().is_some_and(|segment| segment.name == "module") {
            let module = rest.next().expect("peeked");
            if module.key.is_some() {
                return Err(failed("the word module takes no key"));
            }
            let instance = rest.next().ok_or_else(|| failed("a module has no name"))?;
            modules.push(ModuleStep {
                name: instance.name,
                key: instance.key,
            });
        }
        let mut mode = Mode::Managed;
        if rest.peek().is_some_and(|segment| segment.name == "data") {
            let data = rest.next().expect("peeked");
            if data.key.is_some() {
                return Err(failed("the word data takes no key"));
            }
            mode = Mode::Data;
        }
        let resource_type = rest
            .next()
            .ok_or_else(|| failed("the resource type is missing"))?;
        if resource_type.key.is_some() {
            return Err(failed("a resource type takes no key"));
        }
        let name = rest
            .next()
            .ok_or_else(|| failed("the resource name is missing"))?;
        if rest.next().is_some() {
            return Err(failed("there is more after the resource name"));
        }
        Ok(Self {
            modules,
            mode,
            resource_type: resource_type.name,
            name: name.name,
            key: name.key,
        })
    }

    /// The address of the resource in the configuration: this address
    /// without any instance key, such as `module.notes.local_file.note`.
    /// This is how `depends_on` and the plan name resources.
    pub fn config_path(&self) -> String {
        let mut text = String::new();
        for module in &self.modules {
            text.push_str("module.");
            text.push_str(&module.name);
            text.push('.');
        }
        if self.mode == Mode::Data {
            text.push_str("data.");
        }
        text.push_str(&self.resource_type);
        text.push('.');
        text.push_str(&self.name);
        text
    }

    /// The module instance the resource is in, with keys, such as
    /// `module.notes["a"]`, or an empty string at the root.
    pub fn module_path(&self) -> String {
        let mut text = String::new();
        for module in &self.modules {
            if !text.is_empty() {
                text.push('.');
            }
            text.push_str("module.");
            text.push_str(&module.name);
            if let Some(key) = &module.key {
                text.push_str(&key.to_string());
            }
        }
        text
    }

    /// The names of the module calls on the way to the resource, without
    /// instance keys, which is how the plan's configuration nests them.
    pub fn module_names(&self) -> Vec<&str> {
        self.modules
            .iter()
            .map(|module| module.name.as_str())
            .collect()
    }
}

impl fmt::Display for Address {
    /// Writes the address as Terraform does, so that parsing and printing
    /// round-trip.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for module in &self.modules {
            write!(f, "module.{}", module.name)?;
            if let Some(key) = &module.key {
                write!(f, "{key}")?;
            }
            write!(f, ".")?;
        }
        if self.mode == Mode::Data {
            write!(f, "data.")?;
        }
        write!(f, "{}.{}", self.resource_type, self.name)?;
        if let Some(key) = &self.key {
            write!(f, "{key}")?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses_are_read_and_written_back_the_same() {
        for text in [
            "aws_vpc.main",
            "aws_subnet.private[0]",
            "aws_subnet.private[\"us-east-1a\"]",
            "module.vpc.aws_subnet.private[1]",
            "module.notes[\"a\"].local_file.note",
            "module.outer[2].module.inner[\"x.y\"].data.aws_ami.latest",
            "aws_thing.odd[\"quo\\\"te\"]",
            "aws_thing.odd[\"back\\\\slash\"]",
            "module.a.module.b[\"k\"].x.y",
        ] {
            let address = Address::parse(text).unwrap();
            assert_eq!(address.to_string(), text);
        }
    }

    #[test]
    fn the_parts_of_an_address_are_told_apart() {
        let address = Address::parse("module.notes[\"a\"].local_file.note[3]").unwrap();
        assert_eq!(address.modules.len(), 1);
        assert_eq!(address.modules[0].name, "notes");
        assert_eq!(address.modules[0].key, Some(Key::Text("a".into())));
        assert_eq!(address.mode, Mode::Managed);
        assert_eq!(address.resource_type, "local_file");
        assert_eq!(address.name, "note");
        assert_eq!(address.key, Some(Key::Number(3)));
        assert_eq!(address.config_path(), "module.notes.local_file.note");
        assert_eq!(address.module_path(), "module.notes[\"a\"]");
        assert_eq!(address.module_names(), vec!["notes"]);

        let nested = Address::parse("module.a.module.b[\"k\"].x.y").unwrap();
        assert_eq!(nested.module_path(), "module.a.module.b[\"k\"]");
        assert_eq!(nested.config_path(), "module.a.module.b.x.y");
        let data = Address::parse("data.aws_ami.latest").unwrap();
        assert_eq!(data.mode, Mode::Data);
        assert_eq!(data.config_path(), "data.aws_ami.latest");
        assert_eq!(data.module_path(), "");
    }

    #[test]
    fn what_is_not_an_address_is_refused_with_a_reason() {
        for (text, reason) in [
            ("", "a name is missing"),
            ("aws_vpc", "the resource name is missing"),
            (
                "aws_vpc.main.extra",
                "there is more after the resource name",
            ),
            ("module.aws_vpc", "the resource type is missing"),
            ("module[0].x.y", "the word module takes no key"),
            ("data[1].x.y", "the word data takes no key"),
            ("aws_vpc[0].main", "a resource type takes no key"),
            ("aws_vpc.main[", "a key is neither a number nor quoted text"),
            ("aws_vpc.main[\"a", "a key is not closed"),
            ("aws_vpc.main[\"a\\", "a key is not closed"),
            ("aws_vpc.main[1", "a key is not closed by a bracket"),
            ("aws_vpc.main]", "unexpected character ']'"),
            ("aws_vpc..main", "a name is missing"),
        ] {
            let error = Address::parse(text).unwrap_err();
            assert!(
                error.to_string().contains(reason),
                "{text:?}: {error} should mention {reason:?}"
            );
        }
    }

    #[test]
    fn references_split_into_segments_with_their_keys() {
        let parts = segments("module.notes[\"a\"].note_id").unwrap();
        assert_eq!(parts.len(), 3);
        assert_eq!(parts[1].name, "notes");
        assert_eq!(parts[1].key, Some(Key::Text("a".into())));
        assert_eq!(parts[2].name, "note_id");
        assert_eq!(parts[2].key, None);
        let negative = segments("x.y[-1]").unwrap();
        assert_eq!(negative[1].key, Some(Key::Number(-1)));
    }
}
