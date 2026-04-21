//! Tests for the `db` module, split into focused submodules by topic.
//!
//! This module owns the shared test helpers (`test_db`, `simple_task`) that
//! every submodule reuses. Individual test groups live next to the area of
//! the `db` module they exercise (tasks, sessions, plugins, memory, etc.).
#![allow(unused_imports)]

use super::*;
use crate::multi_agent::SubAgentStatus;
use rusqlite::params;

pub(super) fn test_db() -> Database {
    Database::test_db()
}

pub(super) fn simple_task<'a>(
    id: &'a str,
    name: &'a str,
    prompt: &'a str,
    schedule_type: &'a str,
    schedule_expr: &'a str,
    next_run: Option<i64>,
) -> NewTask<'a> {
    NewTask {
        id,
        name,
        prompt,
        schedule_type,
        schedule_expr,
        timezone: "local",
        next_run,
        max_retries: None,
        timeout_ms: None,
        delivery_channel: None,
        delivery_target: None,
        allowed_tools: None,
        task_type: "prompt",
    }
}

mod activity;
mod delivery;
mod memory;
mod meta;
mod multi_agent;
mod pairing;
mod plugins;
mod scripts;
mod sessions;
mod tasks;
mod usage;
mod vitals;
