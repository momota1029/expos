//! File-operation authority.
//!
//! E1 deliberately contains no filesystem access. `model` owns the durable job
//! lifecycle; a later executor receives only immutable requests and tagged commands.

pub mod application;
pub mod batch_rename;
pub mod executor;
pub mod file_clipboard;
pub mod journal;
pub mod model;
pub mod privileged;
pub mod rename;
pub mod runtime;
pub mod transport;
pub mod trash;
pub mod virtual_transfer;
