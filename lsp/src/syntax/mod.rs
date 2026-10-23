// Copyright 2026 Oxide Computer Company

//! P4-16 syntax tree and parser.

pub mod ast;
pub mod parser;

pub use parser::{parse, Parse};

#[cfg(test)]
mod tests;
