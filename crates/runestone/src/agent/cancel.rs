//! Cooperative cancellation for a running turn (Ctrl-C).
//!
//! The token lives on the Engine; a session installs it for its thread while a turn runs, and the Bash tool
//! and the providers poll it. Per thread, so parallel sessions (search attempts, tests) never cancel each
//! other. A provider blocked waiting for the first byte only notices at the next stream line; a second
//! Ctrl-C in the CLI exits the process.

use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

thread_local! {
    static TOKEN: RefCell<Option<Arc<AtomicBool>>> = const { RefCell::new(None) };
}

/// The error a cancelled model call returns.
#[derive(Debug)]
pub struct Interrupted;

impl std::fmt::Display for Interrupted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "interrupted by the user")
    }
}

impl std::error::Error for Interrupted {}

pub struct Guard(Option<Arc<AtomicBool>>);

impl Drop for Guard {
    fn drop(&mut self) {
        let prev = self.0.take();
        TOKEN.with(|t| *t.borrow_mut() = prev);
    }
}

pub fn install(token: Arc<AtomicBool>) -> Guard {
    Guard(TOKEN.with(|t| t.borrow_mut().replace(token)))
}

pub fn is_set() -> bool {
    TOKEN.with(|t| t.borrow().as_ref().is_some_and(|a| a.load(Ordering::SeqCst)))
}

pub fn is_interrupted(e: &anyhow::Error) -> bool {
    e.downcast_ref::<Interrupted>().is_some()
}
