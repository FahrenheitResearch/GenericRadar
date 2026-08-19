//! The reflectivity opacity ramp's own test suite.
//!
//! A sibling module rather than part of `advanced.rs`'s `mod tests`, following
//! `pane_canvas/chrome_tests.rs`: this is one coherent argument - the shape of
//! the transfer function, the domain it is normalised into, the fields it must
//! refuse to grade, and the compositing property it has to preserve - and it is
//! longer than the code it checks. `use super::*` reaches `advanced`'s private
//! `opacity_ramp` mirror and `ramp_applies_to_structure` exactly as an inline
//! `mod tests` would.

use super::*;

/// The shipped WGSL, composed exactly as `vol3d` composes it. A copy of the
/// two-line helper in `advanced`'s own `mod tests`, because a sibling test
/// module cannot see that module's private items.
fn composed() -> String {
    compose_shader(super::super::SHADER)
}

/// Engine range of the reflectivity box, as `product_engine` declares it.
/// The ramp is a statement about dBZ, so the tests normalise through this.
const DBZ_RANGE: (f32, f32) = (-32.0, 94.5);

fn k_at(params: &AdvancedParams, dbz: f32) -> f32 {
    params.extinction_multiplier(dbz, DBZ_RANGE.0, DBZ_RANGE.1)
}

#[test]
fn the_opacity_ramp_is_a_bounded_monotone_function_of_reflectivity() {
    // What makes it a transfer function rather than a trick: more
    // reflectivity is never less opaque, and the curve stays between the
    // floor and the gain.
    let params = AdvancedParams::default();
    let bounds = params.opacity_ramp_floor..=params.opacity_ramp_gain;
    let mut previous = 0.0_f32;
    for step in -640_i16..=1890 {
        let dbz = f32::from(step) * 0.05;
        let value = k_at(&params, dbz);
        assert!(value >= previous - 1e-6, "fell at {dbz} dBZ: {value}");
        assert!(bounds.contains(&value), "out of bounds at {dbz}: {value}");
        previous = value;
    }
    // The knees mean what they say.
    assert!((k_at(&params, 0.0) - params.opacity_ramp_floor).abs() < 1.0e-6);
    assert!((k_at(&params, 5.0) - params.opacity_ramp_floor).abs() < 1.0e-6);
    let gain = params.opacity_ramp_gain;
    assert!((k_at(&params, 60.0) - gain).abs() < 1.0e-6);
    assert!((k_at(&params, 75.0) - gain).abs() < 1.0e-6);
    // The opacity slider has to go on meaning something in the middle of
    // the ramp, or the whole volume simply gets more transparent than it
    // was and nothing gains any body.
    assert!(k_at(&params, 48.0) > 1.0, "nothing reaches full opacity");
    assert!(k_at(&params, 40.0) < 1.0, "everything is at full opacity");
    // The complaint, made checkable: a 55 dBZ core must absorb far more
    // than 25 dBZ stratiform, and weak echo must not be erased either.
    let ratio = k_at(&params, 55.0) / k_at(&params, 25.0);
    assert!(ratio > 8.0, "a core absorbs only {ratio:.2}x weak echo");
    assert!(k_at(&params, 25.0) > 0.02, "weak echo was flattened");
    // The A/B handle: before this ramp existed every admitted sample
    // absorbed the same amount, and equal floor and gain reproduce that.
    // It is what lets the proof example photograph a before and an after
    // through one shader instead of two.
    let flat = AdvancedParams {
        opacity_ramp_floor: 1.0,
        opacity_ramp_gain: 1.0,
        ..Default::default()
    };
    for dbz in [-30.0_f32, 20.0, 60.0, 90.0] {
        assert!((k_at(&flat, dbz) - 1.0).abs() < 1.0e-6, "{dbz} dBZ");
    }
}

#[test]
fn the_default_ramp_tracks_the_marshall_palmer_extinction_law() {
    // Z = 200 R^1.6 (Marshall & Palmer 1948, J. Meteor. 5(4), 165-166)
    // with extinction going as R^0.65 (Atlas 1953, J. Meteor. 10(6),
    // 486-488) gives sigma proportional to Z^0.40625, or 10^(0.040625 dBZ).
    // Between the knees the ramp is supposed to BE that curve, which is
    // what makes the default exponent a fit rather than a taste.
    let params = AdvancedParams::default();
    // Both sides are normalised at the high knee, so what is compared is
    // SHAPE. `opacity_ramp_gain` is a display scale and cannot be asked to
    // agree with a physical extinction coefficient.
    let top = k_at(&params, params.opacity_ramp_high_dbz);
    let reference = |dbz: f32| 10.0_f32.powf(0.040_625 * (dbz - params.opacity_ramp_high_dbz));
    let (mut worst, mut at) = (0.0_f32, 0.0_f32);
    for step in 20..=58 {
        let dbz = step as f32;
        let modelled = reference(dbz);
        let error = ((k_at(&params, dbz) / top - modelled) / modelled).abs();
        if error > worst {
            (worst, at) = (error, dbz);
        }
    }
    assert!(
        worst < 0.14,
        "{:.0}% off the law at {at} dBZ",
        worst * 100.0
    );
    println!(
        "ramp vs Marshall-Palmer + Atlas: worst {:.1}% at {at} dBZ",
        worst * 100.0
    );
}

#[test]
fn the_ramp_reaches_the_shader_in_the_structure_domain() {
    // Same requirement as `iso_value`: the shader compares the knees to a
    // `t_volume` sample, so they normalise against the STRUCTURE range.
    // The velocity range would put the low knee near 34 dBZ instead of 5.
    let params = AdvancedParams::default();
    let slot = |name: &str| {
        ADVANCED_UNIFORM_FIELDS
            .iter()
            .position(|field| *field == name)
            .expect("field is declared")
    };
    let packed = params.shader_uniforms(DBZ_RANGE.0, DBZ_RANGE.1, true);
    let span = DBZ_RANGE.1 - DBZ_RANGE.0;
    let low = (params.opacity_ramp_low_dbz - DBZ_RANGE.0) / span;
    let high = (params.opacity_ramp_high_dbz - DBZ_RANGE.0) / span;
    assert!((packed[slot("opacity_ramp_low")] - low).abs() < 1.0e-6);
    assert!((packed[slot("opacity_ramp_high")] - high).abs() < 1.0e-6);
    let gamma = packed[slot("opacity_ramp_gamma")];
    assert_eq!(gamma, params.opacity_ramp_gamma);
    assert_eq!(
        packed[slot("opacity_ramp_floor")],
        params.opacity_ramp_floor
    );
    assert_eq!(packed[slot("opacity_ramp_gain")], params.opacity_ramp_gain);
    // The mirror and the packed uniform have to describe one curve.
    let from_packed = |dbz: f32| {
        opacity_ramp(
            (dbz - DBZ_RANGE.0) / span,
            packed[slot("opacity_ramp_low")],
            packed[slot("opacity_ramp_high")],
            packed[slot("opacity_ramp_gamma")],
            packed[slot("opacity_ramp_floor")],
            packed[slot("opacity_ramp_gain")],
        )
    };
    for dbz in [10.0_f32, 35.0, 50.0, 62.0] {
        assert!(
            (from_packed(dbz) - k_at(&params, dbz)).abs() < 1e-6,
            "{dbz}"
        );
    }

    // `opacity_ramp` above is a MIRROR of the WGSL, and a mirror that is
    // only checked against its own expectations can drift from the thing it
    // reflects while every assertion stays green. These pin the shader to
    // the same five operations in the same order, so a change to either
    // side has to be made to both.
    let source = composed();
    for step in [
        "let fraction = clamp((structure - low) / (high - low), 0.0, 1.0);",
        "let shaped = pow(fraction, max(ua.opacity_ramp_gamma, 0.05));",
        "return ramp_floor + (gain - ramp_floor) * shaped;",
    ] {
        assert!(
            source.contains(step),
            "the shader ramp no longer does: {step}"
        );
    }
}

/// One ray through a uniform slab of length `span`, in `steps` samples of
/// a reference alpha 0.55. `on_tau` is the shader's corrected form, where
/// the weight multiplies the optical depth; otherwise it is the form that
/// it replaced, where the weight multiplied the finished alpha.
fn accumulate(steps: usize, weight: f32, span: f32, on_tau: bool) -> f32 {
    let dt = span / steps as f32;
    let mut accumulated = 0.0_f32;
    for _ in 0..steps {
        let exponent = dt * 28.0 * if on_tau { weight } else { 1.0 };
        let alpha = (1.0 - 0.45_f32.powf(exponent)) * if on_tau { 1.0 } else { weight };
        accumulated += (1.0 - accumulated) * alpha;
    }
    accumulated
}

#[test]
fn the_ramp_composites_identically_at_every_step_length() {
    // The property a plausible-looking change silently breaks. The
    // adaptive sampler varies the step by more than 4x, so a ramp that did
    // not sit on the optical depth would change the SAME storm's
    // brightness as the sampler changed rate. Transmittance is
    // multiplicative in tau (Max 1995, eq. 1-4), so the corrected form
    // cannot depend on how the ray was chopped up - and it does not.
    for ramp in [0.07_f32, 0.5, 1.0, 3.5] {
        let (coarse, fine) = (
            accumulate(24, ramp, 0.6, true),
            accumulate(384, ramp, 0.6, true),
        );
        assert!((coarse - fine).abs() < 1e-5, "{ramp}: {coarse} vs {fine}");
    }
    // And the shape it replaced genuinely was not invariant, which is what
    // makes this a correctness fix rather than a preference.
    let (coarse, fine) = (
        accumulate(24, 0.2, 0.6, false),
        accumulate(384, 0.2, 0.6, false),
    );
    let drift = (coarse - fine).abs();
    assert!(drift > 0.03, "expected drift, got {coarse} vs {fine}");
    println!(
        "post-multiplied alpha drifts {:.1} opacity points",
        drift * 100.0
    );

    // Everything above is a model of the two forms, and a model proves
    // nothing about the shipped shader on its own: moving the ramp back
    // onto the composited alpha in the WGSL leaves every assertion above
    // green while the picture changes brightness with the sampler's rate.
    // So the property is bound here to the text that has to carry it.
    // Measured on the GPU rather than argued, KUDX 2026-08-19T04:37Z at
    // 96 / 160 / 240 steps: mean frame alpha 0.418 / 0.416 / 0.414 with the
    // ramp and 0.438 / 0.436 / 0.435 without it.
    let source = composed();
    assert_eq!(
        source.matches("max(optical_scale * ramp, 0.0)").count(),
        2,
        "a march branch stopped putting the ramp on the optical depth"
    );
    for forbidden in ["ramp * (1.0", "alpha * ramp", "ramp * alpha"] {
        assert!(
            !source.contains(forbidden),
            "`{forbidden}` multiplies a composited alpha by the ramp"
        );
    }
}

#[test]
fn the_shipped_shader_puts_the_ramp_on_the_optical_depth() {
    // Text checks, because the arithmetic lives in WGSL and no unit test
    // can execute it. Each is a specific way an edit that still compiles
    // could re-break the feature.
    let source = composed();
    let present = |needle: &str, why: &str| assert!(source.contains(needle), "{why}");
    present(
        "fn opacity_ramp(structure: f32)",
        "the ramp function is gone",
    );
    // The ramp multiplies the exponent of the Beer-Lambert correction in
    // BOTH march branches, never the composited alpha.
    assert_eq!(
        source.matches("max(optical_scale * ramp, 0.0)").count(),
        2,
        "a march branch stopped scaling the optical depth by the ramp"
    );
    // The step correction uses the distance actually travelled.
    present(
        "select(base_dt, max(t - previous_t, 0.0), have_previous)",
        "the step correction lost the actual segment length",
    );
    // Support moved INTO the optical depth and must not also multiply the
    // finished alpha.
    assert!(
        !source.contains("alpha = alpha * support_scale"),
        "support is applied twice, or applied after the correction"
    );
    present("support_scale;", "the support weight is gone");
    // The two-box path reads the structure plane, not the colour plane, so
    // a fast empty gate cannot turn solid; slices carry the same ramp.
    present(
        "let ramp = opacity_ramp(structure);",
        "not the structure plane",
    );
    present(
        "max(opacity_ramp(structure), 0.0)",
        "the slice path lost the ramp",
    );
}

#[test]
fn the_reflectivity_structure_range_matches_the_product_catalog() {
    // The ramp gate is an identity check against a declared constant, so
    // the constant has to BE the declared one. If a future edit moves
    // `product_engine`'s reflectivity domain, this fails here rather than
    // silently turning the ramp off in the shipped app.
    let registry = product_engine::ProductRegistry::builtin();
    let reflectivity = registry
        .get("REF")
        .expect("the catalog declares reflectivity");
    let declared = reflectivity.domain.declared_engine_range;
    assert_eq!(
        (declared.min, declared.max),
        REFLECTIVITY_STRUCTURE_RANGE_DBZ
    );
    assert!(ramp_applies_to_structure(declared.min, declared.max));
}

#[test]
fn the_ramp_is_flat_for_every_product_that_is_not_reflectivity() {
    // The 3D explorer builds its box from whatever product the operator has
    // selected and normalises it against THAT product's declared range, so
    // a dBZ-shaped ramp reaches every field in the catalog. Walk the real
    // catalog, not a fixture, and require the ramp to be inert wherever the
    // structure field is not reflectivity.
    let params = AdvancedParams::default();
    let floor = slot_of("opacity_ramp_floor");
    let gain = slot_of("opacity_ramp_gain");
    let mut graded = Vec::new();
    for descriptor in product_engine::ProductRegistry::builtin().all() {
        let range = descriptor.domain.declared_engine_range;
        let packed = params.shader_uniforms(range.min, range.max, false);
        let is_reflectivity = (range.min, range.max) == REFLECTIVITY_STRUCTURE_RANGE_DBZ;
        if is_reflectivity {
            graded.push(descriptor.short_name);
            assert_eq!(
                packed[gain], params.opacity_ramp_gain,
                "{}",
                descriptor.id.0
            );
            continue;
        }
        assert_eq!(
            (packed[floor], packed[gain]),
            (1.0, 1.0),
            "{} ({} .. {}) is graded by a reflectivity ramp",
            descriptor.id.0,
            range.min,
            range.max
        );
    }
    assert!(
        graded.contains(&"REF"),
        "reflectivity itself lost the ramp: {graded:?}"
    );

    // The worst of the cases this protects, spelled out: velocity is signed
    // and near-symmetric about zero, so a ramp read off the raw value makes
    // outbound flow far more opaque than inbound flow of the same speed and
    // fades half of every couplet.
    let velocity = (-100.0_f32, 100.0_f32);
    let inbound = params.extinction_multiplier(-55.0, velocity.0, velocity.1);
    let outbound = params.extinction_multiplier(55.0, velocity.0, velocity.1);
    assert_eq!(
        inbound, outbound,
        "a {inbound} / {outbound} inbound-outbound opacity split"
    );
    assert_eq!(inbound, 1.0);

    // The degenerate corners, which reach this the moment a product
    // declares a collapsed or inverted domain: still finite, still flat,
    // never a division that escapes into the uniform block.
    for range in [
        (10.0_f32, 10.0_f32),
        (94.5, -32.0),
        (0.0, f32::MIN_POSITIVE),
    ] {
        let packed = params.shader_uniforms(range.0, range.1, false);
        assert!(packed.iter().all(|value| value.is_finite()), "{range:?}");
        assert_eq!((packed[floor], packed[gain]), (1.0, 1.0), "{range:?}");
    }
}

#[test]
fn the_shader_flattens_the_ramp_for_the_inspection_modes() {
    // Both beam-support presentations are reachable straight from the
    // render-mode and support-mode dropdowns, with no preset in between, so
    // the flattening has to live in the shader. `support_mode > 1.5` is
    // `Color by support`; `render_mode > 4.5` is `Beam-support inspection`.
    // They are the same two conditions that already switch the sample to
    // `support_color`.
    let source = composed();
    let gate = source
        .split_once("fn opacity_ramp(structure: f32)")
        .expect("the ramp function is declared")
        .1;
    let body = gate.split_once('}').expect("the guard closes").0;
    assert!(
        body.contains("ua.support_mode > 1.5 || ua.render_mode > 4.5"),
        "the inspection modes are graded by reflectivity: {body}"
    );
    assert!(body.contains("return 1.0;"), "the guard returns a ramp");
    // And the two display thresholds that exist to isolate the weak tail,
    // which is the part the ramp is built to push into haze. Measured on
    // KUDX 2026-08-19T04:37Z at `Below 20 dBZ`: graded, the volume painted
    // 0.00% of the frame where the flat ramp painted 1.38%. Two-box has no
    // threshold mode - its gate is `ref_gate` - so it stays graded.
    let guard = source
        .split_once("fn opacity_ramp(structure: f32)")
        .expect("the ramp function is declared")
        .1;
    let guards = guard.split_once("let low =").expect("the guards end").0;
    assert!(
        guards.contains("u.velocity_mode < 0.5 && u.threshold_mode > 0.5"),
        "Below and Outside are graded by the quantity they hide: {guards}"
    );

    // And the preset must NOT do it on the CPU instead: a preset flattening
    // would be missed by both dropdowns and would clobber the operator's
    // ramp on the way through.
    let mut params = AdvancedParams {
        opacity_ramp_gain: 5.5,
        ..Default::default()
    };
    params.apply_support_preset();
    assert_eq!(params.opacity_ramp_gain, 5.5);
    assert_eq!(
        params.opacity_ramp_floor,
        AdvancedParams::default().opacity_ramp_floor
    );
}

#[test]
fn the_slice_path_composites_an_alpha_that_cannot_leave_zero_to_one() {
    // The gain is above 1 on purpose, so a ramp multiplied onto a slice's
    // composited alpha passes 1 at a core; `1 - accumulated` then goes
    // negative and the next of the three planes SUBTRACTS. Measured on
    // KUDX 2026-08-19T04:37Z at a uniform ramp of 20 - strictly more
    // absorption everywhere than a uniform 1 - 0.37% of the frame came back
    // LESS opaque than the flat render, by up to 0.91 opacity points.
    let source = composed();
    assert!(
        !source.contains("* 0.72\n                    * opacity_ramp(structure);"),
        "the slice ramp is multiplied onto a composited alpha again"
    );
    assert!(
        source.contains("let plane_alpha = palette.a * u.opacity * transfer")
            && source.contains("max(1.0 - plane_alpha, 0.0001),"),
        "the slice path lost the bounded form"
    );

    // The arithmetic that form stands on, over the whole reachable domain:
    // bounded, monotone in the ramp, and the identity where the ramp is.
    for step in 0_u8..=40 {
        let plane_alpha = f32::from(step) / 40.0;
        let mut previous = 0.0_f32;
        for ramp in [0.0_f32, 0.07, 1.0, 3.5, 20.0] {
            let alpha = 1.0 - (1.0 - plane_alpha).max(0.0001).powf(ramp);
            assert!((0.0..=1.0).contains(&alpha), "{plane_alpha} at {ramp}");
            assert!(alpha >= previous - 1e-6, "{plane_alpha} fell at {ramp}");
            if (ramp - 1.0).abs() < 1e-6 {
                // 1e-4 rather than exact: `max(1 - a, 0.0001)` is the
                // shader's guard against `pow(0, 0)`, and it costs exactly
                // that much at a fully opaque plane.
                assert!((alpha - plane_alpha).abs() < 2e-4, "{plane_alpha}");
            }
            previous = alpha;
        }
    }
}

fn slot_of(name: &str) -> usize {
    ADVANCED_UNIFORM_FIELDS
        .iter()
        .position(|field| *field == name)
        .expect("field is declared")
}
