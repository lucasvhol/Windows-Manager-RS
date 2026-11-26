//! Safety layer. Every system modification must be journaled here so it can be undone.

mod journal;
pub mod restore_point;

pub use journal::{ChangeJournal, Entry, UndoAction};

/// For tests: true when an undo failed only because this machine keeps no Recycle Bin
/// (CI runners delete outright), so a "restore from the Recycle Bin" check can't run.
#[doc(hidden)]
pub fn no_recycle_bin_here(err: &anyhow::Error) -> bool {
    std::env::var_os("CI").is_some() && format!("{err:#}").contains("no longer in the Recycle Bin")
}
