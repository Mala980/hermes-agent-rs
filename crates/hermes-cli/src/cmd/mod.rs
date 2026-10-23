//! Subcommand handlers for the `hermes` binary.
//!
//! Only modules that compile cleanly are declared here. Broken modules
//! (auth, batch, cloud, common, config, cron, eval, fallback, gateway,
//! interactive, misc, moa, run, serve, skin, tools, tui_cmd) are left on
//! disk but not declared; they will be fixed in a follow-up.

// ── Phase 5 new modules (all compile clean) ──
pub mod advanced;
pub mod checkpoints;
pub mod debug_cmd;
pub mod dump_cmd;
pub mod insights;
pub mod kanban;
pub mod logs;
pub mod monitoring;
pub mod profile;
pub mod prompt_size;
pub mod usage_cmd;
pub mod version;
pub mod worktree;

// ── Previously verified good modules ──
pub mod browser;
pub mod estop;
pub mod slack;
pub mod voice;
pub mod webhook;

// ── Fixed broken modules (2026-10-23) ──
pub mod auth;
pub mod batch;
pub mod cloud;
pub mod common;
pub mod config;
pub mod cron;
pub mod eval;
pub mod fallback;
pub mod gateway;
pub mod interactive;
pub mod misc;
pub mod moa;
pub mod run;
pub mod serve;
pub mod skin;
pub mod tools;
pub mod tui_cmd;
