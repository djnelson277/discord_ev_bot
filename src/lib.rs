//! Discord +EV alert bot — library crate.
//!
//! Phase 1 is poller-only: no inbound HTTP, so nothing here needs a public
//! address. See docs/superpowers/specs/ for the design.

pub mod config;

pub mod owls;

pub mod gating;
