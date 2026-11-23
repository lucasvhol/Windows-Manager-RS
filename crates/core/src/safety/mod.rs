//! Safety layer. Every system modification must be journaled here so it can be undone.

mod journal;
pub mod restore_point;

pub use journal::{ChangeJournal, Entry, UndoAction};
