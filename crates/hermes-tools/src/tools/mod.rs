//! Tool implementations module
//!
//! Each submodule implements one or more tool handlers that conform
//! to the `ToolHandler` trait from `hermes-core`.

pub mod audio_gen;
pub mod browser;
pub mod clarify;
pub mod code_execution;
pub mod computer_use;
pub mod credential_files;
pub mod cronjob;
pub mod delegation;
pub mod desktop_project;
pub mod desktop_terminal;
pub mod drive_preview;
pub mod env_passthrough;
pub mod feishu_doc;
pub mod file;
pub mod gpt_image;
pub mod homeassistant;
pub mod image_gen;
pub mod manage_catalog;
pub mod manage_connections;
pub mod managed_tool_gateway;
pub mod media_workflow;
pub mod memory;
pub mod messaging;
pub mod mixture_of_agents;
pub mod osv_check;
pub mod process_registry;
pub mod session_search;
pub mod setup_beat;
pub mod skill_commands;
pub mod skill_utils;
pub mod skills;
pub mod terminal;
pub mod todo;
pub mod tool_result_storage;
pub mod transcription;
pub mod tts;
pub mod tts_premium;
pub mod url_safety;
pub mod video_gen;
pub mod vision;
pub mod voice_mode;
pub mod web;
pub mod x_search;
