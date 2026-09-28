//! Resolve an intent to an asset, chain, and executable venue.

use engine_types::{Chain, Venue};

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Resolution {
    Supported { chain: Chain, venue: Venue },
    Unsupported { query: String },
}