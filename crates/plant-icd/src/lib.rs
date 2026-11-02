//! Interface control definitions shared by `plantd` and its clients.
//!
//! `no_std` + `alloc`, so the same types work in a desktop front end, on the Pi,
//! or on an MCU with an allocator. `docs/ICD.md` is the language-neutral version
//! of everything in here.
//!
//! - [`node`]: what a soil node serves (consumed by `plantd`).
//! - [`api`]: what `plantd` serves over HTTP (consumed by front ends).
//! - [`record`]: the 8-byte on-disk / `format=bin` sample record.
//! - [`text`]: the line grammars for events and reports, plus UTC time formatting.
#![no_std]

extern crate alloc;

pub mod api;
pub mod node;
pub mod record;
pub mod text;

/// Bumped on any breaking change to [`api`], [`record`] or [`text`].
pub const API_VERSION: u32 = 1;

/// Every `plantd` HTTP route lives under this prefix.
pub const API_PREFIX: &str = "/api/v1";
