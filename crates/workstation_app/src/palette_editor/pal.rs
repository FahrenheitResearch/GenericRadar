//! Reading a `.pal` file back into editable state.
//!
//! This is not a second colour-table parser and must not become one. Nothing
//! here decides what a table *paints* - that is `ColorTable::parse`, which
//! [`super::model::EditorTable::to_color_table`] calls and which every pixel
//! the editor shows goes through. This exists for the one thing a `ColorTable`
//! cannot carry and the editor cannot work without: the `Scale:` row, which
//! the parser applies to the stops and then forgets, so without it the editor
//! would show engine values where the analyst typed knots.
//!
//! Everything else it reads - `Units:`, `Step:`, `Mode:`, `RF:` and the
//! two-colour ramp rows - it reads the way `ColorTable::parse` reads it,
//! including the same key normalisation and the same last-row-wins
//! precedence, so a file cannot mean one thing to the editor and another to
//! the renderer. Two pins, because one was not enough:
//! `every_header_row_is_read_the_way_the_shipped_parser_reads_it` covers the
//! header, and `every_colour_row_is_read_the_way_the_shipped_parser_reads_it`
//! covers the `Color:`/`Color4:` rows, where the two readers used to disagree
//! about how many components a ramp-pair end colour has.

use color_tables::Rgba8;

use super::model::{EditorTable, EditorUnits, Sampling, family_from_product_token};

/// Read a `.pal` into editable state.
///
/// `name` is the fallback when the file carries no `Name:` row - GR `.pal`
/// files never do, so for an imported palette this is the file stem.
///
/// `None` when the text holds fewer than two stops: `ColorTable` rejects that
/// and there would be nothing to preview, so it is refused at the door rather
/// than becoming an editor that cannot render itself.
pub fn from_pal_text(name: &str, text: &str) -> Option<EditorTable> {
    let mut name = name.to_owned();
    let mut product: Option<String> = None;
    let mut units: Option<EditorUnits> = None;
    let mut scale: Option<f32> = None;
    let mut step: Option<f32> = None;
    // The shipped parser's default when a file states no sampling at all is
    // `Interpolated` - the legacy sRGB lerp - so that is what an imported GR
    // palette must come back as, or opening and saving it would repaint it.
    let mut sampling = Sampling::SmoothLegacy;
    let mut range_folded: Option<Rgba8> = None;
    let mut rows: Vec<(f32, Rgba8, Option<Rgba8>)> = Vec::new();

    for original_line in text.lines() {
        let line = original_line.replace('\u{a0}', " ");
        let line = line.trim();
        if line.is_empty()
            || line.starts_with(';')
            || line.starts_with('#')
            || line.starts_with("$$")
        {
            continue;
        }
        let Some((raw_key, raw_value)) = split_key_value(line) else {
            continue;
        };
        let key = normalize_key(raw_key);
        let value = raw_value.trim();
        match key.as_str() {
            "name" => {
                if !value.is_empty() {
                    name = value.to_owned();
                }
            }
            "product" => product = non_empty(value),
            "units" => units = Some(EditorUnits::from_pal(value)),
            "scale" => scale = positive(value),
            "step" => {
                // The shipped parser turns a `Step:` row into a quantised
                // banded mode outright, and an unreadable value into a plain
                // banded one. Mirrored here so the editor agrees with the
                // picture.
                step = positive(value);
                sampling = Sampling::Stepped;
            }
            "mode" | "samplemode" | "interpolate" | "interpolation" | "smooth" => {
                // A recognised `Mode:` row overwrites the parser's single
                // sampling variable, so it also clears any step a `Step:` row
                // put there. An unrecognised one leaves both alone.
                if let Some(parsed) = sampling_from_token(value) {
                    sampling = parsed;
                    step = None;
                }
            }
            "rf" | "rangefolded" | "rangefoldedcolor" => {
                // Three components required, a fourth optional, alpha 245 by
                // default - the shipped parser's rule for this row alone. A
                // row it cannot read fails the whole file there, so it fails
                // the whole file here: the editor must not open a palette the
                // renderer refuses, or a save would rewrite the file without
                // the row that could not be read.
                let components = numbers(value);
                let base = color_at(&components, 0, 3, 245)?;
                let alpha = match components.get(3) {
                    Some(value) => byte(*value)?,
                    None => 245,
                };
                range_folded = Some(Rgba8 { a: alpha, ..base });
            }
            "color" | "color4" | "solidcolor" | "solidcolor4" => {
                let with_alpha = key.ends_with('4');
                // A row the shipped parser rejects fails the whole file here
                // too, rather than being dropped. Dropping it would leave the
                // editor holding a table the renderer refuses to build, and an
                // analyst editing rows that are not the ones in the file.
                rows.push(color_row(&numbers(value), with_alpha)?);
            }
            _ => {}
        }
    }

    // A stop whose value is not a number is dropped and the rest of the file
    // is kept, which is what `ColorTable::from_parts` does to the same row
    // (`stops.retain(|stop| stop.value.is_finite())`). Refusing the file
    // instead - which is what shortening the row in `numbers` used to cause -
    // made the editor reject a palette the renderer draws.
    rows.retain(|(value, _, _)| value.is_finite());
    if rows.len() < 2 {
        return None;
    }

    let family = product
        .as_deref()
        .and_then(family_from_product_token)
        .unwrap_or(color_tables::ColorTableFamily::Generic);
    // No `Units:` row means the parser scales by one, which is exactly what
    // `Unstated` does. Falling back to a family default instead would invent a
    // row the file never had, and the file would come back different from the
    // one that was read.
    let units = units.unwrap_or(EditorUnits::Unstated);
    let mut table = EditorTable::new(family, name);
    table.product = product;
    table.units = units;
    table.scale = scale;
    table.step = step;
    table.sampling = sampling;
    if let Some(color) = range_folded {
        table.range_folded = color;
    }
    table.clear_stops();
    for (value, color, ramp_end) in rows {
        table.push_stop(value, color, ramp_end);
    }
    Some(table)
}

/// One `Color:`/`Color4:` row: a value, a colour, and the ramp-pair end colour
/// when the row carries a second triple.
///
/// The FIRST colour's component count is fixed by the key, not sniffed from
/// how many numbers happen to be on the line: on a `Color:` row with a ramp
/// pair the fifth number is the second colour's red, and reading it as the
/// first colour's alpha - which a "3 or 4" rule would - turns a two-colour row
/// into one translucent colour.
///
/// The SECOND colour's is the opposite, and for the same reason - to read the
/// row the way `parse_color_stop` reads it. That function sizes the end colour
/// from what is left on the line after the value and the first colour, so both
/// of the GR forms the key does not predict are real:
///
/// * `Color4: -30 0 0 0 0 20 40 80` is a four-component first colour with a
///   three-component end, and end alpha 255. Sizing the end by the key made it
///   one number short and dropped the ramp.
/// * `Color: 10 255 0 0 0 255 0 128` is a three-component first colour with a
///   four-component end, and end alpha 128. Sizing the end by the key read
///   three of the four and forced the ramp target opaque.
///
/// Either mis-sizing survives a save, because the round-trip check compares
/// the editor's text against itself and never against the file that was read.
///
/// One or two numbers past the first colour are rejected outright rather than
/// dropped, again matching `parse_color_stop`: there is no reading under which
/// they are a colour, and a file the renderer refuses is not a file the editor
/// should quietly rewrite.
fn color_row(numbers: &[f32], with_alpha: bool) -> Option<(f32, Rgba8, Option<Rgba8>)> {
    let components = if with_alpha { 4 } else { 3 };
    let required = 1 + components;
    if numbers.len() < required {
        return None;
    }
    let color = color_at(numbers, 1, components, 255)?;
    let ramp_end = match numbers.len() - required {
        0 => None,
        1 | 2 => return None,
        // Four or more spare numbers: the first four are the end colour and
        // the rest are ignored, as they always were.
        3 => Some(color_at(numbers, required, 3, 255)?),
        _ => Some(color_at(numbers, required, 4, 255)?),
    };
    Some((numbers[0], color, ramp_end))
}

/// Read `components` numbers starting at `at` as one colour, or `None` when
/// there are not that many left or one of them is outside 0-255.
fn color_at(numbers: &[f32], at: usize, components: usize, default_alpha: u8) -> Option<Rgba8> {
    let component = |index: usize| -> Option<u8> { byte(*numbers.get(index)?) };
    Some(Rgba8::new(
        component(at)?,
        component(at + 1)?,
        component(at + 2)?,
        if components >= 4 {
            component(at + 3)?
        } else {
            default_alpha
        },
    ))
}

fn byte(value: f32) -> Option<u8> {
    (0.0..=255.0).contains(&value).then(|| value.round() as u8)
}

/// The parser's own `Mode:` vocabulary, split three ways instead of four
/// because the quantised mode is reached through `Step:`, never through a
/// word.
fn sampling_from_token(value: &str) -> Option<Sampling> {
    match value.trim().to_ascii_lowercase().as_str() {
        "false" | "no" | "off" | "0" | "step" | "stepped" | "discrete" | "nearest" => {
            Some(Sampling::Stepped)
        }
        "true" | "yes" | "on" | "1" | "smooth" | "linear" | "interpolate" | "interpolated" => {
            Some(Sampling::SmoothLegacy)
        }
        "continuous" | "perceptual" | "oklab" => Some(Sampling::SmoothPerceptual),
        _ => None,
    }
}

/// The shipped parser's key normalisation: whitespace and underscores dropped,
/// lower-cased, so `Solid Color4` and `solidcolor4` are one key.
fn normalize_key(key: &str) -> String {
    key.chars()
        .filter(|character| !character.is_ascii_whitespace() && *character != '_')
        .flat_map(char::to_lowercase)
        .collect()
}

/// A colon splits key from value; failing that, the first run of whitespace
/// does. Same rule as the shipped parser, so `Color 5 255 0 0` reads.
fn split_key_value(line: &str) -> Option<(&str, &str)> {
    if let Some((key, value)) = line.split_once(':') {
        return Some((key, value));
    }
    let mut parts = line.splitn(2, char::is_whitespace);
    Some((parts.next()?, parts.next()?))
}

/// `parse_numbers` from the shipped parser, character for character.
///
/// It used to drop non-finite numbers, and the shipped one does not. A `1e39`
/// in the value column parses to infinity rather than failing, so dropping it
/// shortened the row by one and the whole file was refused - while
/// `ColorTable::parse` read that same file happily, keeping every other row
/// and discarding the one stop it could not place (`from_parts` retains only
/// finite values). The editor refused a palette the renderer draws. Where the
/// non-finite number lands is now decided at the end of the file, the way the
/// parser decides it, rather than here.
fn numbers(value: &str) -> Vec<f32> {
    value
        .split(|character: char| {
            character.is_ascii_whitespace() || character == ',' || character == ';'
        })
        .filter_map(|token| {
            let token = token.trim();
            (!token.is_empty())
                .then(|| token.parse::<f32>().ok())
                .flatten()
        })
        .collect()
}

fn non_empty(value: &str) -> Option<String> {
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_owned())
}

fn positive(value: &str) -> Option<f32> {
    numbers(value)
        .first()
        .copied()
        .filter(|value| value.is_finite() && *value > 0.0)
}
