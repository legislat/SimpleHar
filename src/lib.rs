//! SimpleHar SE — one application managing local inference engines with a simple,
//! reliable, adjustable swap mechanism for local LLM models (DESIGN.md G1).
//!
//! Crate layout follows DESIGN.md §18. Modules land with their milestone; nothing is
//! pre-built for futures that v0 defers (§0.4): tray, process supervision internals,
//! the llama adapter, and the control page arrive with their own turns.

pub mod api;
pub mod budget;
pub mod catalog;
pub mod config;
pub mod engine;
pub mod listeners;
pub mod model;
pub mod observability;
pub mod ownership;
pub mod process;
pub mod scheduler;
pub mod testutil;
pub mod tray;
pub mod webui;
pub mod witness;
