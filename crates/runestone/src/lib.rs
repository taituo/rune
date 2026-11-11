//! runestone: the versioned-world engine behind rune.
//!
//! World and conversation share one state id (an event in a content-addressed DAG). Every event snapshots
//! the world, is attributed by construction, and can be rewound to or forked from. The agent loop
//! (`agent`) is built on the record: the conversation is a fold over the same events.

pub mod agent;
pub mod checks;
pub mod classify;
pub mod engine;
pub mod hook;
pub mod layer;
pub mod ops;
pub mod rewind;
pub mod search;
pub mod store;
pub mod world;

pub use engine::Engine;
pub use store::{Event, Store};
pub use world::World;
