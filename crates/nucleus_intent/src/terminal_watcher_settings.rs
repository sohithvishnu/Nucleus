//! Phase 4b-1's on-by-default, easy-opt-out setting for shell-hook
//! injection. Registers automatically via `#[derive(RegisterSetting)]`'s
//! `inventory::submit!` — no explicit `init()` call needed (same mechanism
//! `crates/call`'s `CallSettings` uses), which matters here since
//! `nucleus_intent` has no top-level `init()` of its own at all (see
//! `docs/NUCLEUS_STATUS.md`'s Architecture summary).

use settings::{RegisterSetting, Settings};

#[derive(Debug, RegisterSetting)]
pub struct NucleusTerminalWatcherSettings {
    pub enabled: bool,
}

impl Settings for NucleusTerminalWatcherSettings {
    fn from_settings(content: &settings::SettingsContent) -> Self {
        let settings = content.nucleus_terminal_watcher.clone().unwrap_or_default();
        Self {
            // Default-on with an easy opt-out, per explicit confirmation —
            // `unwrap_or(true)` rather than requiring `assets/settings/default.json`
            // to supply the value, so this degrades safely even if that
            // entry were ever removed.
            enabled: settings.enabled.unwrap_or(true),
        }
    }
}
