#![recursion_limit = "256"]
//! Whisker's engine: accounts, vaults, moderation and server extensions.
//! Shared by the GUI and the headless agent daemon.

pub mod accounts;
pub mod anoncat;
pub mod events;
pub mod moderation;
pub mod profile;
pub mod rooms;
pub mod stores;
pub mod timeline;
pub mod vault;
pub mod verify;

pub use accounts::{AccountManager, AccountSummary, LoginRequest, VaultId};
pub use events::ClientEvent;
pub use matrix_sdk;
pub use moderation::ModAction;
pub use profile::{AccountId, AccountKind};
