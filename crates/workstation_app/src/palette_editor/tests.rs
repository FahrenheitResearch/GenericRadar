//! What the editor promises, pinned.
//!
//! Split by what each group needs: the model and the `.pal` round trip need
//! nothing but the two functions that cross to `ColorTable`, the store needs a
//! scratch directory, and the window is driven one frame at a time against a
//! bare `egui::Context` - the same harness `product_picker` and `app` use.

use color_tables::{ColorTable, ColorTableFamily, Rgba8, TableRendering};
use eframe::egui;

use super::model::{
    EditorTable, EditorUnits, KNOT_TO_MPS, Sampling, family_from_product_token, product_token,
};
use super::pal;
use super::store;
use super::ui::{
    PaletteEditorInput, PaletteEditorOutcome, PaletteEditorState, draw_palette_editor,
};

/// A table with everything the dialect can express on it: transparency, a
/// ramp pair, a range-folded colour, non-integer values.
fn sample_table() -> EditorTable {
    let mut table = EditorTable::new(ColorTableFamily::Reflectivity, "Bench REF");
    table.clear_stops();
    table.push_stop(-10.0, Rgba8::new(0, 0, 0, 0), None);
    table.push_stop(5.0, Rgba8::opaque(4, 60, 110), None);
    table.push_stop(
        27.5,
        Rgba8::opaque(30, 180, 60),
        Some(Rgba8::opaque(240, 230, 40)),
    );
    table.push_stop(50.0, Rgba8::opaque(220, 40, 30), None);
    table.push_stop(70.0, Rgba8::new(255, 255, 255, 200), None);
    table.range_folded = Rgba8::new(126, 80, 196, 245);
    table
}

// --- units and scale -------------------------------------------------------

/// The one number the editor must not get wrong on its own: the factor that
/// turns a knot into a metre per second. Re-derived here through the shipped
/// parser, so the editor's copy of the constant cannot drift away from it.
#[test]
fn unit_conversion_uses_the_same_factor_the_parser_uses() {
    let parsed = ColorTable::parse(
        "probe",
        "units: kt\ncolor: 0 0 0 0\ncolor: 100 255 255 255\n",
    )
    .expect("the probe parses");
    let parser_factor = parsed.stops().last().expect("two stops").value / 100.0;
    assert!(
        (parser_factor - KNOT_TO_MPS).abs() < 1e-6,
        "the parser scales knots by {parser_factor}, the editor by {KNOT_TO_MPS}"
    );

    let mut table = EditorTable::new(ColorTableFamily::Velocity, "kt probe");
    table.clear_stops();
    table.push_stop(0.0, Rgba8::opaque(0, 0, 0), None);
    table.push_stop(100.0, Rgba8::opaque(255, 255, 255), None);
    table.units = EditorUnits::Knots;
    assert!((table.to_engine(100.0) - parsed.stops().last().unwrap().value).abs() < 1e-4);
}

/// Changing the unit re-expresses every number so each colour stays on the
/// same physical value - the whole point of the control.
#[test]
fn changing_units_preserves_what_each_colour_means() {
    let mut table = EditorTable::new(ColorTableFamily::Velocity, "convert");
    table.clear_stops();
    table.push_stop(-50.0, Rgba8::opaque(0, 255, 0), None);
    table.push_stop(50.0, Rgba8::opaque(255, 0, 0), None);
    table.units = EditorUnits::Knots;
    table.step = Some(10.0);
    let engine_before: Vec<f32> = table
        .stops()
        .iter()
        .map(|stop| table.to_engine(stop.value))
        .collect();

    table.set_units(EditorUnits::MetresPerSecond);

    assert_eq!(table.units, EditorUnits::MetresPerSecond);
    for (stop, engine) in table.stops().iter().zip(engine_before) {
        assert!(
            (table.to_engine(stop.value) - engine).abs() < 1e-3,
            "a colour moved off its own wind speed"
        );
    }
    // 50 kt is 25.72 m/s, and the numbers on screen say so now.
    assert!((table.stops()[1].value - 50.0 * KNOT_TO_MPS).abs() < 1e-3);
    // The band grid is a value too, and it converts with them.
    assert!((table.step.expect("a step") - 10.0 * KNOT_TO_MPS).abs() < 1e-3);
}

/// Scale is the other control and does the opposite thing: the numbers stay
/// and their meaning moves. Said in a test because the difference is the whole
/// reason both exist.
#[test]
fn setting_a_scale_reinterprets_instead_of_converting() {
    let mut table = EditorTable::new(ColorTableFamily::Velocity, "reinterpret");
    table.clear_stops();
    table.push_stop(0.0, Rgba8::opaque(0, 0, 0), None);
    table.push_stop(60.0, Rgba8::opaque(255, 255, 255), None);
    let before: Vec<f32> = table.stops().iter().map(|stop| stop.value).collect();

    table.set_scale(Some(2.0));

    let after: Vec<f32> = table.stops().iter().map(|stop| stop.value).collect();
    assert_eq!(before, after, "scale must not move a stop");
    assert_eq!(
        table.to_engine(60.0),
        30.0,
        "but each number means half as much"
    );
}

/// With a scale in force the unit is inert - the parser never looks at it - so
/// switching it must not move a value either.
#[test]
fn a_scaled_table_ignores_a_unit_change_because_the_parser_does() {
    let mut table = EditorTable::new(ColorTableFamily::Velocity, "scaled");
    table.clear_stops();
    table.push_stop(0.0, Rgba8::opaque(0, 0, 0), None);
    table.push_stop(60.0, Rgba8::opaque(255, 255, 255), None);
    table.set_scale(Some(2.0));
    let before: Vec<f32> = table.stops().iter().map(|stop| stop.value).collect();
    table.set_units(EditorUnits::Knots);
    let after: Vec<f32> = table.stops().iter().map(|stop| stop.value).collect();
    assert_eq!(before, after);
    assert_eq!(table.engine_factor(), 0.5);
}

// --- stop editing ----------------------------------------------------------

#[test]
fn stops_stay_sorted_and_keep_their_identity_across_a_value_edit() {
    let mut table = sample_table();
    let first = table.stops()[0].id;
    // Drag the bottom stop clear past the top one.
    table.set_value(first, 999.0);
    assert_eq!(
        table.stops().last().expect("stops").id,
        first,
        "the stop that moved is now the last one"
    );
    let values: Vec<f32> = table.stops().iter().map(|stop| stop.value).collect();
    let mut sorted = values.clone();
    sorted.sort_by(f32::total_cmp);
    assert_eq!(values, sorted, "stops must stay ascending");
}

#[test]
fn adding_a_stop_lands_midway_and_removing_one_stops_at_two() {
    let mut table = sample_table();
    let second = table.stops()[1].id;
    let inserted = table.insert_after(second).expect("a stop above the second");
    let value = table.stop(inserted).expect("the new stop").value;
    assert!(
        (value - (5.0 + 27.5) / 2.0).abs() < 1e-4,
        "inserted at {value}, expected the midpoint"
    );

    while table.stops().len() > 2 {
        let id = table.stops()[0].id;
        assert!(table.remove(id));
    }
    let last_pair = table.stops()[0].id;
    assert!(
        !table.remove(last_pair),
        "two stops is the floor: a one-stop table is not a colour table"
    );
    assert_eq!(table.stops().len(), 2);
}

/// Insertion is coloured so it changes no pixel a smooth table paints; an
/// analyst adds a stop to gain a handle, not to gain a stripe.
#[test]
fn inserting_a_stop_does_not_repaint_a_smooth_table() {
    let mut table = sample_table();
    table.sampling = Sampling::SmoothLegacy;
    let before = table.to_color_table().expect("builds");
    let second = table.stops()[1].id;
    table.insert_after(second).expect("inserted");
    let after = table.to_color_table().expect("builds");
    // Between the two stops the insertion sits between, the sRGB ramp is a
    // straight line and the midpoint is on it.
    let probe = table.to_engine(16.25);
    assert_eq!(before.sample(probe), after.sample(probe));
}

/// A table whose stops have all been typed onto one value is a state the stop
/// list reaches in one keystroke, and it used to be a dead end: the midpoint
/// of two equal values is that value again, so "Add stop" grew the list
/// without changing anything and `to_color_table` kept failing.
#[test]
fn a_collapsed_table_is_recovered_by_adding_a_stop() {
    let mut table = EditorTable::new(ColorTableFamily::Reflectivity, "Collapsed");
    let ids: Vec<_> = table.stops().iter().map(|stop| stop.id).collect();
    for id in &ids {
        table.set_value(*id, 95.0);
    }
    assert!(
        table.to_color_table().is_err(),
        "the state under test is a table that will not build"
    );

    let added = table.insert_after(ids[0]).expect("a stop is added");
    let values: Vec<f32> = table.stops().iter().map(|stop| stop.value).collect();
    assert_eq!(values.len(), 3);
    assert!(
        table.stop(added).expect("the new stop").value != 95.0,
        "the new stop landed on the pile again: {values:?}"
    );
    table
        .to_color_table()
        .expect("adding a stop makes the table a colour table again");
    // And with three stops the row's cut button comes back, so the pile can be
    // taken apart from the list as well.
    assert!(table.remove(ids[1]));
}

/// The same state, seen from the strip: an axis that is never zero-width, and
/// a drag speed that is not a thousandth of a unit per pixel.
#[test]
fn a_collapsed_table_still_has_an_axis_and_a_usable_drag_speed() {
    let mut table = EditorTable::new(ColorTableFamily::Reflectivity, "Collapsed");
    let ids: Vec<_> = table.stops().iter().map(|stop| stop.id).collect();
    for id in &ids {
        table.set_value(*id, 95.0);
    }
    assert_eq!(table.display_span(), (95.0, 95.0));
    let (low, high) = table.strip_span();
    assert!(high > low, "the strip axis collapsed: {low}..{high}");
    assert!(low < 95.0 && high > 95.0, "the stops fell off the axis");
    // Ninety-five thousand pixels of travel to drag a stop off 95 dBZ was the
    // number before; a full sweep of the strip is now the table's own range.
    assert!(
        table.drag_speed() > 0.01,
        "drag speed {} is still unusable",
        table.drag_speed()
    );

    // A healthy table is unaffected: the axis is exactly the stops' own span.
    let healthy = EditorTable::new(ColorTableFamily::Reflectivity, "Healthy");
    assert_eq!(healthy.strip_span(), healthy.display_span());
}

/// The gap the editor opens when there is no room follows the family's own
/// domain, so it is dBZ-sized on reflectivity and correlation-coefficient
/// sized on correlation coefficient rather than one unit on both.
#[test]
fn the_fallback_gap_is_scaled_to_the_measurement() {
    for family in ColorTableFamily::ALL {
        let table = EditorTable::new(family, "Gap");
        let (low, high) = family.nominal_domain();
        let gap = table.fallback_gap();
        assert!(gap > 0.0 && gap.is_finite(), "{family:?}: {gap}");
        assert!(
            gap < (high - low).abs(),
            "{family:?}: a gap of {gap} is wider than the whole domain"
        );
    }
    // And it follows the display unit, not the engine one: a knots table
    // measures its gap in knots.
    let mut knots = EditorTable::new(ColorTableFamily::Velocity, "Knots");
    let engine_gap = knots.fallback_gap();
    knots.set_units(EditorUnits::Knots);
    assert!(
        knots.fallback_gap() > engine_gap,
        "a knot is smaller than a metre per second, so the gap in knots is larger"
    );
}

// --- the file --------------------------------------------------------------

/// Save, read back, save again: byte-identical, ramp pairs and all. This is
/// the property that makes the file the source of truth rather than a lossy
/// export of it.
#[test]
fn a_saved_table_round_trips_through_its_own_text() {
    for sampling in Sampling::ALL {
        for units in EditorUnits::ALL {
            let mut table = sample_table();
            table.sampling = sampling;
            table.units = units;
            table.step = sampling.uses_step().then_some(5.0);
            table.scale = matches!(units, EditorUnits::Knots).then_some(1.94384);

            let text = table.pal_text();
            let reread = pal::from_pal_text(&table.name, &text)
                .unwrap_or_else(|| panic!("{sampling:?}/{units:?} did not read back"));
            assert_eq!(
                reread.pal_text(),
                text,
                "{sampling:?}/{units:?} did not survive a second write"
            );
            assert_eq!(
                reread.to_color_table().expect("re-read builds"),
                table.to_color_table().expect("original builds"),
                "{sampling:?}/{units:?} paints differently after a round trip"
            );
        }
    }
}

/// The whole header, one row at a time: everything that decides what a number
/// means has to come back, or the file means something else on reload.
#[test]
fn every_header_row_is_read_the_way_the_shipped_parser_reads_it() {
    let mut table = sample_table();
    table.family = ColorTableFamily::Velocity;
    table.product = Some("BV".to_owned());
    table.units = EditorUnits::Knots;
    table.scale = Some(1.94384);
    table.sampling = Sampling::Stepped;
    table.step = Some(2.5);
    table.range_folded = Rgba8::new(11, 22, 33, 44);

    let text = table.pal_text();
    let reread =
        pal::from_pal_text("ignored because the file names itself", &text).expect("reads back");

    assert_eq!(reread.name, "Bench REF");
    assert_eq!(reread.family, ColorTableFamily::Velocity);
    assert_eq!(reread.product.as_deref(), Some("BV"));
    assert_eq!(reread.units, EditorUnits::Knots);
    assert_eq!(reread.scale, Some(1.94384));
    assert_eq!(reread.sampling, Sampling::Stepped);
    assert_eq!(reread.step, Some(2.5));
    assert_eq!(reread.range_folded, Rgba8::new(11, 22, 33, 44));
    // And the shipped parser agrees about the sampling and the scale, which is
    // the half of the header the editor cannot check on its own.
    let built = reread.to_color_table().expect("builds");
    assert_eq!(built.sample_mode_label(), "quantized stepped");
    assert_eq!(built.rendering(), TableRendering::Stepped);
    assert!((built.step_size().expect("a step") - 2.5 / 1.94384).abs() < 1e-4);
}

/// A ramp pair is two colours on one row and must come back as two colours on
/// one row - not as one colour, and not as two rows.
#[test]
fn a_ramp_pair_survives_the_file_as_a_pair() {
    let table = sample_table();
    let text = table.pal_text();
    assert!(
        text.lines()
            .any(|line| line.starts_with("Color4: 27.5") && line.split_whitespace().count() == 10),
        "the ramp row should carry two RGBA quads:\n{text}"
    );
    let reread = pal::from_pal_text(&table.name, &text).expect("reads back");
    let ramped = reread
        .stops()
        .iter()
        .find(|stop| (stop.value - 27.5).abs() < 1e-6)
        .expect("the ramped stop");
    assert_eq!(ramped.ramp_end, Some(Rgba8::opaque(240, 230, 40)));
}

/// The ramp pair reaches the parser intact and paints what the row names: the
/// segment starts on the row's own colour and arrives at the declared one at
/// the next row, with no extra stop invented anywhere.
#[test]
fn a_ramp_pair_paints_the_two_colours_it_names() {
    let table = sample_table();
    let built = table.to_color_table().expect("builds");
    assert_eq!(
        built.stops().len(),
        table.stops().len(),
        "a ramp pair is one row and must stay one stop"
    );
    // At the bottom of the ramped interval, the row's own colour.
    assert_eq!(built.sample(27.5), Rgba8::opaque(30, 180, 60));
    // Just under the next row, all but arrived at the colour it ramps to.
    let approaching = built.sample(49.99);
    assert!(
        approaching.r.abs_diff(240) <= 1
            && approaching.g.abs_diff(230) <= 1
            && approaching.b.abs_diff(40) <= 1,
        "{approaching:?} is not the ramp target"
    );
    // And the next row still starts on its own colour.
    assert_eq!(built.sample(50.0), Rgba8::opaque(220, 40, 30));
    // The end colour is on the stop, not on a stop of its own.
    let ramped = built
        .stops()
        .iter()
        .find(|stop| (stop.value - 27.5).abs() < 1e-6)
        .expect("the ramped stop");
    assert_eq!(ramped.end_color, Some(Rgba8::opaque(240, 230, 40)));
}

/// The `Color:`/`Color4:` rows, read by both readers and compared row for
/// row.
///
/// The two used to disagree on the two GR forms the row key does not predict.
/// `parse_color_stop` sizes a ramp-pair end colour from what is left on the
/// line; the editor's reader sized it from the key, so a `Color4:` row with a
/// three-component end lost its ramp entirely and a `Color:` row with a
/// four-component end had its ramp target forced opaque. Neither showed up in
/// a save, because the round-trip check compares the editor's text against
/// itself and never against the file that was read.
#[test]
fn every_colour_row_is_read_the_way_the_shipped_parser_reads_it() {
    let text = "\
Product: BR
Mode: smooth
Color4: -30 0 0 0 0 20 40 80
Color: -10 255 0 0 0 255 0 128
Color4: 5 10 20 30 40 50 60 70 80
Color: 20 255 0 0 255 255 0
Color4: 15 1 2 3 4
Color: 25 1 2 3
Color: 35 1 2 3 4 5 6 7 8
";
    let shipped = ColorTable::parse("Bench", text).expect("the shipped parser reads it");
    let editable = pal::from_pal_text("Bench", text).expect("the editor reads it");
    let mirrored = editable.to_color_table().expect("builds");
    assert_eq!(
        mirrored.stops(),
        shipped.stops(),
        "the editor's reading of the colour rows is not the shipped one"
    );
    // Named individually too, so a regression says which form broke.
    let at = |value: f32| {
        shipped
            .stops()
            .iter()
            .find(|stop| (stop.value - value).abs() < 1e-6)
            .unwrap_or_else(|| panic!("no stop at {value}"))
    };
    assert_eq!(
        at(-30.0).end_color,
        Some(Rgba8::opaque(20, 40, 80)),
        "a Color4 row with a three-component end is still a ramp"
    );
    assert_eq!(
        at(-10.0).end_color,
        Some(Rgba8::new(0, 255, 0, 128)),
        "a Color row with a four-component end keeps its end alpha"
    );
    assert_eq!(at(15.0).end_color, None, "a flat row has no ramp");
    assert_eq!(at(25.0).end_color, None);
    assert_eq!(
        at(35.0).end_color,
        Some(Rgba8::new(4, 5, 6, 7)),
        "numbers past a complete end colour are ignored, as they always were"
    );
}

/// A row the shipped parser rejects fails the file here too.
///
/// One or two numbers past the first colour are not a colour under any
/// reading. Dropping the row instead would leave the editor holding a table
/// the renderer refuses to build, and would let a save rewrite the file
/// without the row that could not be read.
#[test]
fn a_colour_row_the_shipped_parser_rejects_is_not_read_here_either() {
    for text in [
        "Color: 10 1 2 3 4\nColor: 20 1 2 3\n",
        "Color4: 10 1 2 3 4 5 6\nColor4: 20 1 2 3 4\n",
        "Color: 10 1 2 300\nColor: 20 1 2 3\n",
    ] {
        assert!(
            ColorTable::parse("Bench", text).is_err(),
            "the shipped parser accepted {text:?}"
        );
        assert!(
            pal::from_pal_text("Bench", text).is_none(),
            "the editor accepted {text:?}"
        );
    }
}

/// Numbers a row cannot hold are read the way the shipped parser reads them,
/// which is not the same answer for every kind of them.
///
/// `1e39` parses as a number - infinity - rather than failing, and
/// `ColorTable::parse` keeps the file and discards the one stop it cannot
/// place (`from_parts` retains only finite values). The editor's reader used
/// to drop the token before the row was measured, which made the row one
/// number short and refused the WHOLE file: a palette the renderer draws could
/// not be opened. A component outside 0-255 is the opposite case - the shipped
/// parser refuses the file - and both readers have to make that call the same
/// way too, or a save would rewrite a file the renderer never accepted.
#[test]
fn a_number_the_row_cannot_hold_is_read_the_way_the_shipped_parser_reads_it() {
    // Kept, minus the stop that has no place on the axis.
    let dropped = "\
Product: BR
Mode: smooth
Color4: 1e39 10 20 30 255
Color4: 10 40 50 60 255
Color4: 20 70 80 90 255
";
    let shipped = ColorTable::parse("Bench", dropped).expect("the shipped parser reads it");
    assert_eq!(shipped.stops().len(), 2, "the infinite stop is dropped");
    let editable = pal::from_pal_text("Bench", dropped).expect("the editor reads it too");
    assert_eq!(
        editable.to_color_table().expect("builds").stops(),
        shipped.stops()
    );

    // Refused by both: a component outside 0-255, a non-finite component, a
    // range-folded row that is not a colour, and a file left with one stop.
    for text in [
        "Product: BR\nColor4: 5 300 0 0 255\nColor4: 20 1 2 3 4\n",
        "Product: BR\nColor4: 5 1e39 0 0 255\nColor4: 20 1 2 3 4\n",
        "Product: BR\nRF: 1e39 0 0\nColor4: 5 1 2 3 4\nColor4: 20 1 2 3 4\n",
        "Product: BR\nRF: 10 20\nColor4: 5 1 2 3 4\nColor4: 20 1 2 3 4\n",
        "Product: BR\nColor4: 1e39 1 2 3 4\nColor4: 20 1 2 3 4\n",
    ] {
        assert!(
            ColorTable::parse("Bench", text).is_err(),
            "the shipped parser accepted {text:?}"
        );
        assert!(
            pal::from_pal_text("Bench", text).is_none(),
            "the editor accepted {text:?}"
        );
    }

    // And a header number that is not a number leaves its row unset in both,
    // rather than meaning something else.
    let header =
        "Product: BR\nScale: 1e39\nStep: nonsense\nColor4: 5 1 2 3 4\nColor4: 20 1 2 3 4\n";
    let editable = pal::from_pal_text("Bench", header).expect("the editor reads it");
    assert_eq!(editable.scale, None);
    assert_eq!(editable.step, None);
    assert_eq!(
        editable.sampling,
        Sampling::Stepped,
        "an unreadable Step: still bands"
    );
    let shipped = ColorTable::parse("Bench", header).expect("the shipped parser reads it");
    assert_eq!(shipped.step_size(), None);
    assert_eq!(shipped.sample_mode_label(), "stepped");
    assert_eq!(
        editable.to_color_table().expect("builds").stops(),
        shipped.stops()
    );
}

/// Opening a GR palette and saving it back must not repaint it. The ramp rows
/// are the part that used to be lost, silently, on the first save.
#[test]
fn opening_a_gr_palette_and_saving_it_keeps_its_ramps() {
    let dir = scratch_dir("gr-ramps");
    let path = dir.join("shared.pal");
    let text = "\
Product: BR
Color4: -30 0 0 0 0 20 40 80
Color: 10 255 0 0 0 255 0 128
Color4: 50 255 255 255 255
";
    std::fs::write(&path, text).expect("write");
    let opened = store::load(&path).expect("loads");
    let before = ColorTable::parse("shared", text).expect("the shipped parser reads it");
    store::save(&opened, &path).expect("saves");
    let after = store::load(&path)
        .expect("loads again")
        .to_color_table()
        .expect("builds");
    assert_eq!(
        after.stops(),
        before.stops(),
        "a round trip through the editor changed the colours the file paints"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

// --- the name --------------------------------------------------------------

/// A name is not typed clean. A trailing space made every save fail - and fail
/// blaming the colours - because the writer trimmed the `Name:` row while
/// `to_color_table` named the parsed table from the untrimmed field, so the
/// two tables the round-trip check compares carried different names.
#[test]
fn a_name_with_space_around_it_saves_and_comes_back_trimmed() {
    let dir = scratch_dir("name-spaces");
    for typed in ["Bench ", " Bench", "  Bench  ", "\tBench\t"] {
        let mut table = sample_table();
        table.name = typed.to_owned();
        let path = dir.join("bench.pal");
        store::save(&table, &path).unwrap_or_else(|error| panic!("{typed:?} was refused: {error}"));
        assert_eq!(store::load(&path).expect("loads").name, "Bench");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// A name pasted out of a document brings a no-break space with it. Both `.pal`
/// readers turn U+00A0 into an ordinary space before they look at a line, so
/// the writer has to as well - otherwise the file says one thing and every
/// reader of it says another, which is exactly what the round-trip check is
/// there to refuse.
#[test]
fn a_no_break_space_in_a_name_is_written_as_an_ordinary_space() {
    let dir = scratch_dir("name-nbsp");
    let mut table = sample_table();
    table.name = "Bench\u{a0}REF".to_owned();
    let path = dir.join("bench.pal");
    store::save(&table, &path).expect("a pasted name saves");
    assert_eq!(store::load(&path).expect("loads").name, "Bench REF");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A nameless table is refused **by name**. The `Name:` row is what the file is
/// found by afterwards, and a failure that pointed at the colours instead sent
/// the analyst hunting through the stop list for a problem that was in the
/// header.
#[test]
fn a_table_with_no_name_is_refused_by_name_and_not_by_colour() {
    let dir = scratch_dir("name-empty");
    let mut table = sample_table();
    table.name = "   ".to_owned();
    let error = store::save(&table, &dir.join("bench.pal")).expect_err("refused");
    let message = error.to_string();
    assert!(
        message.contains("no name"),
        "the failure must point at the name: {message}"
    );
    assert!(
        !message.contains("colours"),
        "the failure must not blame the colours: {message}"
    );
    assert!(!dir.join("bench.pal").exists(), "nothing should be written");
    let _ = std::fs::remove_dir_all(&dir);
}

/// The editor's `Name:` row and the row `color_tables::declared_name` reads
/// are one row. The palette restore path uses the second to work out which
/// file holds which palette, and the editor uses the first; a difference
/// between them would be a saved table the restore path cannot find.
#[test]
fn the_name_row_reads_the_same_in_both_homes() {
    for name in ["Bench REF", "Storm: Detail / v2", "kt table", "a"] {
        let mut table = sample_table();
        table.name = name.to_owned();
        let text = table.pal_text();
        assert_eq!(
            color_tables::declared_name(&text).as_deref(),
            Some(name),
            "declared_name disagrees with the editor about {name:?}"
        );
        assert_eq!(
            pal::from_pal_text("fallback", &text).expect("reads").name,
            name
        );
    }
}

// --- crossing to ColorTable ------------------------------------------------

/// Every shipped palette must survive a trip through the editor unchanged, or
/// "duplicate and edit" would repaint the thing it duplicated.
///
/// "Unchanged" is what the table PAINTS, plus every stop's value and its own
/// colour. It is deliberately not stop-for-stop equality of `end_color`,
/// because that cannot hold and should not: a preset built in Rust leaves a
/// clear row's end colour undeclared and ramps out of it, while every table
/// that has been through `ColorTable::parse` carries the `.pal` dialect's
/// reading of its clear rows as a declared end (`hold_clear_gr_rows`). The
/// trip through the editor is a trip through that dialect - `to_color_table`
/// writes the file and reads it back - so the two representations differ by
/// construction. What must not differ is the picture, and this walks the
/// declared range at a step far finer than any radar's quantisation to say so.
#[test]
fn every_built_in_palette_round_trips_through_the_editor_unchanged() {
    for family in ColorTableFamily::ALL {
        for installed in color_tables::builtin_tables_for_family(family) {
            let editable = EditorTable::from_color_table(family, &installed);
            let rebuilt = editable
                .to_color_table()
                .unwrap_or_else(|error| panic!("{} did not rebuild: {error}", installed.name()));
            assert_eq!(
                rebuilt.stops().len(),
                installed.stops().len(),
                "{} lost or gained a stop",
                installed.name()
            );
            for (rebuilt_stop, installed_stop) in
                rebuilt.stops().iter().zip(installed.stops().iter())
            {
                assert_eq!(
                    (rebuilt_stop.value, rebuilt_stop.color),
                    (installed_stop.value, installed_stop.color),
                    "{} moved or recoloured a stop",
                    installed.name()
                );
            }
            let first = installed.stops()[0].value;
            let last = installed.stops()[installed.stops().len() - 1].value;
            let span = last - first;
            for step in 0..=2_000 {
                let value = first + span * (step as f32) / 2_000.0;
                assert_eq!(
                    rebuilt.sample(value),
                    installed.sample(value),
                    "{} paints {value} differently after a trip through the editor",
                    installed.name()
                );
            }
            assert_eq!(
                rebuilt.sample_mode_label(),
                installed.sample_mode_label(),
                "{} changed how it samples",
                installed.name()
            );
            assert_eq!(
                rebuilt.range_folded_rgba(),
                installed.range_folded_rgba(),
                "{} lost its range-folded colour",
                installed.name()
            );
        }
    }
}

#[test]
fn the_product_row_names_the_measurement_in_both_directions() {
    for family in ColorTableFamily::ALL {
        let Some(token) = product_token(family) else {
            assert_eq!(family, ColorTableFamily::Generic);
            continue;
        };
        assert_eq!(family_from_product_token(token), Some(family), "{token}");
    }
}

// --- the store -------------------------------------------------------------

fn scratch_dir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "palette-editor-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock after 1970")
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

#[test]
fn a_saved_file_reads_back_as_the_same_colour_table() {
    let dir = scratch_dir("save");
    let table = sample_table();
    let path = dir.join("bench.pal");
    store::save(&table, &path).expect("the round-trip check passes");

    let loaded = store::load(&path).expect("loads");
    assert_eq!(
        loaded.to_color_table().expect("builds"),
        table.to_color_table().expect("builds"),
        "the file on disk does not paint what the editor showed"
    );
    assert_eq!(loaded.name, table.name);
    assert_eq!(loaded.units, table.units);
    assert_eq!(loaded.sampling, table.sampling);
    assert_eq!(loaded.range_folded, table.range_folded);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn saving_twice_overwrites_the_same_file_and_leaves_no_temporary_behind() {
    let dir = scratch_dir("overwrite");
    let path = dir.join("bench.pal");
    let mut table = sample_table();
    store::save(&table, &path).expect("first save");
    table.name = "Bench REF, second pass".to_owned();
    store::save(&table, &path).expect("second save");

    let files: Vec<String> = std::fs::read_dir(&dir)
        .expect("listing")
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(files, vec!["bench.pal".to_owned()], "{files:?}");
    assert_eq!(
        store::load(&path).expect("loads").name,
        "Bench REF, second pass"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_file_stem_is_portable_and_never_empty() {
    assert_eq!(store::file_stem_for("AWIPS Wilson REF"), "awips-wilson-ref");
    assert_eq!(store::file_stem_for("  ../../etc/passwd  "), "etc-passwd");
    assert_eq!(store::file_stem_for("???"), "palette");
    assert_eq!(
        store::file_stem_for("Storm: Detail / v2"),
        "storm-detail-v2"
    );
    assert!(store::file_stem_for(&"x".repeat(400)).len() <= 64);
}

/// Two names long enough to be cut at the stem limit still get their own
/// files. Without the digest they shared a stem the moment their first
/// sixty-four alphanumerics matched, which for a family of palettes named off
/// one long prefix is every one of them.
#[test]
fn two_long_names_that_share_a_prefix_do_not_share_a_stem() {
    let prefix = "Convective Outlook Reflectivity Working Palette Revision".repeat(2);
    let first = store::file_stem_for(&format!("{prefix} one"));
    let second = store::file_stem_for(&format!("{prefix} two"));
    assert_ne!(first, second);
    assert!(first.len() <= 64 && second.len() <= 64, "{first} {second}");
    // Stable: the same name must produce the same file on every run and every
    // toolchain, or a saved palette would be orphaned by a rebuild.
    assert_eq!(first, store::file_stem_for(&format!("{prefix} one")));
}

/// The stem a name reduces to is many-to-one, so the file it points at is
/// opened and its `Name:` row read before it is believed. Without that check,
/// opening "Storm Detail v2" loaded "Storm: Detail / v2"'s file and the next
/// Save overwrote it.
#[test]
fn a_file_is_claimed_by_the_name_inside_it_and_not_by_its_filename() {
    let dir = scratch_dir("stem-collision");
    let mut first = sample_table();
    first.name = "Storm: Detail / v2".to_owned();
    let first_path = store::free_path_in(&dir, &first.name);
    store::save(&first, &first_path).expect("saves");
    assert_eq!(first_path, dir.join("storm-detail-v2.pal"));

    // The other name reduces to the same stem, and that file is not its file.
    assert_eq!(store::existing_file_in(&dir, "Storm Detail v2"), None);
    assert_eq!(
        store::existing_file_in(&dir, "Storm: Detail / v2"),
        Some(first_path.clone())
    );

    // So it takes a file of its own, and both are still readable afterwards.
    let mut second = sample_table();
    second.name = "Storm Detail v2".to_owned();
    let second_path = store::free_path_in(&dir, &second.name);
    assert_ne!(second_path, first_path);
    store::save(&second, &second_path).expect("saves");
    assert_eq!(
        store::load(&first_path).expect("loads").name,
        "Storm: Detail / v2"
    );
    assert_eq!(
        store::load(&second_path).expect("loads").name,
        "Storm Detail v2"
    );
    // And once it exists it is found where it actually landed, suffix and all.
    assert_eq!(
        store::existing_file_in(&dir, "Storm Detail v2"),
        Some(second_path)
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A name that ends in a rendering suffix is refused, in words that say what
/// to do about it, and nothing is written.
///
/// This is a refusal in place of a silent, total loss. Such a name saved
/// perfectly well: the file was correct, the round-trip check passed, and the
/// palette was gone at the next launch, because what the application stores as
/// the installed palette's identity is `base_name()` - the half WITHOUT the
/// suffix - so the restore looked for "Storm" while the file declared
/// "Storm (stepped)" and installed the shipped default instead. Reopening it
/// renamed it for the same reason. And the picker's own rows are printed in
/// exactly this form, so it is the convention the UI teaches.
#[test]
fn a_name_that_ends_in_a_rendering_suffix_is_refused_before_anything_is_written() {
    let dir = scratch_dir("suffix-name");
    for suffix in [
        " (stepped)",
        " (continuous)",
        " (interpolated)",
        " (quantized stepped)",
    ] {
        let mut table = sample_table();
        table.name = format!("Storm{suffix}");
        let path = store::free_path_in(&dir, &table.name);
        let error = store::save(&table, &path).expect_err("the name is refused");
        let said = error.to_string();
        assert!(
            said.contains(suffix.trim_start()),
            "the refusal must name the ending it is refusing: {said}"
        );
        assert!(
            said.contains("Storm") && said.contains("restart"),
            "the refusal must say what to do and why: {said}"
        );
        assert!(!path.exists(), "a refused save wrote {}", path.display());
    }
    // Nothing at all landed in the directory.
    assert_eq!(std::fs::read_dir(&dir).expect("listing").count(), 0);

    // And the same name with anything after the ending is an ordinary name.
    let mut table = sample_table();
    table.name = "Storm (stepped) v2".to_owned();
    let path = store::free_path_in(&dir, &table.name);
    store::save(&table, &path).expect("a name that merely contains one saves");
    assert_eq!(
        store::load(&path).expect("loads").name,
        "Storm (stepped) v2"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A name this build already ships a palette under is refused, in words that
/// say which measurement ships it and what to do about it, and nothing is
/// written.
///
/// The same silent, total loss the rendering-suffix refusal above exists to
/// prevent, arriving by a different road - and the road is a short one. Copy
/// on a preset row pre-fills that preset's name with " copy" on the end, so
/// "my version of AWIPS Wilson REF" is five deleted characters away from a
/// name that cannot work. It saved perfectly: the file was correct, the
/// round-trip check passed, no other file held the name. Then the restore
/// path, which searches the shipped catalogue BEFORE the analyst's directory
/// so that a stray file cannot replace a documented preset, put the SHIPPED
/// table back on the pane at the next launch; and the picker row for that name
/// offered Edit on the preset, so the analyst's own file could never be
/// reopened either. Perfect file, palette gone, nothing said.
#[test]
fn a_name_this_build_already_ships_is_refused_before_anything_is_written() {
    let dir = scratch_dir("shipped-name");
    let shipped = color_tables::builtin_reflectivity_table();
    let base = shipped.base_name().to_owned();

    // Three forms of the one name, all of which the application writes down
    // somewhere: the stored form, the picker's row label, and the row label
    // for the other rendering.
    let mut forms = vec![base.clone()];
    for rendering in [TableRendering::Smooth, TableRendering::Stepped] {
        forms.push(shipped.rendered(rendering).name().to_owned());
    }
    for form in &forms {
        let mut table = sample_table();
        table.name = form.clone();
        let path = store::free_path_in(&dir, &table.name);
        let error = store::save(&table, &path).expect_err("the name is refused");
        let said = error.to_string();
        assert!(
            said.contains(&base),
            "the refusal must name the palette it is refusing: {said}"
        );
        assert!(
            said.contains("Reflectivity") && said.contains("this build ships"),
            "the refusal must say which measurement ships it: {said}"
        );
        assert!(
            said.contains("name of its own"),
            "the refusal must say what to do about it: {said}"
        );
        assert!(!path.exists(), "a refused save wrote {}", path.display());
    }
    // Every family, not only the one being saved into - the Measurement
    // control moves a table between families after the fact.
    let mut table = sample_table();
    table.name = color_tables::builtin_velocity_table()
        .base_name()
        .to_owned();
    let path = store::free_path_in(&dir, &table.name);
    let error = store::save(&table, &path).expect_err("another family's name is refused too");
    assert!(
        error.to_string().contains("Velocity"),
        "the refusal must name the family that ships it: {error}"
    );
    assert!(!path.exists());

    // Nothing at all landed in the directory.
    assert_eq!(std::fs::read_dir(&dir).expect("listing").count(), 0);

    // And a name of the analyst's own - including the one Copy pre-fills -
    // saves.
    for free in [format!("{base} copy"), format!("{base}, night")] {
        let mut table = sample_table();
        table.name = free.clone();
        let path = store::free_path_in(&dir, &table.name);
        store::save(&table, &path).expect("a name of its own saves");
        assert_eq!(store::load(&path).expect("loads").name, free);
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// A directory that ALREADY holds a file declaring a shipped palette's name -
/// hand-copied in, or written by a build before the refusal above - resolves
/// the same way on every path, and the analyst's file is left alone.
///
/// The shipped preset wins the name: that is deliberate and pinned on the
/// settings side by `a_user_file_never_shadows_a_shipped_preset`, because a
/// stray file must not quietly replace a palette the rest of the build
/// documents by name. The half that matters here is what happens to the file.
/// Nothing does. It is not renamed, not moved and not overwritten - the editor
/// refuses to write under that name at all, so there is no path on which the
/// analyst's bytes are the ones that get replaced.
///
/// The file is unreachable from the UI while it carries that name, and this
/// build has nowhere to say so: there is no faults or notices surface in the
/// window for a palette directory. This test records the limitation without
/// inventing a second reporting surface.
#[test]
fn a_hand_made_file_named_after_a_preset_loses_the_name_and_keeps_its_bytes() {
    let dir = scratch_dir("shipped-shadow");
    let shipped = color_tables::builtin_reflectivity_table();
    let base = shipped.base_name().to_owned();
    // Written through the raw text, because the editor now refuses to create
    // this - which is the point: only a hand-made file can be in this state.
    let text = format!(
        "Name: {base}\nProduct: BR\nMode: continuous\n\
         Color4: -10 11 199 77 255\nColor4: 60 200 210 220 255\n"
    );
    let path = dir.join("mine.pal");
    std::fs::write(&path, &text).expect("write");

    // The launch installs the SHIPPED palette, not the file.
    let restored = crate::settings_ui::palettes::resolve_choice_in(
        &dir,
        ColorTableFamily::Reflectivity,
        &settings::PaletteChoice {
            name: base.clone(),
            rendering: "smooth".to_owned(),
            generation: 2,
            ..Default::default()
        },
        None,
    );
    assert_eq!(restored.base_name(), base);
    assert_eq!(
        restored.stops(),
        shipped.stops(),
        "the hand-made file replaced the shipped preset"
    );
    // Stable, not merely right once.
    for _ in 0..5 {
        assert_eq!(
            crate::settings_ui::palettes::resolve_choice_in(
                &dir,
                ColorTableFamily::Reflectivity,
                &settings::PaletteChoice {
                    name: base.clone(),
                    rendering: "smooth".to_owned(),
                    generation: 2,
                    ..Default::default()
                },
                None,
            )
            .stops(),
            shipped.stops()
        );
    }

    // The row for that name is a shipped row, so the editor opens a COPY and
    // adopts no file - it cannot reach the analyst's, and it cannot overwrite
    // it either. `is_builtin_table` is what the settings page and the picker
    // both ask, so this is the answer the real rows get.
    let duplicate =
        color_tables::is_builtin_table(ColorTableFamily::Reflectivity, shipped.base_name());
    assert!(duplicate, "the row for a shipped name is a shipped row");
    let mut state = PaletteEditorState::default();
    state.set_directory(dir.clone());
    state.edit_or_duplicate(ColorTableFamily::Reflectivity, &shipped, duplicate);
    assert_eq!(state.file(), None, "the copy adopted a file");

    // And a save of that copy under the name it was pre-filled with does not
    // go near the analyst's file.
    let copy = state.table().expect("open").clone();
    assert_eq!(copy.name, format!("{base} copy"));
    let copy_path = store::free_path_in(&dir, &copy.name);
    store::save(&copy, &copy_path).expect("the copy's own name saves");
    assert_ne!(copy_path, path);

    // The analyst's file is byte for byte what it was.
    assert_eq!(
        std::fs::read_to_string(&path).expect("still there"),
        text,
        "the analyst's file was rewritten"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Two files in one directory cannot declare one name.
///
/// The `Name:` row is what every part of this build finds a palette by, so two
/// files answering to one name means the application resolves it to one of
/// them and the analyst reaches the other: Edit on the second row opened the
/// first row's file, and saving from there overwrote a palette that was then
/// unreachable from the UI.
#[test]
fn a_save_that_would_take_another_files_name_is_refused() {
    let dir = scratch_dir("name-taken");
    let mut first = sample_table();
    first.name = "Bench REF".to_owned();
    let first_path = store::free_path_in(&dir, &first.name);
    store::save(&first, &first_path).expect("the first one saves");

    // A different table, renamed onto the first one's name. Its own file, so
    // this is not a self-collision.
    let mut second = sample_table();
    second.name = "Bench REF".to_owned();
    let second_path = dir.join("bench-ref-2.pal");
    let error = store::save(&second, &second_path).expect_err("the name is taken");
    let said = error.to_string();
    assert!(
        said.contains("bench-ref.pal") && said.contains("name of its own"),
        "the refusal must name the file that holds it: {said}"
    );
    assert!(!second_path.exists());

    // Saving the FIRST table again is not a collision with itself.
    store::save(&first, &first_path).expect("a second save of the same file");
    // And a name of its own saves.
    second.name = "Bench REF, night".to_owned();
    store::save(&second, &second_path).expect("a free name saves");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A directory that already holds two files of one name - copied in by hand,
/// or written by a build before the refusal above - resolves to the SAME file
/// on every path there is.
///
/// The editor and the restore path used to carry a search each. Each was
/// internally deterministic and they disagreed: the editor tried the filename
/// the name reduces to first, the restore path walked the sorted directory, so
/// Edit opened one palette and the next launch installed the other. There is
/// now one function under both.
#[test]
fn one_name_in_two_files_resolves_to_the_same_file_on_every_path() {
    let dir = scratch_dir("two-files-one-name");
    // Two palettes with one name and unmistakably different colours. Written
    // through the raw text, because the editor now refuses to create this.
    let pal = |red: u8| {
        format!(
            "Name: Bravo\nProduct: BV\nMode: continuous\n\
             Color4: -40 {red} 0 0 255\nColor4: 40 200 210 220 255\n"
        )
    };
    // The file at the obvious path for the name is NOT the one sorted order
    // picks, which is exactly the case the two searches disagreed on.
    std::fs::write(dir.join("bravo.pal"), pal(11)).expect("write");
    std::fs::write(dir.join("alpha.pal"), pal(222)).expect("write");

    let editor_opens = store::existing_file_in(&dir, "Bravo").expect("the editor finds one");
    let shared =
        color_tables::palette_named_in(&dir, "Bravo").expect("the shared search finds one");
    assert_eq!(editor_opens, shared.path);

    let restored = crate::settings_ui::palettes::resolve_choice_in(
        &dir,
        ColorTableFamily::Velocity,
        &settings::PaletteChoice {
            name: "Bravo".to_owned(),
            rendering: "smooth".to_owned(),
            generation: 2,
            ..Default::default()
        },
        None,
    );
    assert_eq!(
        restored.stops()[0].color,
        store::load(&editor_opens).expect("loads").stops()[0].color,
        "the launch installed a different file than the editor edits"
    );
    // Stable across runs, not merely equal to each other this once.
    for _ in 0..5 {
        assert_eq!(
            store::existing_file_in(&dir, "Bravo"),
            Some(shared.path.clone())
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// The whole colour table suite on one real file, in the order an analyst
/// meets it: a shared `.pal` is imported into the folder, the picker's Edit
/// opens it in the editor, the editor's own Save button writes it, the folder
/// is rescanned, and the stored palette name resolves back to the same table.
///
/// One file throughout. That is the assertion this test exists for: three
/// features touch this folder - the scanner that offers its rows, the editor
/// that writes into it, and the launch-time restore that reads a stored name
/// out of it - and each of them used to answer "which file is this palette"
/// its own way. When those answers disagree the failure is silent and total:
/// Save writes a second file, the picker offers two rows of one name, and the
/// next launch installs whichever one it found first.
///
/// The input is the sample palette this build ships in `docs/palettes`,
/// written in the two-colour ramp form `.pal` files are actually traded in, so
/// the end colours have to survive every hop as well.
#[test]
fn a_shared_ramp_pair_file_imports_opens_saves_rescans_and_resolves_as_one_table() {
    let dir = scratch_dir("suite-end-to-end");
    let source = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("docs")
        .join("palettes")
        .join("Sample Ramp-Pair Velocity.pal");
    assert!(source.is_file(), "{} is missing", source.display());

    // 1. Imported the way a drop imports one: the file is copied in and read.
    let mut library = color_tables::user::UserTableLibrary::open(&dir);
    let outcome = library.import(&source);
    assert!(
        outcome.is_loaded(),
        "import said: {}",
        outcome.status_line()
    );
    assert_eq!(library.tables().len(), 1, "one file, one table");
    let imported_path = library.tables()[0].path().to_owned();
    let imported = library.tables()[0].table().clone();
    assert_eq!(
        library.tables()[0].display_name(),
        "Sample Ramp-Pair Velocity"
    );
    assert_eq!(library.tables()[0].family(), ColorTableFamily::Velocity);
    let ramp_pairs = imported
        .stops()
        .iter()
        .filter(|stop| stop.end_color.is_some())
        .count();
    assert_eq!(
        ramp_pairs,
        imported.stops().len(),
        "every row of this file is a ramp pair, and the import must keep them"
    );

    // 2. Opened through the picker's entry point - not a duplicate, because
    //    the catalogue does not ship this name - and it claims the file it was
    //    imported into rather than starting a new one.
    let mut bench = Bench::opened_for_editing(ColorTableFamily::Velocity, &imported, dir.clone());
    assert_eq!(
        bench.state.file(),
        Some(imported_path.as_path()),
        "the editor opened a different file than the one the picker's row came from"
    );

    // 3. Saved through the window's own Save button.
    let saved = bench
        .press("Save")
        .saved
        .expect("the Save button wrote a file");
    assert_eq!(
        saved, imported_path,
        "the save made a second file instead of writing the one it opened"
    );
    let files: Vec<_> = std::fs::read_dir(&dir)
        .expect("listing")
        .filter_map(Result::ok)
        .map(|entry| entry.file_name())
        .collect();
    assert_eq!(
        files.len(),
        1,
        "the folder holds more than one file: {files:?}"
    );

    // 4. Rescanned, the way the application rescans after a save.
    library.reread();
    assert_eq!(library.tables().len(), 1);
    assert_eq!(
        library.tables()[0].display_name(),
        "Sample Ramp-Pair Velocity",
        "the saved file answers to the name it was imported under"
    );
    assert_eq!(library.tables()[0].path(), imported_path);

    // 5. Resolved by name, the way a launch resolves a stored choice.
    let restored = crate::settings_ui::palettes::resolve_choice_in(
        &dir,
        ColorTableFamily::Velocity,
        &settings::PaletteChoice {
            name: "Sample Ramp-Pair Velocity".to_owned(),
            rendering: "smooth".to_owned(),
            generation: 2,
            ..Default::default()
        },
        Some(&library),
    );
    assert_eq!(restored.base_name(), "Sample Ramp-Pair Velocity");
    assert_eq!(
        restored.stops().len(),
        imported.stops().len(),
        "a stop was lost between the import and the restore"
    );
    for (after, before) in restored.stops().iter().zip(imported.stops().iter()) {
        assert_eq!(
            (after.value, after.color, after.end_color),
            (before.value, before.color, before.end_color),
            "a stop moved, changed colour or lost its ramp end"
        );
    }
    assert_eq!(restored.range_folded_rgba(), imported.range_folded_rgba());
    // And it paints what it painted on the way in.
    let first = imported.stops()[0].value;
    let last = imported.stops()[imported.stops().len() - 1].value;
    for step in 0..=2_000 {
        let value = first + (last - first) * (step as f32) / 2_000.0;
        assert_eq!(
            restored.sample(value),
            imported.sample(value),
            "the round trip repainted {value}"
        );
    }

    // And the shared search agrees with the folder scan about which file that
    // name is, which is the property the whole suite rests on.
    assert_eq!(
        color_tables::palette_named_in(&dir, "Sample Ramp-Pair Velocity")
            .expect("the shared search finds it")
            .path,
        imported_path
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// The user directory is beside the settings file, which is the contract the
/// rest of the application scans against.
#[test]
fn user_tables_live_beside_the_settings_file() {
    let dir = store::user_colortables_dir();
    assert_eq!(
        dir.file_name().and_then(|name| name.to_str()),
        Some("colortables")
    );
    assert_eq!(dir.parent(), Some(settings::app_config_root().as_path()));
}

// --- the window ------------------------------------------------------------

/// Drive the real window one frame at a time against a bare context, the way
/// the picker's and the application's own tests do.
struct Bench {
    context: egui::Context,
    state: PaletteEditorState,
}

impl Bench {
    /// Opened the way an Edit on a shipped preset row opens it.
    fn opened_on(family: ColorTableFamily, table: &ColorTable) -> Self {
        Self::opened(family, table, true, scratch_dir("window"))
    }

    /// Opened the way an Edit on a palette this build does not ship opens it,
    /// against a directory of the caller's choosing.
    fn opened_for_editing(
        family: ColorTableFamily,
        table: &ColorTable,
        directory: std::path::PathBuf,
    ) -> Self {
        Self::opened(family, table, false, directory)
    }

    fn opened(
        family: ColorTableFamily,
        table: &ColorTable,
        duplicate: bool,
        directory: std::path::PathBuf,
    ) -> Self {
        let mut state = PaletteEditorState::default();
        // Never the analyst's own directory: these tests read it, and two of
        // them write into it.
        state.set_directory(directory);
        state.edit_or_duplicate(family, table, duplicate);
        let mut bench = Self {
            context: egui::Context::default(),
            state,
        };
        // An auto-sized `egui::Window` spends its first pass measuring, and a
        // measuring pass paints nothing and hit-tests against a rect that is
        // not where the window will be. Two warm-up frames put the layout
        // where it settles, which is the state a test is about.
        bench.idle();
        bench.idle();
        bench
    }

    fn opened_on_source_field(id: radar_core::ProductId, table: &ColorTable) -> Self {
        let mut state = PaletteEditorState::default();
        state.set_directory(scratch_dir("source-field-window"));
        state.edit_source_field(id, table, true, false);
        let mut bench = Self {
            context: egui::Context::default(),
            state,
        };
        bench.idle();
        bench.idle();
        bench
    }

    fn frame(&mut self, events: Vec<egui::Event>) -> egui::FullOutput {
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1400.0, 1100.0),
            )),
            events,
            ..Default::default()
        };
        let state = &mut self.state;
        // `run_ui` and `ui.ctx()`, not the deprecated `Context::run`: a window
        // needs a context, and the root `Ui` is the supported way to reach one.
        self.context.run_ui(raw, |ui| {
            draw_palette_editor(
                ui.ctx(),
                PaletteEditorInput {
                    state,
                    volume: None,
                },
            );
        })
    }

    fn idle(&mut self) -> egui::FullOutput {
        self.frame(Vec::new())
    }

    /// One frame, keeping what the window reported rather than only what it
    /// painted. [`Self::frame`] throws the outcome away, which is right for
    /// the tests that assert on state; a test that presses Save has to see the
    /// path that came back out.
    fn frame_outcome(&mut self, events: Vec<egui::Event>) -> PaletteEditorOutcome {
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1400.0, 1100.0),
            )),
            events,
            ..Default::default()
        };
        let state = &mut self.state;
        let mut captured = PaletteEditorOutcome::default();
        let _ = self.context.run_ui(raw, |ui| {
            captured = draw_palette_editor(
                ui.ctx(),
                PaletteEditorInput {
                    state,
                    volume: None,
                },
            );
        });
        captured
    }

    /// Press the button carrying this label, through the pointer, and report
    /// what the window did about it.
    ///
    /// Found by where its label was painted rather than by an id, because the
    /// footer's buttons are laid out rather than addressed - which is also
    /// what makes this a real press: the label has to be on screen and
    /// enabled for the click to land on anything.
    fn press(&mut self, label: &str) -> PaletteEditorOutcome {
        let rect = self
            .painted_boxes()
            .into_iter()
            .find(|(text, _)| text == label)
            .unwrap_or_else(|| panic!("{label:?} is not on the window"))
            .1;
        let mut outcome = PaletteEditorOutcome::default();
        for events in click_events(rect.center()) {
            outcome = self.frame_outcome(events);
        }
        outcome
    }

    fn painted(&mut self) -> Vec<String> {
        let output = self.idle();
        let mut painted = Vec::new();
        for clipped in &output.shapes {
            collect_text(&clipped.shape, &mut painted);
        }
        painted
    }

    fn rect_of(&self, id: egui::Id) -> Option<egui::Rect> {
        self.context.read_response(id).map(|response| response.rect)
    }

    /// Every painted string with the rectangle it was painted in.
    ///
    /// Where a caption landed is the only evidence there is that it is over
    /// the column it names: no widget owns it, so nothing about the state
    /// says whether it points at the right control.
    /// Every square of a transparency checkerboard the frame painted, with the
    /// colour it was painted in.
    ///
    /// The checkerboard is the only thing in the window that paints whole
    /// `CHECKER`-sided squares - the ramp columns are a point and a half wide
    /// by the strip's full height - so the size is the filter.
    fn painted_checker_squares(&mut self) -> Vec<egui::Color32> {
        let output = self.idle();
        let mut rects = Vec::new();
        for clipped in &output.shapes {
            collect_rect_fills(&clipped.shape, &mut rects);
        }
        let side = super::ui::CHECKER;
        rects
            .into_iter()
            .filter(|(rect, _)| {
                (rect.width() - side).abs() < 0.01 && (rect.height() - side).abs() < 0.01
            })
            .map(|(_, fill)| fill)
            .collect()
    }

    fn painted_boxes(&mut self) -> Vec<(String, egui::Rect)> {
        let output = self.idle();
        let mut painted = Vec::new();
        for clipped in &output.shapes {
            collect_text_boxes(&clipped.shape, &mut painted);
        }
        painted
    }
}

#[test]
fn a_source_field_editor_applies_only_to_its_exact_product_id() {
    let id = crate::source_fields::product_id("V1");
    let table = crate::palettes::source_field_table(
        "V1",
        -23.07,
        23.07,
        &color_tables::ColorTableSet::default(),
    );
    let mut bench = Bench::opened_on_source_field(id.clone(), &table);
    assert_eq!(bench.state.source_target(), Some(&id));
    let painted = bench.painted();
    assert!(
        painted.iter().any(|line| line == "Source field")
            && painted.iter().any(|line| line == "V1")
            && painted
                .iter()
                .any(|line| line.contains("raw producer numbers")),
        "the exact target and raw-value rule were not visible: {painted:?}"
    );

    let outcome = bench.press("Apply only to V1");
    assert!(outcome.install.is_none());
    let applied = outcome.source_install.expect("source-specific apply");
    assert_eq!(applied.id, id);
    assert_eq!(applied.table, table);
    assert!(!applied.durable, "an unsaved Apply is session-only");
    assert!(
        bench.state.status().expect("Apply status").0.contains(
            "Session-only preview applied to V1 · undo: CUSTOM → Reset to observed range"
        )
    );
}

#[test]
fn source_save_then_apply_marks_the_clean_file_backed_binding_durable() {
    let id = crate::source_fields::product_id("V1");
    let table = crate::palettes::source_field_table(
        "V1",
        -23.07,
        23.07,
        &color_tables::ColorTableSet::default(),
    );
    let mut bench = Bench::opened_on_source_field(id.clone(), &table);

    let saved = bench.press("Save");
    assert!(saved.saved.is_some());
    assert!(saved.source_saved.is_some());
    let applied = bench
        .press("Apply only to V1")
        .source_install
        .expect("source-specific apply");
    assert_eq!(applied.id, id);
    assert_eq!(applied.table, table);
    assert!(applied.durable, "a clean saved table should persist");
}

#[test]
fn source_apply_then_save_promotes_only_that_matching_session_preview() {
    let id = crate::source_fields::product_id("V1");
    let table = crate::palettes::source_field_table(
        "V1",
        -23.07,
        23.07,
        &color_tables::ColorTableSet::default(),
    );
    let mut bench = Bench::opened_on_source_field(id.clone(), &table);
    let applied = bench
        .press("Apply only to V1")
        .source_install
        .expect("source-specific apply");
    assert!(!applied.durable);

    let mut bindings = crate::source_field_palettes::SourceFieldPaletteOverrides::default();
    assert!(bindings.apply_session(applied.id, applied.table));
    assert!(bindings.capture().is_empty());

    let saved = bench.press("Save");
    let (saved_id, saved_table) = saved.source_saved.expect("source save identity");
    assert_eq!(saved_id, id);
    assert!(bindings.promote_matching_saved(&saved_id, &saved_table));
    assert!(bindings.capture().contains_key(&id.0));
}

#[test]
fn automatic_or_session_source_edit_does_not_reopen_a_stale_same_named_palette() {
    let directory = scratch_dir("source-reset-stays-reset");
    std::fs::write(
        directory.join("source-field-v1.pal"),
        "Name: Source field V1\nProduct: Generic\nMode: continuous\nColor4: -100 255 0 0 255\nColor4: 100 0 0 255 255\n",
    )
    .expect("stale saved override");
    let automatic = crate::palettes::source_field_table(
        "V1",
        -23.07,
        23.07,
        &color_tables::ColorTableSet::default(),
    );
    let mut state = PaletteEditorState::default();
    state.set_directory(directory.clone());

    state.edit_source_field(
        crate::source_fields::product_id("V1"),
        &automatic,
        true,
        false,
    );

    let opened = state
        .table()
        .expect("automatic source table")
        .to_color_table()
        .expect("builds");
    assert_eq!(opened.stops().first().expect("first").value, -23.07);
    assert_eq!(opened.stops().last().expect("last").value, 23.07);
    assert!(
        state.file().is_none(),
        "Reset must not adopt the stale file"
    );

    let session = crate::palettes::source_field_table(
        "V1",
        -12.0,
        12.0,
        &color_tables::ColorTableSet::default(),
    );
    state.edit_source_field(
        crate::source_fields::product_id("V1"),
        &session,
        false,
        false,
    );
    let opened = state
        .table()
        .expect("session source table")
        .to_color_table()
        .expect("builds");
    assert_eq!(opened.stops().first().expect("first").value, -12.0);
    assert_eq!(opened.stops().last().expect("last").value, 12.0);
    assert!(
        state.file().is_none(),
        "a session preview must not be replaced by the older file"
    );
    let _ = std::fs::remove_dir_all(directory);
}

fn collect_text(shape: &egui::Shape, into: &mut Vec<String>) {
    match shape {
        egui::Shape::Text(text) => into.push(text.galley.text().to_owned()),
        egui::Shape::Vec(shapes) => {
            for shape in shapes {
                collect_text(shape, into);
            }
        }
        _ => {}
    }
}

fn collect_rect_fills(shape: &egui::Shape, into: &mut Vec<(egui::Rect, egui::Color32)>) {
    match shape {
        egui::Shape::Rect(rect) => into.push((rect.rect, rect.fill)),
        egui::Shape::Vec(shapes) => {
            for shape in shapes {
                collect_rect_fills(shape, into);
            }
        }
        _ => {}
    }
}

fn collect_text_boxes(shape: &egui::Shape, into: &mut Vec<(String, egui::Rect)>) {
    match shape {
        egui::Shape::Text(text) => into.push((
            text.galley.text().to_owned(),
            egui::Rect::from_min_size(text.pos, text.galley.size()),
        )),
        egui::Shape::Vec(shapes) => {
            for shape in shapes {
                collect_text_boxes(shape, into);
            }
        }
        _ => {}
    }
}

/// One press and release at a point, as two frames' worth of events.
fn click_events(at: egui::Pos2) -> Vec<Vec<egui::Event>> {
    vec![
        vec![
            egui::Event::PointerMoved(at),
            egui::Event::PointerButton {
                pos: at,
                button: egui::PointerButton::Primary,
                pressed: true,
                modifiers: egui::Modifiers::NONE,
            },
        ],
        vec![egui::Event::PointerButton {
            pos: at,
            button: egui::PointerButton::Primary,
            pressed: false,
            modifiers: egui::Modifiers::NONE,
        }],
    ]
}

fn drag_events(from: egui::Pos2, to: egui::Pos2) -> Vec<Vec<egui::Event>> {
    vec![
        vec![
            egui::Event::PointerMoved(from),
            egui::Event::PointerButton {
                pos: from,
                button: egui::PointerButton::Primary,
                pressed: true,
                modifiers: egui::Modifiers::NONE,
            },
        ],
        vec![egui::Event::PointerMoved(to)],
        vec![
            egui::Event::PointerMoved(to),
            egui::Event::PointerButton {
                pos: to,
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: egui::Modifiers::NONE,
            },
        ],
    ]
}

/// Opening on a shipped preset duplicates it. The catalogue is never at risk,
/// and the window says so rather than leaving it to be discovered on save.
#[test]
fn opening_a_shipped_preset_duplicates_it_instead_of_editing_it() {
    let installed = color_tables::builtin_reflectivity_table();
    let mut bench = Bench::opened_on(ColorTableFamily::Reflectivity, &installed);
    let table = bench.state.table().expect("a table is open");
    assert_eq!(table.name, format!("{} copy", installed.base_name()));
    assert!(bench.state.file().is_none(), "a duplicate has no file yet");
    let painted = bench.painted();
    assert!(
        painted
            .iter()
            .any(|line| line.contains("never overwritten")),
        "the window must say why this is a copy: {painted:?}"
    );
}

/// The header rows are all on screen, in the app's own language rather than
/// as bare ids.
#[test]
fn the_window_offers_the_whole_header() {
    let mut bench = Bench::opened_on(
        ColorTableFamily::Velocity,
        &color_tables::builtin_velocity_table(),
    );
    let painted = bench.painted();
    for expected in [
        "Name",
        "Measurement",
        "Units",
        "Scale",
        "Sampling",
        "Step",
        "Range folded",
        "Stops",
        "Preview",
        "Add stop",
        "Save",
    ] {
        assert!(
            painted.iter().any(|line| line == expected),
            "{expected:?} is not on the window: {painted:?}"
        );
    }
    // The units-versus-scale distinction is stated, not left to be guessed.
    assert!(
        painted
            .iter()
            .any(|line| line.contains("CONVERT") && line.contains("REINTERPRET")),
        "the difference between the two controls must be explicit"
    );
}

/// Dragging a strip handle changes that stop's value and nothing else's.
#[test]
fn dragging_a_strip_handle_moves_its_own_stop() {
    let mut bench = Bench::opened_on(
        ColorTableFamily::Reflectivity,
        &color_tables::builtin_reflectivity_table(),
    );
    bench.idle();
    let stops: Vec<_> = bench
        .state
        .table()
        .expect("open")
        .stops()
        .iter()
        .map(|stop| (stop.id, stop.value))
        .collect();
    // A stop in the middle of the ramp, so there is room to move it both ways.
    let (id, before) = stops[stops.len() / 2];
    let handle = egui::Id::new(("palette-editor-handle", id));
    let rect = bench.rect_of(handle).expect("the handle was drawn");
    let from = rect.center();
    let to = egui::pos2(from.x + 60.0, from.y);
    for events in drag_events(from, to) {
        bench.frame(events);
    }
    let table = bench.state.table().expect("open");
    let after = table.stop(id).expect("the stop is still there").value;
    assert!(
        after > before,
        "dragging right must raise the value: {before} -> {after}"
    );
    for (other, was) in stops {
        if other == id {
            continue;
        }
        assert_eq!(
            table.stop(other).expect("still there").value,
            was,
            "a drag moved a stop it was not holding"
        );
    }
}

/// With no volume cached the preview says so in words. An empty box would read
/// as a broken preview.
#[test]
fn the_preview_explains_itself_when_there_is_no_volume() {
    let mut bench = Bench::opened_on(
        ColorTableFamily::Reflectivity,
        &color_tables::builtin_reflectivity_table(),
    );
    let painted = bench.painted();
    assert!(
        painted
            .iter()
            .any(|line| line.contains("No volume is loaded")),
        "the preview panel must explain itself: {painted:?}"
    );
}

/// The strip and the preview both read one built table, so they cannot
/// disagree, and it follows an edit.
#[test]
fn the_window_rebuilds_the_table_after_an_edit() {
    let mut bench = Bench::opened_on(
        ColorTableFamily::Reflectivity,
        &color_tables::builtin_reflectivity_table(),
    );
    bench.idle();
    let before = bench
        .state
        .table()
        .expect("open")
        .to_color_table()
        .expect("builds")
        .signature();
    let id = bench.state.table().expect("open").stops()[1].id;
    let handle = egui::Id::new(("palette-editor-handle", id));
    let rect = bench.rect_of(handle).expect("drawn");
    for events in drag_events(
        rect.center(),
        egui::pos2(rect.center().x + 40.0, rect.center().y),
    ) {
        bench.frame(events);
    }
    let after = bench
        .state
        .table()
        .expect("open")
        .to_color_table()
        .expect("builds")
        .signature();
    assert_ne!(
        before, after,
        "moving a stop must change what the table paints"
    );
}

/// A two-stop table this build does not ship, for the edit-versus-duplicate
/// tests. Parsed rather than assembled, so it arrives the way any other table
/// does.
fn unshipped_table(name: &str) -> ColorTable {
    ColorTable::parse(
        name,
        "Product: BR\nMode: continuous\nColor4: -10 10 20 30 255\nColor4: 60 200 210 220 255\n",
    )
    .expect("builds")
}

/// The picker computes edit-versus-duplicate and the editor obeys it.
///
/// A palette this build does not ship - reachable today by pressing Apply on
/// an edited table without saving it, then pressing Edit on that row - opened
/// as "… copy" because the editor re-derived the answer from whether a file of
/// that name happened to exist. The picker's own
/// `a_palette_this_build_does_not_ship_opens_for_editing_rather_than_copying`
/// asserts the contract; this is the other half of it.
#[test]
fn a_palette_this_build_does_not_ship_opens_under_its_own_name() {
    let dir = scratch_dir("edit-unshipped");
    let table = unshipped_table("My Own Table");
    let bench = Bench::opened_for_editing(ColorTableFamily::Reflectivity, &table, dir.clone());
    assert_eq!(bench.state.table().expect("open").name, "My Own Table");
    assert!(
        bench.state.file().is_none(),
        "a table with no file yet must not claim one"
    );
    let (status, failed) = bench
        .state
        .status()
        .expect("the window says where it stands");
    assert!(!failed);
    assert!(
        status.contains("no file yet"),
        "the footer must say the table is unsaved, not that it is a copy: {status}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Copy on a shipped preset copies the preset, whatever is in the directory.
///
/// An analyst's own palette whose name reduces to the same file stem as a
/// preset used to be opened instead - in edit-in-place mode, from a button
/// whose hover promised a copy - because the editor looked for a file before
/// it looked at the answer it had been given.
#[test]
fn copying_a_shipped_preset_never_opens_an_analysts_file() {
    let dir = scratch_dir("copy-preset");
    let preset = color_tables::builtin_reflectivity_table();
    let mut decoy = EditorTable::from_color_table(
        ColorTableFamily::Reflectivity,
        &unshipped_table(preset.base_name()),
    );
    decoy.name = preset.base_name().to_owned();
    let decoy_path = store::free_path_in(&dir, &decoy.name);
    // The bytes `save` would write, written round it: the editor now refuses
    // to CREATE a file declaring a shipped palette's name, and a directory
    // that already holds one is exactly the state this test is about.
    std::fs::write(&decoy_path, decoy.pal_text()).expect("the decoy is on disk");

    // The fixture has to actually collide, or the test proves nothing: opened
    // for editing, this preset's name finds the analyst's file.
    let colliding = Bench::opened_for_editing(ColorTableFamily::Reflectivity, &preset, dir.clone());
    assert_eq!(colliding.state.file(), Some(decoy_path.as_path()));

    let mut state = PaletteEditorState::default();
    state.set_directory(dir.clone());
    state.edit_or_duplicate(ColorTableFamily::Reflectivity, &preset, true);
    let opened = state.table().expect("open");
    assert_eq!(opened.name, format!("{} copy", preset.base_name()));
    assert_eq!(
        opened.stops().len(),
        preset.stops().len(),
        "Copy opened something other than the preset"
    );
    assert!(state.file().is_none(), "a copy must claim no file");
    // And the analyst's file is untouched.
    assert_eq!(store::load(&decoy_path).expect("loads").stops().len(), 2);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Copy twice on one preset makes two palettes, not two files claiming one
/// name.
///
/// " copy" used to be appended unconditionally, so the second copy declared
/// the same `Name:` row as the first. Both files were then found by one name,
/// which the rest of the build resolves to a single file: pressing Edit on the
/// second row opened the FIRST row's table, and saving from there overwrote it.
#[test]
fn copying_a_preset_twice_makes_two_names_and_not_one_name_twice() {
    let dir = scratch_dir("copy-twice");
    let preset = color_tables::builtin_reflectivity_table();

    let mut saved = Vec::new();
    for _ in 0..3 {
        let mut state = PaletteEditorState::default();
        state.set_directory(dir.clone());
        state.edit_or_duplicate(ColorTableFamily::Reflectivity, &preset, true);
        let table = state.table().expect("open").clone();
        let path = store::free_path_in(&dir, &table.pal_name());
        store::save(&table, &path).expect("each copy saves");
        saved.push((table.pal_name(), path));
    }

    let names: Vec<&str> = saved.iter().map(|(name, _)| name.as_str()).collect();
    assert_eq!(
        names,
        vec![
            format!("{} copy", preset.base_name()),
            format!("{} copy 2", preset.base_name()),
            format!("{} copy 3", preset.base_name()),
        ],
    );
    // Every file answers to its own name, and to nobody else's.
    for (name, path) in &saved {
        assert_eq!(store::existing_file_in(&dir, name).as_ref(), Some(path));
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// A `.pal` with no `Product:` row is edited under the family it was installed
/// in, not under the catch-all.
///
/// The edit-in-place arm threw the caller's family away: `pal::from_pal_text`
/// derives the family from the `Product:` row alone and falls back to Generic,
/// so a hand-dropped palette installed on the reflectivity pane opened with
/// the footer offering "Apply to Other", and pressing it moved the table to a
/// different family with nothing on screen having said so. The duplicate arm
/// was always right, because `from_color_table` takes the caller's family as
/// its fallback.
#[test]
fn a_file_with_no_product_row_keeps_the_family_it_was_opened_from() {
    let dir = scratch_dir("no-product");
    std::fs::write(
        dir.join("field-table.pal"),
        "Name: Field Table\nMode: smooth\nColor4: 5 10 20 30 255\nColor4: 50 200 210 220 255\n",
    )
    .expect("write");
    let installed = ColorTable::parse(
        "Field Table",
        "Mode: smooth\nColor4: 5 10 20 30 255\nColor4: 50 200 210 220 255\n",
    )
    .expect("builds");

    let bench = Bench::opened_for_editing(ColorTableFamily::Reflectivity, &installed, dir.clone());
    assert_eq!(
        bench.state.file(),
        Some(dir.join("field-table.pal").as_path()),
        "the file was not adopted, so this proves nothing about the family"
    );
    assert_eq!(
        bench.state.table().expect("open").family,
        ColorTableFamily::Reflectivity,
        "the caller's family must be the fallback when the file names none"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Edit on a file whose `Name:` row ends in a rendering suffix opens THAT
/// file, under the name it declares.
///
/// The editor cannot write such a name any more, but a palette copied into the
/// directory by hand can carry one, and this is the half that used to lose it:
/// the file was looked for under `base_name()` - the name with the suffix
/// taken off - so it was never found, the footer said "this table has no file
/// yet", the name on screen was silently the shortened one, and the next Save
/// wrote a SECOND file while the original sat there orphaned. The full name is
/// tried first now, and what comes back is the file's own name.
#[test]
fn edit_opens_a_suffixed_file_under_its_own_name_instead_of_renaming_it() {
    let dir = scratch_dir("suffixed-file");
    let text = "Name: Hand Dropped (stepped)\nProduct: BR\nMode: stepped\n\
                Color4: 5 10 20 30 255\nColor4: 50 200 210 220 255\n";
    let path = dir.join("hand-dropped-stepped.pal");
    std::fs::write(&path, text).expect("write");
    let installed = ColorTable::parse("Hand Dropped (stepped)", text).expect("builds");

    let bench = Bench::opened_for_editing(ColorTableFamily::Reflectivity, &installed, dir.clone());
    assert_eq!(
        bench.state.file(),
        Some(path.as_path()),
        "Edit did not open the file the row came from"
    );
    let table = bench.state.table().expect("open");
    assert_eq!(
        table.name, "Hand Dropped (stepped)",
        "Edit renamed the analyst's palette"
    );
    // Saving it is refused - loudly, and without writing a second file.
    assert!(store::save(table, &path).is_err());
    let files: Vec<String> = std::fs::read_dir(&dir)
        .expect("listing")
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(files, vec!["hand-dropped-stepped.pal".to_owned()]);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A name ending in a rendering suffix is called out while it is still on
/// screen, not only when Save refuses it.
#[test]
fn the_window_says_a_reserved_name_will_not_save() {
    let dir = scratch_dir("suffix-window");
    let mut bench = Bench::opened_for_editing(
        ColorTableFamily::Reflectivity,
        &unshipped_table("Field REF"),
        dir.clone(),
    );
    assert!(
        !bench
            .painted()
            .iter()
            .any(|line| line.contains("reserved for the stepped")),
        "an ordinary name must not be warned about"
    );
    if let Some(table) = bench.state.table_mut() {
        table.name = "Field REF (stepped)".to_owned();
    }
    let painted = bench.painted();
    assert!(
        painted
            .iter()
            .any(|line| line.contains("(stepped)") && line.contains("will not save")),
        "the window must say the name will not save: {painted:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A shipped palette's name is called out at the field it is typed in, not
/// only when Save refuses it - and it is the thing said about a name that
/// trips both checks, because it is the one that survives taking the suffix
/// off.
#[test]
fn the_window_says_a_shipped_name_will_not_save() {
    let dir = scratch_dir("shipped-window");
    let mut bench = Bench::opened_for_editing(
        ColorTableFamily::Reflectivity,
        &unshipped_table("Field REF"),
        dir.clone(),
    );
    assert!(
        !bench
            .painted()
            .iter()
            .any(|line| line.contains("this build ships")),
        "an ordinary name must not be warned about"
    );

    let base = color_tables::builtin_reflectivity_table()
        .base_name()
        .to_owned();
    if let Some(table) = bench.state.table_mut() {
        table.name = base.clone();
    }
    let painted = bench.painted();
    assert!(
        painted.iter().any(|line| line.contains(&base)
            && line.contains("this build ships")
            && line.contains("will not save")),
        "the window must say the name will not save: {painted:?}"
    );

    // The row label form, which trips the suffix check too. One warning, and
    // it is the one that is still true after the suffix comes off.
    if let Some(table) = bench.state.table_mut() {
        table.name = format!("{base} (stepped)");
    }
    let painted = bench.painted();
    assert!(
        painted.iter().any(|line| line.contains("this build ships")),
        "the deeper of the two refusals must be the one said: {painted:?}"
    );
    assert!(
        !painted
            .iter()
            .any(|line| line.contains("reserved for the stepped")),
        "one name must not be warned about twice: {painted:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Leaving the Name field cleans it up, so what is on screen is what the file
/// can carry.
///
/// Not while it is being typed in - trimming under the cursor would make a
/// space impossible to type in the middle of a name - and not silently on
/// save either: the field itself moves, where it can be seen.
#[test]
fn leaving_the_name_field_trims_what_the_file_would_trim() {
    let dir = scratch_dir("name-focus");
    let mut bench = Bench::opened_for_editing(
        ColorTableFamily::Reflectivity,
        &unshipped_table("Field REF"),
        dir.clone(),
    );
    let field = bench
        .rect_of(egui::Id::new("palette-editor-name"))
        .expect("the name field was drawn");
    for events in click_events(field.center()) {
        bench.frame(events);
    }
    // Typed, not assigned: what the field holds while the caret is in it.
    if let Some(table) = bench.state.table_mut() {
        table.name = " Storm\u{a0}Detail ".to_owned();
    }
    bench.idle();
    assert_eq!(
        bench.state.table().expect("open").name,
        " Storm\u{a0}Detail ",
        "the name must be left alone while the field has the caret"
    );

    // Clicking anywhere else takes the caret out of the field.
    let elsewhere = egui::pos2(field.left(), field.bottom() + 90.0);
    for events in click_events(elsewhere) {
        bench.frame(events);
    }
    assert_eq!(bench.state.table().expect("open").name, "Storm Detail");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Every stop-list caption sits over the column it names.
///
/// The captions used to be five `allocate_ui` calls with hand-written widths,
/// and `allocate_ui` states a maximum: each caption shrank to its own text and
/// they closed up against the left edge, so in the window photographs "Colour"
/// sat over the value field, "Ramp to" over the colour swatch and "Add / cut"
/// over the ramp checkbox, while the + and x buttons had no caption at all. A
/// caption over the wrong control is an instruction to press the wrong thing,
/// and nothing pinned the header, so the source comment claiming the widths
/// tracked the row could go on saying so.
///
/// Measured on the painted shapes: the caption row's own boxes give the column
/// boundaries, and every control in the first stop row has to fall inside the
/// column whose caption names it.
#[test]
fn every_stop_list_caption_sits_over_the_column_it_names() {
    let dir = scratch_dir("captions");
    let mut bench = Bench::opened_for_editing(
        ColorTableFamily::Reflectivity,
        &unshipped_table("Field REF"),
        dir.clone(),
    );
    let painted = bench.painted_boxes();
    let single = |text: &str| -> egui::Rect {
        let found: Vec<egui::Rect> = painted
            .iter()
            .filter(|(painted, _)| painted == text)
            .map(|(_, rect)| *rect)
            .collect();
        assert_eq!(found.len(), 1, "{text:?} was painted {} times", found.len());
        found[0]
    };
    let captions: Vec<egui::Rect> = ["#", "Value", "Colour", "Pair", "Ramp to", "Add", "Cut"]
        .into_iter()
        .map(single)
        .collect();
    let header_y = captions[0].center().y;
    for (index, caption) in captions.iter().enumerate() {
        assert!(
            (caption.center().y - header_y).abs() < 2.0,
            "caption {index} is not on the caption row: {caption:?}"
        );
    }
    // Each caption starts at its column's left edge, so the captions bound the
    // columns: column i runs from its own caption's left to the next one's.
    let spacing = bench.context.global_style().spacing.item_spacing.x;
    let column = |index: usize| -> (f32, f32) {
        let left = captions[index].left();
        let right = match captions.get(index + 1) {
            Some(next) => next.left() - spacing,
            None => {
                left + captions[index]
                    .width()
                    .max(crate::theme::bevel::MIN_TOUCH_POINTS)
            }
        };
        (left - 1.0, right + 1.0)
    };
    for index in 0..captions.len() - 1 {
        let (left, right) = column(index);
        assert!(right > left, "column {index} has no width");
    }

    // The first stop row: everything on it that paints text has to be under
    // the right caption. `-10` is the first stop of the fixture, the "-" is
    // the placeholder in the ramp-target column of a row that is not a pair.
    let row_top = painted
        .iter()
        .filter(|(text, rect)| text == "+" && rect.top() > captions[0].bottom())
        .map(|(_, rect)| rect.top())
        .fold(f32::INFINITY, f32::min);
    assert!(row_top.is_finite(), "no stop row was painted");
    let in_first_row = |text: &str| -> egui::Rect {
        painted
            .iter()
            .filter(|(painted, rect)| painted == text && (rect.top() - row_top).abs() < 12.0)
            .map(|(_, rect)| *rect)
            .next()
            .unwrap_or_else(|| panic!("{text:?} is not on the first stop row"))
    };
    for (column_index, text) in [(0usize, "1"), (4, "-"), (5, "+"), (6, "×")] {
        let control = in_first_row(text);
        let (left, right) = column(column_index);
        assert!(
            control.center().x >= left && control.center().x <= right,
            "{text:?} sits at {:.1}, outside the {:?} column {left:.1}..{right:.1}",
            control.center().x,
            ["#", "Value", "Colour", "Pair", "Ramp to", "Add", "Cut"][column_index],
        );
    }
    // The value field prints its own number, whatever the formatting: find the
    // one text on the row that parses as the first stop's value.
    let value = painted
        .iter()
        .filter(|(text, rect)| {
            (rect.top() - row_top).abs() < 12.0
                && text
                    .trim_end_matches(" dBZ")
                    .parse::<f32>()
                    .is_ok_and(|number| (number + 10.0).abs() < 0.01)
        })
        .map(|(_, rect)| *rect)
        .next()
        .expect("the value field printed its number");
    let (left, right) = column(1);
    assert!(
        value.center().x >= left && value.center().x <= right,
        "the value field sits at {:.1}, outside the Value column {left:.1}..{right:.1}",
        value.center().x
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// The live preview draws the echo on a dark ground in BOTH theme variants.
///
/// It used to take `palette.well`, which in the light variant is near-paper
/// (250, 249, 245). A palette's whitest bands - AWIPS Wilson's 60 dBZ stop is
/// pure white - were then a hole in the echo, in the one panel whose caption
/// promises "this palette, on the volume on screen".
#[test]
fn the_preview_ground_is_dark_in_both_theme_variants() {
    let (dark, light) = super::ui::preview_ground();
    for square in [dark, light] {
        let luminance = 0.2126 * f32::from(square.r()) / 255.0
            + 0.7152 * f32::from(square.g()) / 255.0
            + 0.0722 * f32::from(square.b()) / 255.0;
        assert!(
            luminance < 0.2,
            "{square:?} is not a ground a white echo band shows up on"
        );
        // Neutral: an echo is judged against sky, not against a tint.
        let channels = [square.r(), square.g(), square.b()];
        let spread = channels.iter().max().expect("three channels")
            - channels.iter().min().expect("three channels");
        assert!(spread <= 8, "{square:?} is a tint, not a neutral");
    }
    assert_ne!(dark, light, "a checkerboard needs two squares");
    // And it is NOT the light variant's well, which is what it used to be.
    assert_ne!(dark, crate::theme::palette::LIGHT.well);
    assert_ne!(light, crate::theme::palette::LIGHT.well);
}

/// The gradient strip stands on that same ground, in both variants.
///
/// The strip and the preview are two pictures of one table, and for a while
/// they disagreed: the preview was moved to the dark checkerboard and the
/// strip was left on the current variant's, so in the light theme one alpha-0
/// stop read three ways inside one window - near-white checks under the strip,
/// dark checks in the row's own colour swatch, dark checks in the preview. The
/// strip is the control an analyst DRAGS that stop on, and `Palette::light`'s
/// well and pressed face are both paper (250,249,245 and 198,195,188), so a
/// pure-white 60 dBZ band vanished on the one control that has to show it.
///
/// Read off the shapes the real window painted, in both variants, rather than
/// off the source: the visible result is what matters.
#[test]
fn the_strip_stands_on_the_same_ground_as_the_preview_in_both_theme_variants() {
    let (dark, light) = super::ui::preview_ground();
    let expected: std::collections::BTreeSet<(u8, u8, u8, u8)> = [dark, light]
        .into_iter()
        .map(|colour| colour.to_tuple())
        .collect();
    for variant in ["dark", "light"] {
        let dir = scratch_dir("strip-ground");
        // A table whose lowest stop is transparent, which is what the ground
        // is there for. `sample_table`'s first stop is alpha 0.
        let mut bench = Bench::opened_for_editing(
            ColorTableFamily::Reflectivity,
            &sample_table().to_color_table().expect("builds"),
            dir.clone(),
        );
        crate::theme::apply(&bench.context, &crate::theme::Appearance::by_id(variant));
        bench.idle();
        bench.idle();

        let squares: std::collections::BTreeSet<(u8, u8, u8, u8)> = bench
            .painted_checker_squares()
            .into_iter()
            .map(|colour| colour.to_tuple())
            .collect();
        assert_eq!(
            squares, expected,
            "in the {variant:?} variant the strip's ground is {squares:?}, not the \
             preview's {expected:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}

/// A table whose stops have collapsed onto one value keeps its drag handles.
///
/// The strip used to return before the handles were laid out whenever the
/// table would not build, which took the handles away at the exact moment the
/// only way out was to pull two stops apart with them.
#[test]
fn a_collapsed_table_keeps_its_drag_handles() {
    let dir = scratch_dir("collapsed-window");
    let mut bench = Bench::opened_for_editing(
        ColorTableFamily::Reflectivity,
        &unshipped_table("Collapsed"),
        dir.clone(),
    );
    let ids: Vec<_> = {
        let table = bench.state.table_mut().expect("open");
        let ids: Vec<_> = table.stops().iter().map(|stop| stop.id).collect();
        for id in &ids {
            table.set_value(*id, 95.0);
        }
        ids
    };
    bench.idle();
    bench.idle();
    assert!(
        bench.state.table().expect("open").to_color_table().is_err(),
        "the state under test is a table that will not build"
    );

    let mut rects = Vec::new();
    for id in &ids {
        let rect = bench
            .rect_of(egui::Id::new(("palette-editor-handle", id)))
            .unwrap_or_else(|| panic!("the handle for {id:?} was not drawn"));
        assert!(
            rect.width() >= crate::theme::bevel::MIN_TOUCH_POINTS,
            "a handle smaller than a fingertip: {rect:?}"
        );
        rects.push(rect);
    }

    // Dragging one of the stacked handles pulls a stop off the pile, whichever
    // of the two the pointer lands on, and the table builds again.
    let from = rects[0].center();
    for events in drag_events(from, egui::pos2(from.x + 120.0, from.y)) {
        bench.frame(events);
    }
    let table = bench.state.table().expect("open");
    let values: Vec<f32> = table.stops().iter().map(|stop| stop.value).collect();
    assert!(
        values[0] != values[1],
        "a drag on the collapsed pile moved nothing: {values:?}"
    );
    table.to_color_table().expect("the table builds again");
    let _ = std::fs::remove_dir_all(&dir);
}
