//! Round numbers for a legend's tick ladder.
//!
//! The failure this module exists to prevent is a colour bar labelled 13.7,
//! 27.4, 41.1, 54.8. Those are what "span divided by N" gives you for
//! reflectivity, and every one of them is correct. They are also useless: an
//! analyst reads a legend by finding the label nearest a colour and rounding in
//! their head, and there is nothing to round to when the labels are already
//! arbitrary. The ladder below gives 0, 20, 40, 60, 80 instead - fewer digits,
//! and every label a number a person already thinks in.
//!
//! The algorithm is Heckbert's, from Paul S. Heckbert, "Nice Numbers for Graph
//! Labels", in Andrew S. Glassner (ed.), Graphics Gems, Academic Press, 1990,
//! pp. 61-63 (ISBN 0-12-286165-5), with one deliberate change: Heckbert's
//! ladder is 1, 2, 5, 10 and ours adds 2.5. Without 2.5 a correlation
//! coefficient legend has to choose between a step of 0.2 and one of 0.5, and
//! 0.25 is the step that actually reads well on that bar. The richer
//! optimisation of Justin Talbot, Sharon Lin and Pat Hanrahan, "An Extension of
//! Wilkinson's Algorithm for Positioning Tick Labels on Axes", IEEE
//! Transactions on Visualization and Computer Graphics 16(6):1036-1043, 2010,
//! DOI 10.1109/TVCG.2010.130, scores simplicity, coverage, density and
//! legibility together; it is deliberately not used here because it needs a
//! search and this runs on the thread that paints.
//!
//! Everything in this module is in DISPLAY units and `f64`. It does not know
//! that knots exist, or kilofeet, or that a colour table is involved. The
//! caller converts first (see `crate::units::AffineTransform`) and hands over
//! plain numbers.

/// The mantissas a step may take, ascending.
///
/// `10.0` is the same number as `1.0` one decade up, and it is listed anyway:
/// when the ideal raw step is 9.4 the closest ladder entry in that decade is
/// the top of it, and leaving it out would force the choice down to 5 and
/// double the label count.
const LADDER: [f64; 5] = [1.0, 2.0, 2.5, 5.0, 10.0];

/// A ladder is not a ladder with one rung, so [`nice_ticks`] keeps stepping
/// down until it has at least this many ticks or runs out of descents.
const MIN_TICKS: usize = 2;

/// The most ticks a ladder may hold before [`nice_ticks`] gives up on the
/// range entirely.
///
/// 512 is far above anything legitimate. The largest honest ladder is about
/// 1.4 times `target_intervals`, and `target_intervals` is a `u8`: a sweep of
/// two million ranges spread over nineteen decades, each asked for 0, 6, 100
/// and 255 intervals, topped out at 340 ticks and never once tripped this. A
/// legend bar on a 4K display is around 2000 px and a label needs about 14 px
/// of height, so more than about 150 labels cannot be read anyway.
///
/// Exceeding it returns nothing rather than the first 512 rungs. A truncated
/// ladder is worse than no ladder: it stops partway across the bar and there is
/// nothing in the returned `Vec` to say so, so the caller draws a legend that
/// silently covers a fraction of the range it claims to.
const MAX_TICKS: usize = 512;

/// How many rungs [`nice_ticks`] may descend when the chosen step puts fewer
/// than [`MIN_TICKS`] multiples inside the range.
///
/// Four descents divide the step by ten, and a step no larger than a third of
/// the span always leaves two multiples inside it, so three descents is the
/// most that is ever needed from a step of at most about 1.35 times the span.
/// Eight is that with room to spare, and it is bounded so the loop cannot run
/// away on a step that has decayed into a subnormal.
const MAX_LADDER_DESCENTS: usize = 8;

/// Beyond this many decimal places `10^d` stops being an exact power of ten in
/// `f64` and snapping would move a tick instead of cleaning it, so
/// [`snap_to_decimals`] gives up and returns the raw product.
const MAX_SNAP_DECIMALS: i32 = 15;

/// The magnitude at and above which [`snap_to_decimals`] must not touch a tick.
///
/// 2^52 is where consecutive doubles are 1.0 apart, so every `f64` from here up
/// is already a whole number and `round` can only hand back what it was given.
/// The multiply and divide around that `round` are then not a no-op but pure
/// damage: at 1e15 a tick of 1000000000000002.5 scales to 10000000000000025,
/// which is not representable and lands on ...024, and dividing that by ten
/// gives 1000000000000002.375. The tick moved an eighth of a unit because a
/// cosmetic rounding was applied where there was nothing left to round.
const MAX_ROUNDABLE: f64 = 4_503_599_627_370_496.0;

/// How many spacings of neighbouring `f64`s a step must clear before a range
/// can be walked at all.
///
/// Near a magnitude M the representable doubles are about `M * f64::EPSILON`
/// apart. A step at or below that spacing has no ladder to give: consecutive
/// multiples round onto the same double, so the ticks come back as a column of
/// identical labels, and the `f64` index that walks them stops advancing at the
/// same magnitude, so the loop emitting them never ends. `nice_ticks(1e17,
/// 1e17 + 16.0, 6)` is exactly that range, and before this rule existed it
/// returned 512 copies of 1e17 - a legend with 512 labels stacked on one pixel
/// and a zero gap for a caller to divide by.
///
/// Four spacings, not one: [`snap_to_decimals`] multiplies and then divides,
/// and each of those roundings can move a tick by half a spacing, so a step of
/// exactly one spacing could still land two ticks on the same double. Four
/// leaves room for both roundings at both ends. Clearing it guarantees the
/// ticks are distinct and ascending; it does not make them perfectly even,
/// because within a few hundred spacings of the floor each tick still carries
/// up to half a spacing of rounding. That is why the even-spacing test states a
/// tolerance rather than exact equality.
const MIN_STEP_SPACINGS: f64 = 4.0;

/// One rung: a mantissa from [`LADDER`] and a power of ten.
///
/// Kept as (index, exponent) rather than as the bare `f64` because the number
/// of decimal places a tick needs is a fact about the rung, and recovering it
/// from the product afterwards means asking how many digits
/// 0.30000000000000004 has.
#[derive(Clone, Copy, Debug, PartialEq)]
struct LadderStep {
    mantissa_index: usize,
    exponent: i32,
}

impl LadderStep {
    fn value(self) -> f64 {
        LADDER[self.mantissa_index] * power_of_ten(self.exponent)
    }

    /// Decimal places needed to write this step, and therefore any multiple of
    /// it, exactly.
    ///
    /// A step of `2.5 * 10^e` is `25 * 10^(e-1)`, so it needs one place more
    /// than its exponent suggests; a step of `10 * 10^e` is `1 * 10^(e+1)` and
    /// needs one fewer. Getting this wrong does not move a tick, it only
    /// leaves 0.6000000000000001 on the label.
    fn decimals(self) -> i32 {
        let shift = match self.mantissa_index {
            2 => 1,
            4 => -1,
            _ => 0,
        };
        (-self.exponent + shift).max(0)
    }

    /// The next rung down.
    ///
    /// From the bottom of a decade the next rung is 5 in the decade below, not
    /// 10: `10 * 10^(e-1)` is the same number we started from, and descending
    /// onto it would make the caller's loop stop making progress.
    fn next_finer(self) -> Self {
        if self.mantissa_index == 0 {
            Self {
                mantissa_index: 3,
                exponent: self.exponent - 1,
            }
        } else {
            Self {
                mantissa_index: self.mantissa_index - 1,
                exponent: self.exponent,
            }
        }
    }
}

/// Choose round tick values spanning `[min, max]` in DISPLAY units.
///
/// Returns the multiples of a ladder step that lie inside the range, endpoints
/// included when they happen to be multiples. Every returned value satisfies
/// `min <= tick <= max` exactly - not to within a tolerance - and the result
/// ascends strictly. It is evenly spaced to the precision `f64` has at that
/// magnitude, which for any range a legend can draw means exactly.
///
/// The price of exact containment is one edge case: when a caller's bound is
/// itself a fraction of an ulp inside a round number, the tick on that round
/// number is genuinely outside the range asked for and is not returned. Giving
/// that tick back would mean testing tick indices against whole numbers with a
/// tolerance, and a tolerance loose enough to catch it also drags in ticks that
/// sit a real fraction of a step outside the range - see `ticks_for_step`.
///
/// Returns an empty vector - never a panic, never a partial ladder - when the
/// request is not answerable: `min > max` (a caller that built an inverted
/// range has a bug, and swapping the ends silently would hide it until someone
/// noticed the legend was upside down), `min == max` (no span, so no ladder),
/// either bound being NaN or infinite, a span so narrow for its magnitude that
/// `f64` cannot hold distinct ticks across it (see `MIN_STEP_SPACINGS`), or a
/// ladder longer than `MAX_TICKS` - 512, which no honest ladder reaches.
///
/// The step used can be finer than [`nice_step`] reports for the same span.
/// [`nice_step`] answers a question about a span; this answers a question about
/// a range, and a range can hold no multiple at all of the span's own step:
/// 0.01 to 0.99 contains no multiple of 1. When that happens this descends the
/// ladder until the range holds at least two ticks. A caller that needs the
/// step actually used should subtract two neighbouring ticks.
pub fn nice_ticks(min: f64, max: f64, target_intervals: u8) -> Vec<f64> {
    if !min.is_finite() || !max.is_finite() {
        return Vec::new();
    }
    if min >= max {
        return Vec::new();
    }
    let span = max - min;
    let Some(mut step) = choose_ladder_step(span, target_intervals) else {
        return Vec::new();
    };

    let mut ticks = ticks_for_step(min, max, step);
    let mut descents = 0;
    while ticks.len() < MIN_TICKS && descents < MAX_LADDER_DESCENTS {
        step = step.next_finer();
        let finer = step.value();
        if !finer.is_finite() || finer <= 0.0 {
            break;
        }
        ticks = ticks_for_step(min, max, step);
        descents += 1;
    }
    ticks
}

/// The rounded step a ladder would use for this span.
///
/// Returns 0.0 - not NaN, not a guess - when the span is zero, negative, or
/// non-finite. There is no round step for "no span", and a caller must check
/// for 0.0 before dividing by it or walking a loop with it.
///
/// `target_intervals` is a wish, not a promise: the answer is the ladder entry
/// whose interval count comes closest to it, which for a span of 200 and a
/// target of 5 is a step of 50 (four intervals) rather than anything ending in
/// a 7. A target of 0 is read as 1, since zero intervals is not a legend.
pub fn nice_step(span: f64, target_intervals: u8) -> f64 {
    match choose_ladder_step(span, target_intervals) {
        Some(step) => step.value(),
        None => 0.0,
    }
}

/// The rung whose interval count lands closest to the target.
fn choose_ladder_step(span: f64, target_intervals: u8) -> Option<LadderStep> {
    if !span.is_finite() || span <= 0.0 {
        return None;
    }
    // Zero intervals would divide by zero here and would mean nothing on a
    // bar, so the least a caller can be asking for is one.
    let target = f64::from(target_intervals.max(1));
    let raw = span / target;
    if !raw.is_finite() || raw <= 0.0 {
        return None;
    }
    let exponent = raw.log10().floor();
    if !exponent.is_finite() {
        return None;
    }
    let exponent = exponent as i32;

    let mut best: Option<(f64, LadderStep)> = None;
    for mantissa_index in 0..LADDER.len() {
        let candidate = LadderStep {
            mantissa_index,
            exponent,
        };
        let step = candidate.value();
        if !step.is_finite() || step <= 0.0 {
            continue;
        }
        let intervals = span / step;
        let score = (intervals - target).abs();
        // `<=`, not `<`. The ladder is ascending, so a tie hands the range to
        // the larger step. A span of 200 with a target of 6 can be cut into 8
        // intervals of 25 or 4 of 50, both two away from the wish; 50 wins. A
        // legend with one label fewer than ideal is still readable, and one
        // with labels crowding into each other is not.
        let better = match best {
            None => true,
            Some((best_score, _)) => score <= best_score,
        };
        if better {
            best = Some((score, candidate));
        }
    }
    best.map(|(_, step)| step)
}

/// Every multiple of `step` inside `[min, max]`, endpoints included.
///
/// Membership is decided by comparing the finished tick VALUE against the
/// caller's bounds, never by comparing the tick INDEX against a whole number,
/// and that is the whole design of this function. Testing the index needs a
/// tolerance, and no tolerance works at both ends of the problem: `-0.3 / 0.1`
/// is -2.9999999999999996, so a bare `ceil` throws away an endpoint the caller
/// did ask for, while a tolerance wide enough to recover it (1e-9 of the index,
/// say) also pulls `60.00000001 / 20 = 3.0000000005` down to 3 and puts a tick
/// at 60.0, below the minimum. Widening the candidate range by one index at
/// each end and then asking whether the finished value is inside gets both
/// right and needs no tolerance at all.
fn ticks_for_step(min: f64, max: f64, step: LadderStep) -> Vec<f64> {
    let value = step.value();
    if !value.is_finite() || value <= 0.0 {
        return Vec::new();
    }
    if !step_is_resolvable(min, max, value) {
        return Vec::new();
    }

    // One index wider than the range at each end. The two divisions can land a
    // fraction of an ulp on the wrong side of an integer, so the extra
    // candidate is what keeps an endpoint that is a genuine multiple of the
    // step; the range test in the loop is what throws it out when it is not.
    let first = (min / value).ceil() - 1.0;
    let last = (max / value).floor() + 1.0;
    if !first.is_finite() || !last.is_finite() {
        return Vec::new();
    }
    if last - first + 1.0 > MAX_TICKS as f64 {
        return Vec::new();
    }

    let decimals = step.decimals();
    let mut ticks = Vec::new();
    let mut multiple = first;
    // This loop terminates because `step_is_resolvable` bounds every index by
    // `1 / (4 * f64::EPSILON)`, which is about 1.1e15 and so below the 9.0e15
    // where `multiple + 1.0` would stop advancing, and because the check above
    // bounds the number of iterations by MAX_TICKS.
    while multiple <= last {
        let tick = snap_to_decimals(multiple * value, decimals);
        if tick >= min && tick <= max {
            ticks.push(tick);
        }
        multiple += 1.0;
    }
    ticks
}

/// Whether a step of `value` can be walked across `[min, max]` in `f64` at all.
///
/// See [`MIN_STEP_SPACINGS`] for the failure this prevents.
fn step_is_resolvable(min: f64, max: f64, value: f64) -> bool {
    let magnitude = min.abs().max(max.abs());
    value > magnitude * f64::EPSILON * MIN_STEP_SPACINGS
}

/// `10^exponent`, written so the negative case is visibly one division of an
/// exact integer power rather than whatever `powi` chooses to do with a
/// negative argument. Exact for every exponent a legend ever sees.
fn power_of_ten(exponent: i32) -> f64 {
    if exponent >= 0 {
        10f64.powi(exponent)
    } else {
        1.0 / 10f64.powi(-exponent)
    }
}

/// Round a tick to the decimal places its step needs.
///
/// `3.0 * 0.2` is 0.6000000000000001 in `f64`, and that is what the label
/// would say. Rounding to the one decimal place a step of 0.2 needs lands on
/// the same double as the literal `0.6`, so a caller may compare ticks for
/// equality and a formatter may print them without a wall of digits.
fn snap_to_decimals(value: f64, decimals: i32) -> f64 {
    if decimals > MAX_SNAP_DECIMALS {
        return normalise_zero(value);
    }
    let factor = power_of_ten(decimals);
    let scaled = value * factor;
    if !scaled.is_finite() || scaled.abs() >= MAX_ROUNDABLE {
        return normalise_zero(value);
    }
    normalise_zero(scaled.round() / factor)
}

/// Turn -0.0 into 0.0.
///
/// `-0.0 == 0.0` is true, so this changes no comparison. It changes what the
/// label says: `format!("{}", -0.0_f64)` is "-0", and a colour bar with "-0"
/// printed on it reads as a bug to whoever is looking at it.
///
/// No path in [`ticks_for_step`] currently reaches it. It used to: the first
/// index came from `(min / value).ceil()`, and `(-3.0_f64 / 5.0).ceil()` is
/// -0.0, so every range with a negative minimum and a tick on zero produced
/// one. The index walk now starts a rung lower, so the zero tick is reached as
/// `0.0 * value`. This stays because it is one comparison, because
/// `(-0.4_f64).round()` is -0.0 and the rounding below is one edit away from
/// depending on that, and because the test that pins it is what would notice.
fn normalise_zero(value: f64) -> f64 {
    if value == 0.0 { 0.0 } else { value }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Every expected tick below is a whole multiple of its step, so each one is
    // the same f64 the literal in the test parses to and exact equality is the
    // honest assertion. Where a step is not itself a power of ten (0.2, 0.25)
    // that is only true because snap_to_decimals put it back: the raw product
    // 3.0 * 0.2 is 0.6000000000000001, and a test written around that number
    // would be pinning the defect rather than the behaviour.

    #[test]
    fn a_reflectivity_span_from_minus_thirty_two_to_ninety_four_point_five_dbz_gets_twenty_dbz_ticks()
     {
        // The default TickHint asks for 6 intervals. 126.5 / 6 is 21.08, and
        // the ladder rounds that to 20.
        assert_eq!(
            nice_ticks(-32.0, 94.5, 6),
            vec![-20.0, 0.0, 20.0, 40.0, 60.0, 80.0]
        );
        assert_eq!(nice_step(126.5, 6), 20.0);
    }

    #[test]
    fn a_velocity_span_of_plus_or_minus_one_hundred_knots_gets_fifty_knot_ticks_at_both_ends() {
        assert_eq!(
            nice_ticks(-100.0, 100.0, 5),
            vec![-100.0, -50.0, 0.0, 50.0, 100.0]
        );
        assert_eq!(nice_step(200.0, 5), 50.0);
    }

    #[test]
    fn a_correlation_coefficient_span_gets_two_tenth_ticks_rather_than_a_step_of_one() {
        // 0.85 / 5 is 0.17, which rounds up the ladder to 0.2. A step of 1
        // would put a single tick on the whole bar.
        assert_eq!(nice_ticks(0.2, 1.05, 5), vec![0.2, 0.4, 0.6, 0.8, 1.0]);
        assert_eq!(nice_step(0.85, 5), 0.2);
    }

    #[test]
    fn a_vil_density_span_of_zero_to_twelve_grams_gets_two_gram_ticks() {
        assert_eq!(
            nice_ticks(0.0, 12.0, 6),
            vec![0.0, 2.0, 4.0, 6.0, 8.0, 10.0, 12.0]
        );
        assert_eq!(nice_step(12.0, 6), 2.0);
    }

    #[test]
    fn an_echo_top_span_of_zero_to_seventy_kilofeet_gets_ten_kilofoot_ticks() {
        assert_eq!(
            nice_ticks(0.0, 70.0, 7),
            vec![0.0, 10.0, 20.0, 30.0, 40.0, 50.0, 60.0, 70.0]
        );
        assert_eq!(nice_step(70.0, 7), 10.0);
    }

    #[test]
    fn a_span_of_half_a_unit_gets_tenth_ticks_so_the_step_falls_below_one() {
        assert_eq!(nice_ticks(0.0, 0.5, 5), vec![0.0, 0.1, 0.2, 0.3, 0.4, 0.5]);
        assert_eq!(nice_step(0.5, 5), 0.1);
    }

    #[test]
    fn a_span_that_wants_two_and_a_half_gets_it_because_the_ladder_carries_that_rung() {
        assert_eq!(nice_ticks(0.0, 10.0, 4), vec![0.0, 2.5, 5.0, 7.5, 10.0]);
        assert_eq!(nice_step(25.0, 10), 2.5);
    }

    #[test]
    fn a_zero_span_returns_no_ticks_rather_than_looping_on_a_zero_step() {
        assert_eq!(nice_ticks(37.5, 37.5, 6), Vec::<f64>::new());
        assert_eq!(nice_step(0.0, 6), 0.0);
    }

    #[test]
    fn an_inverted_range_returns_no_ticks_rather_than_silently_swapping_the_ends() {
        // Swapping would draw a plausible legend over a caller's bug and leave
        // nobody anything to notice.
        assert_eq!(nice_ticks(94.5, -32.0, 6), Vec::<f64>::new());
        assert_eq!(nice_step(-126.5, 6), 0.0);
    }

    #[test]
    fn a_nan_bound_returns_no_ticks() {
        assert_eq!(nice_ticks(f64::NAN, 100.0, 6), Vec::<f64>::new());
        assert_eq!(nice_ticks(-100.0, f64::NAN, 6), Vec::<f64>::new());
        assert_eq!(nice_ticks(f64::NAN, f64::NAN, 6), Vec::<f64>::new());
        assert_eq!(nice_step(f64::NAN, 6), 0.0);
    }

    #[test]
    fn an_infinite_bound_returns_no_ticks() {
        assert_eq!(nice_ticks(f64::NEG_INFINITY, 100.0, 6), Vec::<f64>::new());
        assert_eq!(nice_ticks(-100.0, f64::INFINITY, 6), Vec::<f64>::new());
        assert_eq!(
            nice_ticks(f64::NEG_INFINITY, f64::INFINITY, 6),
            Vec::<f64>::new()
        );
        assert_eq!(nice_step(f64::INFINITY, 6), 0.0);
    }

    #[test]
    fn a_range_wide_enough_that_its_span_overflows_to_infinity_returns_no_ticks() {
        // Both bounds are finite here; max - min is not.
        assert_eq!(nice_ticks(-1.5e308, 1.5e308, 6), Vec::<f64>::new());
    }

    #[test]
    fn a_tick_that_lands_on_negative_zero_is_returned_as_positive_zero() {
        // This is the range that used to produce one: -3.0 / 5.0 is -0.6 and
        // `ceil` of that is -0.0, so the first tick was computed as
        // -0.0 * 5.0 = -0.0 and would have printed as "-0".
        let ticks = nice_ticks(-3.0, 12.0, 3);
        assert_eq!(ticks, vec![0.0, 5.0, 10.0]);
        assert!(
            !ticks[0].is_sign_negative(),
            "zero tick came back as negative zero and would label as -0"
        );
        assert_eq!(format!("{}", ticks[0]), "0");
    }

    #[test]
    fn a_negative_endpoint_that_is_a_multiple_of_the_step_survives_an_inexact_division() {
        // -0.3 / 0.1 is -2.9999999999999996, so an unsnapped ceil starts the
        // ladder at -0.2 and loses the endpoint the caller asked for.
        assert_eq!(
            nice_ticks(-0.3, 0.3, 6),
            vec![-0.3, -0.2, -0.1, 0.0, 0.1, 0.2, 0.3]
        );
    }

    #[test]
    fn a_range_that_contains_no_multiple_of_the_ladder_step_steps_down_until_it_holds_two() {
        // 0.01 to 0.99 with one interval wanted picks a step of 1, and there is
        // no multiple of 1 inside it at all. Two descents reach 0.25.
        assert_eq!(nice_ticks(0.01, 0.99, 1), vec![0.25, 0.5, 0.75]);
        // nice_step still reports the span's own rung; only nice_ticks descends.
        assert_eq!(nice_step(0.98, 1), 1.0);
    }

    #[test]
    fn a_tie_between_two_ladder_steps_resolves_to_the_larger_step_so_labels_stay_sparse() {
        // 200 / 25 is 8 intervals and 200 / 50 is 4; both miss a target of 6 by
        // exactly 2.
        assert_eq!(nice_step(200.0, 6), 50.0);
        assert_eq!(
            nice_ticks(-100.0, 100.0, 6),
            vec![-100.0, -50.0, 0.0, 50.0, 100.0]
        );
    }

    #[test]
    fn a_target_of_zero_intervals_is_read_as_one_rather_than_dividing_by_zero() {
        assert_eq!(nice_step(200.0, 0), nice_step(200.0, 1));
        assert_eq!(nice_step(200.0, 0), 200.0);
        // A step of 200 leaves only one multiple inside -100 to 100, so the
        // ladder descends once to 100 to keep two ticks.
        assert_eq!(nice_ticks(-100.0, 100.0, 0), vec![-100.0, 0.0, 100.0]);
    }

    #[test]
    fn every_non_zero_finite_span_returns_at_least_two_ticks() {
        let cases: [(f64, f64, u8); 8] = [
            (-32.0, 94.5, 6),
            (0.0, 0.001, 5),
            (-1.0e6, 1.0e6, 3),
            (0.9, 0.91, 8),
            (12.0, 12.5, 2),
            (0.01, 0.99, 1),
            (-0.0004, 0.0004, 4),
            (1.0e-6, 3.0e-6, 7),
        ];
        for (min, max, target) in cases {
            let ticks = nice_ticks(min, max, target);
            assert!(
                ticks.len() >= 2,
                "span {min} to {max} at target {target} produced {ticks:?}"
            );
        }

        // The same claim swept across sixteen decades of magnitude.
        for exponent in -6..=9 {
            let scale = 10f64.powi(exponent);
            let ticks = nice_ticks(0.37 * scale, 1.83 * scale, 6);
            assert!(ticks.len() >= 2, "span at 1e{exponent} produced {ticks:?}");
        }
    }

    #[test]
    fn every_returned_tick_lies_inside_the_requested_range_and_ascends() {
        // The last four are the shape that used to break both claims at once:
        // a range sitting far from zero and only a hair wide, where the tick
        // index is enormous and the step is close to the spacing of the
        // doubles. Those produced ticks a whole step outside the range and
        // runs of identical values.
        let cases: [(f64, f64, u8); 10] = [
            (-32.0, 94.5, 6),
            (-100.0, 100.0, 5),
            (0.2, 1.05, 5),
            (0.0, 12.0, 6),
            (0.0, 70.0, 7),
            (0.01, 0.99, 1),
            (1.0e6, 1.0e6 + 0.001, 255),
            (1.0e12, 1.0e12 + 1.0, 6),
            (1.0e12, 1.0e12 + 16.0, 0),
            (1.0e17, 1.0e17 + 1000.0, 6),
        ];
        for (min, max, target) in cases {
            let ticks = nice_ticks(min, max, target);
            for tick in &ticks {
                assert!(
                    *tick >= min && *tick <= max,
                    "tick {tick} escaped the range {min} to {max}"
                );
            }
            for pair in ticks.windows(2) {
                assert!(
                    pair[1] > pair[0],
                    "ticks {ticks:?} do not ascend in {min} to {max}"
                );
            }
        }
    }

    #[test]
    fn the_gap_between_neighbouring_ticks_is_the_step_the_ladder_reported() {
        let cases: [(f64, f64, u8); 5] = [
            (-32.0, 94.5, 6),
            (-100.0, 100.0, 5),
            (0.2, 1.05, 5),
            (0.0, 12.0, 6),
            (0.0, 70.0, 7),
        ];
        for (min, max, target) in cases {
            let step = nice_step(max - min, target);
            let ticks = nice_ticks(min, max, target);
            for pair in ticks.windows(2) {
                let gap = pair[1] - pair[0];
                // One part in 1e9 of the step, not tighter: the ticks are
                // snapped to the step's decimal places and subtracting two
                // snapped doubles carries about 1e-16 relative error, which
                // this clears by seven orders of magnitude. A wrong rung would
                // miss by a factor of 2 or more, so nothing real hides in here.
                assert!(
                    (gap - step).abs() <= 1e-9 * step,
                    "gap {gap} is not the reported step {step} in {min} to {max}"
                );
            }
        }
    }

    #[test]
    fn the_largest_target_a_u8_can_hold_stays_under_the_tick_cap() {
        // 255 intervals is the most a caller can ask for, and the ladder never
        // overshoots a target far enough to reach MAX_TICKS, so no answerable
        // range is ever refused for being too long. A range that would be long
        // enough is one whose ticks f64 cannot tell apart, and that is refused
        // one check earlier, for that reason instead.
        let ticks = nice_ticks(-32.0, 94.5, 255);
        assert_eq!(ticks.len(), 254);
        assert!(ticks.len() < MAX_TICKS);
    }

    #[test]
    fn a_range_too_narrow_for_its_own_magnitude_returns_no_ticks_rather_than_a_column_of_duplicates()
     {
        // The gap between neighbouring doubles at 1e17 is 16, so this range is
        // as narrow as a range can be there. Every multiple of any step small
        // enough to fit inside it rounds to the same double, and the f64 index
        // that walks them sits above 2^53 where `index + 1.0` is `index`.
        // This used to return 512 copies of 1e17 - a ladder with 511 zero-width
        // gaps for a legend to divide by - and at target 6 as well as 255, so
        // it was not confined to absurd targets.
        assert_eq!(nice_ticks(1.0e17, 1.0e17 + 16.0, 255), Vec::<f64>::new());
        assert_eq!(nice_ticks(1.0e17, 1.0e17 + 16.0, 6), Vec::<f64>::new());
        // Two decades of headroom make the same shape drawable again: the
        // doubles near 1e15 are 0.125 apart, so a step of 2.5 is twenty of them
        // and every tick is a distinct, exactly representable number.
        assert_eq!(
            nice_ticks(1.0e15, 1.0e15 + 16.0, 6),
            vec![
                1.0e15,
                1.0e15 + 2.5,
                1.0e15 + 5.0,
                1.0e15 + 7.5,
                1.0e15 + 10.0,
                1.0e15 + 12.5,
                1.0e15 + 15.0,
            ]
        );
    }

    #[test]
    fn a_tick_above_two_to_the_fifty_second_keeps_its_step_instead_of_being_rounded_off_it() {
        // Every f64 at this magnitude is already a whole number, so the
        // cosmetic decimal rounding has nothing to clean and can only lose
        // bits: scaling 1000000000000002.5 by ten lands on 10000000000000024,
        // and dividing that back gives 1000000000000002.375 - an eighth of a
        // unit off the ladder the tick belongs to. The gaps must be the step,
        // exactly, and exactly is available here because both neighbours are
        // representable and their difference is computed without rounding.
        let ticks = nice_ticks(1.0e15, 1.0e15 + 16.0, 6);
        assert_eq!(ticks.len(), 7);
        for pair in ticks.windows(2) {
            assert_eq!(pair[1] - pair[0], 2.5);
        }
    }

    #[test]
    fn a_minimum_a_hair_above_a_round_number_gets_no_tick_on_that_round_number() {
        // 60.00000001 / 20 is 3.0000000005. Snapping that index to 3 - which a
        // relative tolerance of 1e-9 does - puts a tick at 60.0, one hundredth
        // of a micro-unit BELOW the minimum the caller asked for, and it is a
        // whole 60 dBZ label sitting outside the bar. Testing the tick value
        // instead of the tick index is what keeps it out.
        assert_eq!(nice_ticks(60.000000010, 100.0, 2), vec![80.0, 100.0]);
        assert_eq!(
            nice_ticks(60.000000010, 100.0, 6),
            vec![65.0, 70.0, 75.0, 80.0, 85.0, 90.0, 95.0, 100.0]
        );
    }

    #[test]
    fn no_tick_escapes_the_range_at_magnitudes_where_a_step_is_not_exactly_representable() {
        // At 1e200 no step on the ladder is an exact double and neither is any
        // multiple of one, so the product for the last rung can land an ulp
        // past `max` and the first an ulp under `min`. Both used to be
        // returned. There is no rounding available that fixes the arithmetic;
        // the fix is to drop a tick that misses.
        let extremes: [(f64, f64, u8); 6] = [
            (0.0, 1.0e200, 6),
            (1.0e200, 2.0e200, 6),
            (-2.0e200, -1.0e200, 10),
            (1.0e100, 2.0e100, 255),
            (0.0, 1.0e-300, 6),
            (-2.0e-300, -1.0e-300, 10),
        ];
        for (min, max, target) in extremes {
            let ticks = nice_ticks(min, max, target);
            assert!(
                ticks.len() >= 2,
                "range {min:e} to {max:e} at target {target} produced {ticks:?}"
            );
            for tick in &ticks {
                assert!(
                    *tick >= min && *tick <= max,
                    "tick {tick:e} escaped the range {min:e} to {max:e}"
                );
            }
            for pair in ticks.windows(2) {
                assert!(
                    pair[1] > pair[0],
                    "ticks {ticks:?} do not ascend in {min:e} to {max:e}"
                );
            }
        }
    }

    #[test]
    fn a_step_is_always_a_ladder_entry_times_a_power_of_ten() {
        let spans: [f64; 7] = [126.5, 200.0, 0.85, 12.0, 70.0, 0.5, 2.0e6];
        for span in spans {
            let step = nice_step(span, 6);
            let exponent = step.log10().floor() as i32;
            let mantissa = step / power_of_ten(exponent);
            assert!(
                LADDER.iter().any(|entry| (entry - mantissa).abs() < 1e-9),
                "step {step} for span {span} has mantissa {mantissa}, which is not on the ladder"
            );
        }
    }
}
