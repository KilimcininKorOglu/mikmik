// Unified web-search pipeline: a provider registry walked as an auto-fallback
// chain, plus a structured query layer that parses and enforces Google-style
// directives (`site:`, `filetype:`, `before:`/`after:`, …).

pub mod query;
pub mod types;
