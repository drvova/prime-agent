//! Settings subsystem: typed settings documents, global/project storage,
//! legacy migrations, the manager, and a distribution build's first-run
//! configuration import.

pub(crate) mod distribution_import;
pub(crate) mod interactive_settings;
pub(crate) mod load;
pub(crate) mod manager;
pub(crate) mod merge;
pub(crate) mod storage;
pub(crate) mod types;

#[cfg(test)]
mod persistence_tests;

pub use distribution_import::{import_official_config, OfficialConfigImport};
pub use manager::{IdleEviction, SettingsError, SettingsManager, DEFAULT_IDLE_EVICTION_MINUTES};
pub use storage::{
    FileSettingsStorage, InMemorySettingsStorage, SettingsScope, SettingsStorage, CONFIG_DIR_NAME,
};
pub use types::{
    AutoRefineSettings, AutonomousSettings, CacheRetentionSetting, CacheWarmingSetting,
    CompactionSettings, QueueModeSetting, Settings, ThinkingLevelSetting, TransportSetting,
    UpdateChannel, WebSearchProvider,
};
