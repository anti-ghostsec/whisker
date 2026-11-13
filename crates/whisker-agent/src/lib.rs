//! AI agents for Whisker: an automated Matrix account answered by a model,
//! with permission-gated moderation tools.
#![recursion_limit = "256"]

pub mod api;
pub mod card;
pub mod config;
pub mod llm;
pub mod memory;
pub mod permissions;
pub mod reply;
pub mod runner;
pub mod tools;
pub mod web;

pub use api::{ApiConfig, ApiFormat, Detected, detect};
pub use config::{AgentConfig, Trigger};
pub use llm::Llm;
pub use permissions::{Permission, Permissions};
pub use runner::{AgentRunner, agents_in, save_agents, spawn, start_all, start_vault};
