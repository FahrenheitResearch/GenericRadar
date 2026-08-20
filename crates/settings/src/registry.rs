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
    /// One line under the label in the open menu: what picking this one
    /// means. Empty for the many choices whose label already says it
    /// ("Knots first", "Every 50 km") — the menu draws nothing then, so a
    /// list of self-explanatory options stays as tight as it was.
    ///
    /// It exists for the lists where the label CANNOT say it. A theme is the
    /// case that forced it: eight entries reading "Paper", "Broadcast desk",
    /// "Amber ops" tell an analyst nothing about which one to pick on a
    /// glare-lit bench, and the answer was already written down in each
    /// theme's own `description` — it just had nowhere to appear.
    pub description: String,
}

impl ChoiceOption {
    pub fn new(id: impl Into<String>, label: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            description: String::new(),
        }
    }

    /// Add the line shown under the label in the open menu.
    pub fn describe(mut self, description: impl Into<String>) -> Self {
        self.description = description.into();
        self
    }
}

/// What the leftmost position of a [`SettingKind::Slider`] means.
///
/// This is a claim about the control's *range*, and two things follow from it.
///
/// A `Number` slider's ends are both ordinary values - a dim level, a frame
/// budget, a smoothing radius - so the nearest legal number is the closest
/// thing to what an unreadable stored value asked for, and the widget's
/// readout at the minimum is that number.
///
/// An `Off` slider is asymmetric: it does nothing at all at the minimum and
/// as much as it can at the maximum. Clamping there is not a small error. A
/// gate filter's `filter_min_dbz` of 900 - a hand edit, a file from a future
/// build, a byte flipped on disk - clamps to the top of the range, which on a
/// real scene removes the bloom, the precipitation shield and most of the
/// convective line; the analyst never chose it and cannot tell from the
/// picture why the echo went. The two failure directions are not symmetric
/// for a control whose job is censoring, so an unreadable number on an `Off`
/// slider resolves to the declared default rather than to the strongest
/// setting on offer.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum SliderFloor {
    /// The minimum is just the smallest number the control offers.
    #[default]
    Number,
    /// The control is inert at the minimum: that position means *off*.
    Off,
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
        /// What `min` means. See [`SliderFloor`] - it decides both how an
        /// out-of-range stored value resolves and what the widget writes at
        /// the leftmost stop.
        floor: SliderFloor,
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

    /// The value written the way the control writes it: a choice by its
    /// option label rather than its stored id, a slider at its declared
    /// precision with its unit, a toggle as on/off.
    ///
    /// This is what "your value" and "the default" say in a modified marker,
    /// a reset confirmation and an import summary. Those three all have to
    /// agree with the widget above them, so they all read it from here rather
    /// than each formatting a number its own way.
    pub fn display(&self, value: &SettingValue) -> String {
        match (self, value) {
            (Self::Toggle { .. }, SettingValue::Bool(on)) => {
                if *on { "on" } else { "off" }.to_owned()
            }
            (Self::Slider { decimals, unit, .. }, _) => {
                let number = value.as_float().unwrap_or_default();
                let text = format!("{number:.*}", usize::from(*decimals));
                if unit.is_empty() {
                    text
                } else {
                    format!("{text} {unit}")
                }
            }
            (Self::Integer { unit, .. }, _) => {
                let number = value.as_int().unwrap_or_default();
                if unit.is_empty() {
                    number.to_string()
                } else {
                    format!("{number} {unit}")
                }
            }
            (Self::Choice { options, .. }, SettingValue::Text(id)) => options
                .iter()
                .find(|option| option.id == *id)
                .map(|option| option.label.clone())
                .unwrap_or_else(|| id.clone()),
            (Self::Text { .. }, SettingValue::Text(text)) if text.is_empty() => {
                "(empty)".to_owned()
            }
            (Self::Text { .. }, SettingValue::Text(text)) => text.clone(),
            // A value whose shape does not match its kind cannot reach the
            // screen through `sanitize`, but a caller may hand one over
            // anyway; printing it is more useful than panicking.
            (_, SettingValue::Bool(on)) => on.to_string(),
            (_, SettingValue::Int(number)) => number.to_string(),
            (_, SettingValue::Float(number)) => number.to_string(),
            (_, SettingValue::Text(text)) => text.clone(),
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
                min,
                max,
                default,
                floor,
                ..
            } => {
                let stored = stored
                    .and_then(SettingValue::as_float)
                    .filter(|value| value.is_finite());
                let value = match (stored, floor) {
                    // An `Off` slider refuses to guess: a number outside the
                    // declared range says the file and this build disagree,
                    // and the safe reading of that disagreement is the
                    // declaring module's default, not the end of the range
                    // the number happened to overshoot. See `SliderFloor`.
                    (Some(value), SliderFloor::Off) if !(*min..=*max).contains(&value) => *default,
                    (Some(value), _) => value,
                    (None, _) => *default,
                };
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
    /// The subsection heading this item sits under, or empty for none.
    ///
    /// A heading is presentation, not persistence: it never appears in the
    /// stored file, and renaming one loses nothing. Sections are *runs* of
    /// consecutive items carrying the same heading (see
    /// [`SettingsCategory::sections`]), so declaration order is the order on
    /// screen and a category that sets no heading renders exactly as it did
    /// before headings existed.
    pub group: String,
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
            group: String::new(),
            enabled: true,
        }
    }

    /// Put this item under a subsection heading. Consecutive items given the
    /// same heading form one section; the heading is drawn once, above the
    /// first item of the run.
    ///
    /// Long pages are the reason. A page of nineteen sliders reads as a wall
    /// whatever the sliders say, and grouping is the only thing that turns it
    /// back into structure. The heading is declared here rather than chosen
    /// by the window so that a crate contributing a page describes its own
    /// shape and the window renders whatever it was handed - the same
    /// contract the rest of this module keeps.
    pub fn group(mut self, group: impl Into<String>) -> Self {
        self.group = group.into();
        self
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

    /// The page split into subsections, in declaration order.
    ///
    /// A section is a *run* of consecutive settings sharing one
    /// [`SettingSpec::group`]. Runs rather than a group-by on purpose: a
    /// group-by would silently reorder a page whose author interleaved two
    /// headings, and the declaration order of a settings page is a deliberate
    /// reading order, not an accident. Two runs that happen to carry the same
    /// heading are drawn as two sections with that heading, which is exactly
    /// what the declaration says.
    ///
    /// A category that sets no headings yields **one** section with an empty
    /// heading holding every setting in order - the same list, in the same
    /// order, so a page that declares no groups renders exactly as it did
    /// before groups existed.
    pub fn sections(&self) -> Vec<SettingsSection<'_>> {
        let mut sections: Vec<SettingsSection<'_>> = Vec::new();
        let mut start = 0usize;
        while start < self.settings.len() {
            let heading = self.settings[start].group.as_str();
            let mut end = start + 1;
            while end < self.settings.len() && self.settings[end].group == heading {
                end += 1;
            }
            sections.push(SettingsSection {
                heading,
                settings: &self.settings[start..end],
            });
            start = end;
        }
        sections
    }

    /// Whether any item on this page asked for a heading. `false` is the
    /// no-subsections page the window must render unchanged.
    pub fn has_sections(&self) -> bool {
        self.settings.iter().any(|spec| !spec.group.is_empty())
    }
}

/// One run of consecutive settings under a shared heading. See
/// [`SettingsCategory::sections`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SettingsSection<'a> {
    /// Empty for the items a page declared without a heading.
    pub heading: &'a str,
    pub settings: &'a [SettingSpec],
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
            floor: SliderFloor::Number,
        }
    }

    /// The same range declared as a control that is *off* at its minimum.
    fn censoring_slider() -> SettingKind {
        let SettingKind::Slider {
            min,
            max,
            default,
            decimals,
            unit,
            ..
        } = slider()
        else {
            unreachable!("slider() builds a slider")
        };
        SettingKind::Slider {
            min,
            max,
            default,
            decimals,
            unit,
            floor: SliderFloor::Off,
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

    /// The point of [`SliderFloor::Off`]: a stored number this build cannot
    /// account for must not resolve to the far end of the range.
    ///
    /// The same 7.0 that clamps to 1.0 above - the strongest setting the
    /// control offers - falls back to the default here, and does so in both
    /// directions, so neither a corrupt file nor a file from a build with a
    /// wider range can turn a censor on that nobody asked for.
    #[test]
    fn an_off_floored_slider_falls_back_to_its_default_instead_of_clamping() {
        let kind = censoring_slider();
        for stranger in [7.0_f64, 900.0, -4.0, f64::INFINITY, f64::NEG_INFINITY] {
            assert_eq!(
                kind.sanitize(Some(&SettingValue::Float(stranger))),
                SettingValue::Float(0.5),
                "{stranger} did not fall back to the declared default"
            );
        }
        // Everything the ordinary slider already did, unchanged: a legal
        // number is still the value, and a missing or malformed one is still
        // the default.
        assert_eq!(
            kind.sanitize(Some(&SettingValue::Float(0.25))),
            SettingValue::Float(0.25)
        );
        for edge in [0.0_f64, 1.0] {
            assert_eq!(
                kind.sanitize(Some(&SettingValue::Float(edge))),
                SettingValue::Float(edge),
                "{edge} is inside the declared range and must be kept"
            );
        }
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

    /// The promise the window relies on to leave every existing page alone:
    /// a category that declares no headings is ONE section, in declaration
    /// order, holding exactly the settings that were declared.
    #[test]
    fn a_category_with_no_headings_is_one_unheaded_section_in_declaration_order() {
        let category = SettingsCategory::new(
            "map",
            "Map",
            vec![
                SettingSpec::new("a", "A", slider()),
                SettingSpec::new("b", "B", slider()),
                SettingSpec::new("c", "C", slider()),
            ],
        );
        assert!(!category.has_sections());
        let sections = category.sections();
        assert_eq!(sections.len(), 1);
        assert_eq!(sections[0].heading, "");
        assert_eq!(
            sections[0]
                .settings
                .iter()
                .map(|spec| spec.id.as_str())
                .collect::<Vec<_>>(),
            ["a", "b", "c"]
        );
    }

    #[test]
    fn headings_split_a_page_into_runs_without_reordering_it() {
        let category = SettingsCategory::new(
            "vol3d",
            "3D Volume",
            vec![
                SettingSpec::new("lead", "Lead", slider()),
                SettingSpec::new("a", "A", slider()).group("Ramp"),
                SettingSpec::new("b", "B", slider()).group("Ramp"),
                SettingSpec::new("c", "C", slider()).group("Box"),
                // A second run under a heading already used earlier stays a
                // second run: reordering a page to merge them would move
                // settings the author put where they are on purpose.
                SettingSpec::new("d", "D", slider()).group("Ramp"),
            ],
        );
        assert!(category.has_sections());
        let sections = category.sections();
        let shape: Vec<(&str, Vec<&str>)> = sections
            .iter()
            .map(|section| {
                (
                    section.heading,
                    section
                        .settings
                        .iter()
                        .map(|spec| spec.id.as_str())
                        .collect(),
                )
            })
            .collect();
        assert_eq!(
            shape,
            vec![
                ("", vec!["lead"]),
                ("Ramp", vec!["a", "b"]),
                ("Box", vec!["c"]),
                ("Ramp", vec!["d"]),
            ]
        );
        // Whatever the split, every setting appears exactly once.
        assert_eq!(
            sections
                .iter()
                .map(|section| section.settings.len())
                .sum::<usize>(),
            category.settings.len()
        );
    }

    #[test]
    fn an_empty_category_has_no_sections_at_all() {
        let category = SettingsCategory::new("empty", "Empty", Vec::new());
        assert!(category.sections().is_empty());
        assert!(!category.has_sections());
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
