//! Discord publishing.

pub mod publisher;

use crate::owls::models::{EvEvent, Opportunity};

/// One alert awaiting publication.
///
/// `enriched` records whether `booksInConsensus` and `kellyFraction` were
/// available when this was queued, so the embed can mark an alert built from a
/// cold cache rather than hiding it.
#[derive(Debug, Clone)]
pub struct Alert {
    pub event: EvEvent,
    pub opportunity: Opportunity,
    pub enriched: bool,
}
