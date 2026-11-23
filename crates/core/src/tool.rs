use crate::{config::Config, i18n::I18n, safety::ChangeJournal};

/// Sidebar grouping. Order of variants is the order shown in the UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Category {
    Home,
    OneClick,
    Information,
    Optimizer,
    Cleaner,
    Customization,
    Security,
    Network,
    Misc,
}

impl Category {
    pub const ALL: [Category; 9] = [
        Category::Home,
        Category::OneClick,
        Category::Information,
        Category::Optimizer,
        Category::Cleaner,
        Category::Customization,
        Category::Security,
        Category::Network,
        Category::Misc,
    ];

    /// Fluent message id for the category label.
    pub fn i18n_key(self) -> &'static str {
        match self {
            Category::Home => "category-home",
            Category::OneClick => "category-oneclick",
            Category::Information => "category-information",
            Category::Optimizer => "category-optimizer",
            Category::Cleaner => "category-cleaner",
            Category::Customization => "category-customization",
            Category::Security => "category-security",
            Category::Network => "category-network",
            Category::Misc => "category-misc",
        }
    }
}

/// Everything a tool may need from the host application while drawing its UI.
pub struct AppCtx<'a> {
    pub config: &'a mut Config,
    pub journal: &'a ChangeJournal,
    pub i18n: &'a I18n,
    /// Whether the current process runs with an elevated (administrator) token.
    pub elevated: bool,
}

/// A single utility inside the suite (e.g. Startup Manager).
///
/// Implementations must never block in [`Tool::ui`]; long work goes through
/// [`crate::task::BackgroundTask`]. Any change to the system must be recorded via
/// [`AppCtx::journal`] and must honour [`Config::dry_run`].
pub trait Tool {
    /// Stable identifier, also used as the Fluent key prefix (`<id>-title`).
    fn id(&self) -> &'static str;
    fn category(&self) -> Category;
    /// Tools needing admin rights show a "restart as administrator" banner when not elevated.
    fn requires_admin(&self) -> bool {
        false
    }
    fn ui(&mut self, ui: &mut egui::Ui, ctx: &mut AppCtx<'_>);

    /// Localised title, defaults to the `<id>-title` Fluent message.
    fn title(&self, i18n: &I18n) -> String {
        i18n.tr(&format!("{}-title", self.id()))
    }
}

use crate::egui;
