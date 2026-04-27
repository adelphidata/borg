//! Test submodules for `crate::config`.
//!
//! Split from the former monolithic `config/tests.rs` into topic-focused
//! files purely for organizational purposes — no behavior changes.

#[allow(unused_imports)]
use super::*;

mod basics;
mod channels;
mod compaction_workflow;
mod credentials;
mod gateway;
mod guards;
mod media;
mod memory_skills;
mod save_roundtrip;
mod secrets_llm;
