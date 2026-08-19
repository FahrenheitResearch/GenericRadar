//! The shell-injection contract, in its own binary because the override is
//! process-global: a mobile shell sets the roots once before the UI starts,
//! and every later lookup honours them.

use std::path::Path;

#[test]
fn injected_roots_win_and_are_set_once() {
    assert!(settings::set_app_config_root("Z:/sandbox/config"));
    assert!(settings::set_app_cache_root("Z:/sandbox/caches"));
    assert_eq!(settings::app_config_root(), Path::new("Z:/sandbox/config"));
    assert_eq!(settings::app_cache_root(), Path::new("Z:/sandbox/caches"));
    assert_eq!(
        settings::default_settings_file(),
        Path::new("Z:/sandbox/config/settings.json")
    );
    // A second injection is refused rather than splitting state mid-session.
    assert!(!settings::set_app_config_root("Z:/other"));
    assert_eq!(settings::app_config_root(), Path::new("Z:/sandbox/config"));
}
