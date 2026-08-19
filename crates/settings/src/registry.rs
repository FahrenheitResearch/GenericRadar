//! The contributed shape of the settings menu: plain data declarations.
//!
//! A category is `(id, label, list of typed items)`. Any module can declare
//! one - typically as a `pub fn settings_category() -> SettingsCategory` -
//! and the application collects them into one [`SettingsRegistry`] at
//! startup. The master settings window renders the registry generically, so
//! adding a knob anywhere in the workspace never touches the menu code.
//!
//! Ids are the persistence contract. A value is stored under
//! `(category.id, setting.id)`, so ids must never be reused for a different
//! meaning: a settings file written last month names its choices by these
//! strings. Renaming an id orphans the stored value (harmless - it is
//! carried, ignored, and the default applies); *reusing* one misreads it.

use crate::value::SettingValue;

/// One option of a [`SettingKind::Choice`].
#[derive(Clone, Debug, PartialEq)]
pub struct ChoiceOption {
    /// Stable stored identifier. Same rules as setting ids.
    pub id: String,
    /// What the menu shows.
    pub label: String,
}

impl ChoiceOption {
    pub fn new(id: impl Into<String>, label: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
        }
    }
}

/// The type, range and default of one setting.
///
/// Every kind carries its own default so resolution can never fail: a stored
/// value that is missing, malformed or out of range resolves to something the
/// declaring module chose, never to a blank.
#[derive(Clone, Debug, PartialEq)]
pub enum SettingKind {
    /// On/off.
    Toggle { default: bool },
    /// A real number on a closed range.
    Slider {
        min: f64,
        max: f64,
        default: f64,
        /// Display precision, for the widget; not a storage constraint.
        decimals: u8,
        /// Unit suffix for the widget ("km", "s", "×"). Empty for none.
        unit: String,
    },
    /// A whole number on a closed range.
    Integer {
        min: i64,
        max: i64,
        default: i64,
        unit: String,
    },
    /// One of a fixed list of identified options.
    Choice {
        options: Vec<ChoiceOption>,
        default_id: String,
    },
    /// Free text, for things like a site id.
    Text {
        default: String,
        placeholder: String,
        /// Longest accepted text; longer stored values are truncated on
        /// resolution rather than rejected.
        max_len: usize,
    },
}

impl SettingKind {
    pub fn default_value(&self) -> SettingValue {
        match self {
            Self::Toggle { default } => SettingValue::Bool(*default),
            Self::Slider { default, .. } => SettingValue::Float(*default),
            Self::Integer { default, .. } => SettingValue::Int(*default),
            Self::Choice { default_id, .. } => SettingValue::Text(default_id.clone()),
            Self::Text { default, .. } => SettingValue::Text(default.clone()),
        }
    }

    /// Resolve a stored value against this kind: coerce, clamp to range,
    /// validate choice ids, and fall back to the default. Total on purpose -
    /// a stale or hand-edited file must degrade a value to its default, never
    /// take a setting (or the pane it drives) down with it.
    pub fn sanitize(&self, stored: Option<&SettingValue>) -> SettingValue {
        match self {
            Self::Toggle { default } => {
                SettingValue::Bool(stored.and_then(SettingValue::as_bool).unwrap_or(*default))
            }
            Self::Slider {
                min, max, default, ..
            } => {
                let value = stored
                    .and_then(SettingValue::as_float)
                    .filter(|value| value.is_finite())
                    .unwrap_or(*default);
                SettingValue::Float(value.clamp(*min, *max))
            }
            Self::Integer {
                min, max, default, ..
            } => {
                let value = stored.and_then(SettingValue::as_int).unwrap_or(*default);
                SettingValue::Int(value.clamp(*min, *max))
            }
            Self::Choice {
                options,
                default_id,
            } => {
                let value = stored
                    .and_then(SettingValue::as_text)
                    .filter(|id| options.iter().any(|option| option.id == *id))
                    .unwrap_or(default_id);
                SettingValue::Text(value.to_owned())
            }
            Self::Text {
                default, max_len, ..
            } => {
                let value = stored.and_then(SettingValue::as_text).unwrap_or(default);
                let mut value = value.to_owned();
                if value.len() > *max_len {
                    // Truncate on a character boundary; a site id or a path
                    // fragment cut mid-codepoint would panic `String::truncate`.
                    let mut cut = *max_len;
                    while cut > 0 && !value.is_char_boundary(cut) {
                        cut -= 1;
                    }
                    value.truncate(cut);
                }
                SettingValue::Text(value)
            }
        }
    }
}

/// One declared setting: identity, words, type.
#[derive(Clone, Debug, PartialEq)]
pub struct SettingSpec {
    /// Stable stored identifier within its category.
    pub id: String,
    /// The menu row's name.
    pub label: String,
    /// One or two sentences under the control. Shown inline, not on hover:
    /// hover does not exist on glass and this application ships to glass.
    pub help: String,
    pub kind: SettingKind,
    /// `false` for a setting that is declared - so its id, range and default
    /// are the contract - but whose owning code has not been wired to read it
    /// yet. The menu draws it disabled. The declaration still matters: the
    /// stored value survives, and the wiring lands without a menu change.
    pub enabled: bool,
}

impl SettingSpec {
    pub fn new(id: impl Into<String>, label: impl Into<String>, kind: SettingKind) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            help: String::new(),
            kind,
            enabled: true,
        }
    }

    pub fn help(mut self, help: impl Into<String>) -> Self {
        self.help = help.into();
        self
    }

    /// Mark the setting as declared-but-not-wired. See [`Self::enabled`].
    pub fn pending_wiring(mut self) -> Self {
        self.enabled = false;
        self
    }
}

/// One page of the settings menu, declared by the module that owns the knobs.
#[derive(Clone, Debug, PartialEq)]
pub struct SettingsCategory {
    /// Stable stored identifier.
    pub id: String,
    /// The page's name in the category list.
    pub label: String,
    pub settings: Vec<SettingSpec>,
}

impl SettingsCategory {
    pub fn new(
        id: impl Into<String>,
        label: impl Into<String>,
        settings: Vec<SettingSpec>,
    ) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            settings,
        }
    }
}

/// The collected menu: every contributed category, in contribution order.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SettingsRegistry {
    categories: Vec<SettingsCategory>,
}

impl SettingsRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a category. Registering an id that already exists appends the new
    /// items to the existing page instead of duplicating it, so two crates
    /// can contribute to one page; a duplicated *setting* id within a page is
    /// a programming error and the first declaration wins (resolution is by
    /// first match, and a test over the real catalog pins uniqueness).
    pub fn register(&mut self, category: SettingsCategory) {
        if let Some(existing) = self
            .categories
            .iter_mut()
            .find(|existing| existing.id == category.id)
        {
            existing.settings.extend(category.settings);
        } else {
            self.categories.push(category);
        }
    }

    pub fn categories(&self) -> &[SettingsCategory] {
        &self.categories
    }

    pub fn category(&self, id: &str) -> Option<&SettingsCategory> {
        self.categories.iter().find(|category| category.id == id)
    }

    pub fn setting(&self, category_id: &str, setting_id: &str) -> Option<&SettingSpec> {
        self.category(category_id)?
            .settings
            .iter()
            .find(|setting| setting.id == setting_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn slider() -> SettingKind {
        SettingKind::Slider {
            min: 0.0,
            max: 1.0,
            default: 0.5,
            decimals: 2,
            unit: String::new(),
        }
    }

    #[test]
    fn sanitize_clamps_ranges_and_falls_back_to_defaults() {
        let kind = slider();
        assert_eq!(
            kind.sanitize(Some(&SettingValue::Float(7.0))),
            SettingValue::Float(1.0)
        );
        assert_eq!(
            kind.sanitize(Some(&SettingValue::Float(f64::NAN))),
            SettingValue::Float(0.5)
        );
        assert_eq!(
            kind.sanitize(Some(&SettingValue::Text("nope".into()))),
            SettingValue::Float(0.5)
        );
        assert_eq!(kind.sanitize(None), SettingValue::Float(0.5));
    }

    #[test]
    fn sanitize_rejects_a_choice_id_that_is_not_offered() {
        let kind = SettingKind::Choice {
            options: vec![
                ChoiceOption::new("slate", "Slate Dark"),
                ChoiceOption::new("daylight", "Daylight"),
            ],
            default_id: "slate".to_owned(),
        };
        assert_eq!(
            kind.sanitize(Some(&SettingValue::Text("daylight".into()))),
            SettingValue::Text("daylight".into())
        );
        assert_eq!(
            kind.sanitize(Some(&SettingValue::Text("neon".into()))),
            SettingValue::Text("slate".into())
        );
    }

    #[test]
    fn sanitize_truncates_text_on_a_character_boundary() {
        let kind = SettingKind::Text {
            default: String::new(),
            placeholder: String::new(),
            max_len: 5,
        };
        // 'é' is two bytes; a byte-5 cut would land inside the second one.
        let stored = SettingValue::Text("KTLXé".to_owned());
        assert_eq!(
            kind.sanitize(Some(&stored)),
            SettingValue::Text("KTLX".into())
        );
    }

    #[test]
    fn registering_the_same_category_id_twice_merges_the_pages() {
        let mut registry = SettingsRegistry::new();
        registry.register(SettingsCategory::new(
            "map",
            "Map",
            vec![SettingSpec::new("a", "A", slider())],
        ));
        registry.register(SettingsCategory::new(
            "map",
            "Map",
            vec![SettingSpec::new("b", "B", slider())],
        ));
        assert_eq!(registry.categories().len(), 1);
        assert!(registry.setting("map", "a").is_some());
        assert!(registry.setting("map", "b").is_some());
    }
}
