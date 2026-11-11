//! runestone: the versioned-world engine behind rune.
//!
//! World and conversation share one state id (an event in a content-addressed DAG). Every event snapshots
//! the world, is attributed by construction, and can be rewound to or forked from. The agent loop is a
//! swappable part; this crate never talks to a model.

pub mod classify;
pub mod engine;
pub mod hook;
pub mod ops;
pub mod rewind;
pub mod store;
pub mod world;

pub use engine::Engine;
pub use store::{Event, Store};
pub use world::World;
