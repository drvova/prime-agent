//! Settings subsystem: typed settings documents, global/project storage,
//! legacy migrations, the manager, and a distribution build's first-run
//! configuration import and one-time history import.

pub(crate) mod distribution_import;
pub(crate) mod history_import;
pub(crate) mod interactive_settings;
pub(crate) mod load;
pub(crate) mod manager;
pub(crate) mod merge;
pub(crate) mod profiles;
pub(crate) mod storage;
pub(crate) mod types;

#[cfg(test)]
mod persistence_tests;

pub use distribution_import::{import_official_config, OfficialConfigImport};
pub use history_import::{
    plan_official_history_import, skip_official_history_import, HistoryImportCheck,
    HistoryImportPlan, HistoryImportReport, HistoryImportRun, HISTORY_IMPORT_MARKER,
};
pub use manager::{IdleEviction, SettingsError, SettingsManager, DEFAULT_IDLE_EVICTION_MINUTES};
pub use profiles::{
    active_profile, list_profiles, load_profile, profile_path, save_profile, set_active_profile,
    validate_profile_name, Profile, DEFAULT_CREDENTIAL, PROFILES_DIR, PROFILE_ENV_VAR,
};
pub use storage::{
    FileSettingsStorage, InMemorySettingsStorage, SettingsScope, SettingsStorage, CONFIG_DIR_NAME,
};
pub use types::{
    AutoRefineSettings, AutonomousSettings, CacheRetentionSetting, CacheWarmingSetting,
    CompactionSettings, QueueModeSetting, Settings, ThinkingLevelSetting, TransportSetting,
    UpdateChannel, WebSearchProvider,
};
pub(crate) use types::{WebCrawlAiSettings, WebCrawlSettings};
