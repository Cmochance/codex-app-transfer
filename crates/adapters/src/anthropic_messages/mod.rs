//! Anthropic Messages adapter building blocks.
//!
//! P3 only lands request-side lowering. Adapter/registry wiring is tracked by
//! P5 so this module can be tested without exposing a half-complete provider
//! entry point.

pub mod request;
