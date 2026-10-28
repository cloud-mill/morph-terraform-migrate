//! The renderers that write a [`Migration`](crate::model::Migration) as
//! files.
//!
//! Each takes the migration and the [`Settings`](crate::Settings) and
//! returns one text, deciding nothing further: [`sql`] the tables and the
//! rows, [`config`] the controller's configuration, and [`report`] what the
//! user should look at. [`crate::migrate`] calls all three.

pub mod config;
pub mod report;
pub mod sql;
