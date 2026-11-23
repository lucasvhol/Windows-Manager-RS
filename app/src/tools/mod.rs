//! Tool registry. Each phase adds its tool crates here.

mod home;
mod journal;
mod settings;

use wm_core::Tool;

/// All tools in sidebar order (sorted by category, then registration order).
pub fn all() -> Vec<Box<dyn Tool>> {
    let mut tools: Vec<Box<dyn Tool>> = vec![
        Box::new(home::Home),
        Box::new(journal::Journal::default()),
        Box::new(settings::Settings::default()),
    ];
    tools.extend(wm_tools_info::tools());
    tools.extend(wm_tools_optimizer::tools());
    tools.sort_by_key(|t| t.category());
    tools
}
