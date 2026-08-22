//! Which colour table draws which product.
//!
//! The four built-in colour families were built for the base moments, and
//! everything that did not fit fell through to a "generic" ramp spanning 0 to
//! 100. That is harmless for a probability and wrong for everything else: an
//! echo top runs to 21 000 metres and correlation coefficient lives between
//! 0.2 and 1.05, so both render as a single flat colour on a 0-100 ramp. The
//! visible symptom is a ZDR field that looks like an unlit blue-grey blob.
//!
//! So a product whose domain the built-in families do not cover gets a ramp
//! synthesised across its own declared range. The ramp is stretched to the
//! product, not the product squeezed onto the ramp.

use color_tables::{ColorStop, ColorTable, ColorTableFamily, ColorTableSet, Rgba8};
use product_engine::registry::{DerivedVolumeId, ProductDescriptor};
use render2d::color_family_for_moment;

/// A cool-to-hot sequence for quantities that only increase: liquid water,
/// hail size, echo height. Ordered dark to bright so the biggest values are
/// the ones that catch the eye.
const SEQUENTIAL_RAMP: [(u8, u8, u8); 8] = [
    (12, 18, 34),
    (26, 62, 120),
    (28, 122, 166),
    (46, 166, 128),
    (128, 186, 74),
    (226, 194, 62),
    (226, 118, 46),
    (216, 58, 58),
];

/// A ramp for probabilities, which people read in tenths rather than as a
/// continuum, so it stays legible in coarse bands.
const PROBABILITY_RAMP: [(u8, u8, u8); 6] = [
    (18, 24, 38),
    (34, 84, 128),
    (56, 148, 132),
    (198, 186, 66),
    (222, 118, 48),
    (214, 52, 52),
];

/// The colour table for one product.
///
/// Base moments keep the built-in families so nothing an analyst already knows
/// changes colour. Volume products get a ramp over their own domain, except
/// composite reflectivity, which is reflectivity and must look like it - a
/// composite that did not match the base product would be read as a different
/// quantity.
pub fn table_for(descriptor: &ProductDescriptor, tables: &ColorTableSet) -> ColorTable {
    match descriptor.computation.derived_volume() {
        None => tables
            .for_family(color_family_for_moment(
                &descriptor.computation.source_moment(),
            ))
            .clone(),
        Some(DerivedVolumeId::CompositeReflectivity) => {
            tables.for_family(color_families::REFLECTIVITY).clone()
        }
        Some(DerivedVolumeId::ProbabilityOfHail | DerivedVolumeId::ProbabilityOfSevereHail) => {
            ramp_over(descriptor, &PROBABILITY_RAMP)
        }
        Some(_) => ramp_over(descriptor, &SEQUENTIAL_RAMP),
    }
}

/// The explicitly generic table for one producer-native field.
///
/// Only the stop positions move: the source field's observed finite minimum
/// and maximum replace Generic's 0..100 authoring span. That makes every
/// finite value visible without claiming the field is reflectivity, velocity,
/// power, or any other scientific quantity. The producer's own unit token is
/// shown separately by the pane and is never read here.
pub fn source_field_table(
    producer_name: &str,
    minimum: f32,
    maximum: f32,
    tables: &ColorTableSet,
) -> ColorTable {
    source_field_table_from_template(
        producer_name,
        minimum,
        maximum,
        tables.for_family(ColorTableFamily::Generic),
    )
}

/// Stretch one chosen palette over raw producer values for one exact source
/// field. This is also the restart path for a persisted field override: the
/// named user table supplies the colours while the exact field entry supplies
/// the numeric range.
pub fn source_field_table_from_template(
    producer_name: &str,
    minimum: f32,
    maximum: f32,
    template: &ColorTable,
) -> ColorTable {
    let (minimum, maximum) = drawable_source_range(minimum, maximum);
    let first = template
        .stops()
        .first()
        .map(|stop| stop.value)
        .unwrap_or(0.0);
    let last = template
        .stops()
        .last()
        .map(|stop| stop.value)
        .unwrap_or(100.0);
    let span = (last - first).max(f32::EPSILON);
    let stops = template
        .stops()
        .iter()
        .map(|stop| {
            let fraction = (stop.value - first) / span;
            ColorStop {
                value: minimum + (maximum - minimum) * fraction,
                color: stop.color,
                end_color: stop.end_color,
            }
        })
        .collect();
    ColorTable::new(format!("Source field {producer_name}"), stops)
        .expect("the source template keeps at least two ascending stops")
        .rendered(template.rendering())
}

pub fn drawable_source_range(minimum: f32, maximum: f32) -> (f32, f32) {
    if minimum < maximum {
        return (minimum, maximum);
    }
    let padding = minimum.abs().mul_add(0.01, 0.0).max(0.5);
    (minimum - padding, maximum + padding)
}

mod color_families {
    use color_tables::ColorTableFamily;
    pub const REFLECTIVITY: ColorTableFamily = ColorTableFamily::Reflectivity;
}

/// Spread a colour sequence evenly across a product's declared range.
///
/// The first stop is fully transparent so the floor of the range - no liquid
/// water, no hail, zero probability - paints nothing rather than painting a
/// dark wash over the whole map. That transparency is also what the legend's
/// inked span reads to decide where the bar should start.
///
/// It is a floor marker and not a mask: the segment above it fades up into the
/// second colour, so the bottom seventh of a product's range - hail under
/// 14 mm, echo tops under 3 km - is drawn rather than blanked. That comes free
/// from a stop that declares no end colour, which is every stop here. Only a
/// clear *row* of GR `.pal` text holds its colour across its segment, and none
/// of these are parsed from text.
fn ramp_over(descriptor: &ProductDescriptor, colors: &[(u8, u8, u8)]) -> ColorTable {
    let range = descriptor.domain.declared_engine_range;
    let last = colors.len().saturating_sub(1).max(1) as f32;
    let stops: Vec<ColorStop> = colors
        .iter()
        .enumerate()
        .map(|(index, (red, green, blue))| {
            let fraction = index as f32 / last;
            ColorStop {
                value: range.min + (range.max - range.min) * fraction,
                color: if index == 0 {
                    Rgba8::new(*red, *green, *blue, 0)
                } else {
                    Rgba8::opaque(*red, *green, *blue)
                },
                // Every step runs to the next colour in the sequence, which is
                // what an undeclared end means.
                end_color: None,
            }
        })
        .collect();
    ColorTable::new(descriptor.short_name, stops)
        .expect("a synthesised ramp has at least two ascending finite stops")
}

#[cfg(test)]
mod tests {
    use super::*;
    use product_engine::ProductRegistry;

    fn descriptor(id: &str) -> &'static ProductDescriptor {
        ProductRegistry::builtin()
            .get(id)
            .expect("product must exist")
    }

    #[test]
    fn a_base_moment_keeps_the_colour_table_an_analyst_already_knows() {
        let tables = ColorTableSet::default();
        let reflectivity = table_for(descriptor("REF"), &tables);
        assert_eq!(
            reflectivity.name(),
            tables.for_family(color_families::REFLECTIVITY).name()
        );
    }

    #[test]
    fn composite_reflectivity_looks_like_reflectivity_because_it_is_reflectivity() {
        // A composite drawn on a different ramp would be read as a different
        // quantity, which is exactly the mistake it is meant to avoid.
        let tables = ColorTableSet::default();
        assert_eq!(
            table_for(descriptor("CREF"), &tables).name(),
            table_for(descriptor("REF"), &tables).name()
        );
    }

    #[test]
    fn an_echo_top_ramp_spans_metres_and_not_the_generic_zero_to_one_hundred() {
        // The defect this module exists for: on the generic ramp every echo
        // top above 100 m - which is all of them - clamps to one colour.
        let table = table_for(descriptor("ET18"), &ColorTableSet::default());
        let stops = table.stops();
        assert_eq!(stops.first().expect("stops exist").value, 0.0);
        assert_eq!(stops.last().expect("stops exist").value, 21_000.0);
    }

    #[test]
    fn a_hail_size_ramp_spans_millimetres() {
        let table = table_for(descriptor("MESH"), &ColorTableSet::default());
        assert_eq!(table.stops().last().expect("stops exist").value, 100.0);
    }

    #[test]
    fn a_probability_ramp_spans_nought_to_one_hundred_percent() {
        let table = table_for(descriptor("POH"), &ColorTableSet::default());
        assert_eq!(table.stops().first().expect("stops exist").value, 0.0);
        assert_eq!(table.stops().last().expect("stops exist").value, 100.0);
    }

    /// Every product `table_for` synthesises a ramp for, found through the
    /// registry rather than listed.
    ///
    /// A list of ids is a guard that only covers the products someone
    /// remembered to add to it, and a seventh derived product would join the
    /// build without joining the test. The condition here is `table_for`'s own:
    /// a derived volume that is not composite reflectivity gets `ramp_over`.
    fn every_synthesised_ramp() -> Vec<(String, ColorTable)> {
        let tables = ColorTableSet::default();
        let ramps: Vec<_> = ProductRegistry::builtin()
            .all()
            .iter()
            .filter(|descriptor| {
                !matches!(
                    descriptor.computation.derived_volume(),
                    None | Some(DerivedVolumeId::CompositeReflectivity)
                )
            })
            .map(|descriptor| (descriptor.id.0.clone(), table_for(descriptor, &tables)))
            .collect();
        assert!(
            ramps.len() >= 6,
            "the registry stopped offering the derived products these guards are about"
        );
        ramps
    }

    #[test]
    fn the_bottom_of_every_synthesised_ramp_paints_nothing() {
        // Zero hail and zero liquid water must not wash the whole map, and the
        // legend reads this transparency to decide where its bar starts.
        for (id, table) in every_synthesised_ramp() {
            assert_eq!(
                table.stops().first().expect("stops exist").color.a,
                0,
                "{id} paints its floor"
            );
        }
    }

    /// The floor marker is a fade, not a mask.
    ///
    /// The floor stop is a seventh or a fifth of the product's range below the
    /// first opaque one, so whether that segment fades up or holds clear is the
    /// difference between drawing hail under 14 mm and echo tops under 3 km and
    /// blanking them. It fades because the stop declares no end colour and a
    /// stop list has no dialect; the GR clear-row hold belongs to parsed `.pal`
    /// text and reaches these tables through nothing.
    ///
    /// Pinned per product rather than per ramp constant so a product that stops
    /// going through `ramp_over` is noticed here.
    #[test]
    fn a_synthesised_ramp_fades_up_out_of_its_clear_floor() {
        for (id, table) in every_synthesised_ramp() {
            let stops = table.stops();
            let floor = stops[0];
            let first_band = stops[1];
            assert_eq!(floor.end_color, None, "{id} floor declares a ramp target");

            let halfway = (floor.value + first_band.value) / 2.0;
            let painted = table.sample(halfway);
            assert!(
                painted.a > 0 && painted.a < 255,
                "{id} at {halfway} came back {painted:?} instead of part-way up the fade"
            );
        }
    }

    #[test]
    fn every_product_gets_a_table_whose_inked_span_overlaps_its_domain() {
        // If these ever stop overlapping the legend cannot be drawn at all,
        // and the pane would show a colour bar for a range nothing paints.
        let tables = ColorTableSet::default();
        for descriptor in ProductRegistry::builtin().all() {
            let table = table_for(descriptor, &tables);
            let inked = table
                .inked_value_span()
                .unwrap_or_else(|| panic!("{} has an entirely transparent table", descriptor.id.0));
            let domain = descriptor.domain.declared_engine_range;
            assert!(
                inked.0 <= domain.max && inked.1 >= domain.min,
                "{} inks {inked:?} but declares {domain:?}",
                descriptor.id.0
            );
        }
    }
}
