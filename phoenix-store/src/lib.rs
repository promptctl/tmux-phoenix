//! `phoenix-store` — atomic, versioned, generational persistence for a
//! `phoenix-core` `Snapshot` (DESIGN.md §7).

mod binary;
mod checksum;
mod codec;
mod error;
mod header;
mod json;
mod store;

pub use error::StoreError;
pub use json::to_json;
pub use store::{SaveOutcome, Store};
