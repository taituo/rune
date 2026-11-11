//! rune's own agent loop. The conversation is a fold over the record, so rewind and fork apply to the
//! conversation and the world at once, and every request the model saw can be re-rendered byte for byte.

pub mod cancel;
pub mod fold;
pub mod model;
pub mod openai;
pub mod prompt;
pub mod session;
pub mod tools;

pub use session::{render, verify_requests, Config, Permission, Quiet, Session, TurnResult, Ui};
