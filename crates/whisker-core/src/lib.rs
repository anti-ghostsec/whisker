#![recursion_limit = "256"]
//! Whisker's engine: accounts, vaults, moderation and server extensions.
//! Shared by the GUI and the headless agent daemon.

pub mod accounts;
pub mod addresses;
pub mod anoncat;
pub mod devices;
pub mod events;
pub mod files;
pub mod link_preview;
pub mod media_fetch;
pub mod moves;
pub mod moderation;
pub mod notices;
pub mod polls;
pub mod profile;
pub mod rooms;
pub mod signup;
pub mod safe_web;
pub mod stores;
pub mod timeline;
pub mod tls;
pub mod twins;
pub mod stickers;
pub mod update;
pub mod upload;
pub mod vault;
pub mod verify;
pub mod voice_mask;

pub use accounts::{AccountManager, AccountSummary, LoginRequest, SignupAnswer, SignupAsk, SignupStep, VaultId};
pub use events::ClientEvent;
pub use matrix_sdk;
pub use moderation::ModAction;
pub use profile::{AccountId, AccountKind};

/// Keeps secrets in memory out of crash files: no core dump is written
/// (it would hold vault keys and access tokens), and on Linux other
/// programs of the same user can't attach to Whisker or read its memory.
/// Called first thing at startup.
pub fn harden_process() {
    #[cfg(unix)]
    {
        let none = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
        // SAFETY: plain system calls on this process, with valid arguments.
        unsafe {
            if libc::setrlimit(libc::RLIMIT_CORE, &none) != 0 {
                tracing::warn!("couldn't turn off core dumps");
            }
            #[cfg(any(target_os = "linux", target_os = "android"))]
            if libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0) != 0 {
                tracing::warn!("couldn't mark the process as not dumpable");
            }
        }
    }
}
