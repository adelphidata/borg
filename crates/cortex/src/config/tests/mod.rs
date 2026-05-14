//! Test submodules for `crate::config`.
//!
//! Split from the former monolithic `config/tests.rs` into topic-focused
//! files purely for organizational purposes — no behavior changes.

#[allow(unused_imports)]
use super::*;

mod basics;
mod channels;
mod consolidation_settings;
mod credentials;
mod gateway;
mod goals_settings;
mod guards;
mod media;
mod memory_skills;
mod save_roundtrip;
mod secrets_llm;
