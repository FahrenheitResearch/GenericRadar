//! Where settings and caches live, per platform, with shell injection.
//!
//! Nothing in this workspace may hardcode a desktop path deep in logic: an
//! iOS app runs in a sandbox whose paths only the shell knows, and Android
//! has no usable environment variables at all. So both roots here are
//! **injectable** - the platform shell calls [`set_app_config_root`] /
//! [`set_app_cache_root`] once, before the UI starts - and the functions
//! fall back to the desktop conventions this workspace already uses:
//!
//! * Windows: `%LOCALAPPDATA%\FahrenheitResearch\RadarWorkstation` (the same
//!   root `live_service::default_live_cache_dir` established, so all of the
//!   application's disk state is found in one place);
//! * Linux and the BSDs: `$XDG_CONFIG_HOME`/`$XDG_CACHE_HOME`, falling back
//!   to `~/.config` / `~/.cache`, under `radar-workstation`;
//! * macOS and iOS: `~/Library/Application Support/RadarWorkstation` for
//!   config and `~/Library/Caches/RadarWorkstation` for caches. On iOS
//!   `$HOME` is the app sandbox, so these are the sandbox-correct locations
//!   (`Library/Caches` is what the OS may purge and what stays out of iCloud
//!   backup); the shell should still inject explicitly rather than rely on it.
//!
//! The overrides are set-once ([`std::sync::OnceLock`]): a root that moved
//! mid-session would split the application's state across two directories.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

static CONFIG_ROOT: OnceLock<PathBuf> = OnceLock::new();
static CACHE_ROOT: OnceLock<PathBuf> = OnceLock::new();

/// Inject the config root (iOS/Android shells; tests). Returns `false` if a
/// root was already fixed - by an earlier call or by a path lookup having
/// already happened - in which case the existing root stays.
pub fn set_app_config_root(path: impl Into<PathBuf>) -> bool {
    CONFIG_ROOT.set(path.into()).is_ok()
}

/// Inject the cache root. Same contract as [`set_app_config_root`].
pub fn set_app_cache_root(path: impl Into<PathBuf>) -> bool {
    CACHE_ROOT.set(path.into()).is_ok()
}

/// Directory for configuration: the settings file, saved layouts, anything
/// the user would be upset to lose.
pub fn app_config_root() -> PathBuf {
    CONFIG_ROOT.get_or_init(default_config_root).clone()
}

/// Directory for re-downloadable data: Level II volumes, basemap tiles, the
/// site catalog. The OS (or the application's own eviction) may empty it.
pub fn app_cache_root() -> PathBuf {
    CACHE_ROOT.get_or_init(default_cache_root).clone()
}

/// The settings file itself.
pub fn default_settings_file() -> PathBuf {
    app_config_root().join("settings.json")
}

fn default_config_root() -> PathBuf {
    #[cfg(target_os = "windows")]
    {
        if let Some(base) = std::env::var_os("LOCALAPPDATA") {
            return PathBuf::from(base)
                .join("FahrenheitResearch")
                .join("RadarWorkstation");
        }
    }
    #[cfg(any(target_os = "macos", target_os = "ios"))]
    {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home)
                .join("Library")
                .join("Application Support")
                .join("RadarWorkstation");
        }
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "ios")))]
    {
        if let Some(base) = std::env::var_os("XDG_CONFIG_HOME") {
            return PathBuf::from(base).join("radar-workstation");
        }
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home)
                .join(".config")
                .join("radar-workstation");
        }
    }
    // Last resort (a stripped service environment, or an Android shell that
    // forgot to inject): a stable subdirectory of the system temp dir. Wrong
    // for real use, but visibly wrong - the path shows in the settings window
    // - rather than a crash before the first frame.
    std::env::temp_dir()
        .join("radar-workstation")
        .join("config")
}

fn default_cache_root() -> PathBuf {
    #[cfg(target_os = "windows")]
    {
        if let Some(base) = std::env::var_os("LOCALAPPDATA") {
            return PathBuf::from(base)
                .join("FahrenheitResearch")
                .join("RadarWorkstation")
                .join("cache");
        }
    }
    #[cfg(any(target_os = "macos", target_os = "ios"))]
    {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home)
                .join("Library")
                .join("Caches")
                .join("RadarWorkstation");
        }
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "ios")))]
    {
        if let Some(base) = std::env::var_os("XDG_CACHE_HOME") {
            return PathBuf::from(base).join("radar-workstation");
        }
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home).join(".cache").join("radar-workstation");
        }
    }
    std::env::temp_dir().join("radar-workstation").join("cache")
}

/// True when `path` is under the system temp directory - i.e. the last-resort
/// fallback fired. The settings window uses this to warn rather than let a
/// misconfigured shell silently store settings somewhere the OS empties.
pub fn is_fallback_root(path: &Path) -> bool {
    path.starts_with(std::env::temp_dir())
}

#[cfg(test)]
mod tests {
    use super::*;

    // The override behaviour is process-global (OnceLock), so it is proven in
    // its own integration-test binary: `tests/paths_override.rs`. Testing it
    // here would poison every other unit test's view of the default roots.

    #[test]
    fn desktop_roots_are_absolute_and_distinct() {
        let config = default_config_root();
        let cache = default_cache_root();
        assert!(config.is_absolute(), "{config:?}");
        assert!(cache.is_absolute(), "{cache:?}");
        assert_ne!(config, cache, "config and cache must not share a directory");
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn on_windows_the_roots_live_under_the_existing_fahrenheit_convention() {
        // The same root live_service::default_live_cache_dir writes under, so
        // every piece of on-disk state is in one place for the user.
        let config = default_config_root();
        assert!(
            config.ends_with("FahrenheitResearch/RadarWorkstation") || is_fallback_root(&config),
            "{config:?}"
        );
        let cache = default_cache_root();
        assert!(
            cache.ends_with("FahrenheitResearch/RadarWorkstation/cache")
                || is_fallback_root(&cache),
            "{cache:?}"
        );
    }
}
