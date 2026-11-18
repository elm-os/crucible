//! The project settings that reads depend on (contract 12.4 and 13.1). The control plane will own them
//! (milestone 4); until then they come from configuration through [`StaticProjectSettings`].

use std::fmt;

use crucible_query::RetentionSettings;
use crucible_records::ProjectId;

/// The default maximum window of a list or analytics request, in days (contract 12.4).
pub const DEFAULT_MAX_WINDOW_DAYS: u32 = 30;

/// The settings of one project that its reads depend on.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ProjectSettings {
    /// Retention (contract 13.1): reads hide what it has expired.
    pub retention: RetentionSettings,
    /// The widest window a list or analytics request may ask for, in days (contract 12.4).
    pub max_window_days: u32,
}

impl Default for ProjectSettings {
    /// Data kept forever (contract 13.1) and windows of up to 30 days (contract 12.4).
    fn default() -> Self {
        Self {
            retention: RetentionSettings::default(),
            max_window_days: DEFAULT_MAX_WINDOW_DAYS,
        }
    }
}

/// Where the API finds a project's settings. Like key verification, it answers from memory: the control plane
/// implementation will serve a snapshot.
pub trait ProjectSettingsSource: fmt::Debug + Send + Sync {
    /// The settings of `project_id`, a project the caller has been authorized for.
    fn settings(&self, project_id: ProjectId) -> ProjectSettings;
}

/// The same settings for every project, from configuration.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct StaticProjectSettings(pub ProjectSettings);

impl ProjectSettingsSource for StaticProjectSettings {
    fn settings(&self, _: ProjectId) -> ProjectSettings {
        self.0
    }
}
