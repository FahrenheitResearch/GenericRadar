//! The Level 1 probe's command line, pinned.
//!
//! The probe is the tool the moment estimators were validated with against an
//! independent implementation, so a flag that is silently ignored does not
//! merely inconvenience an operator - it invalidates the comparison that was
//! run through it. `--stride 16 --dwell 64` used to discard the stride, because
//! the `--dwell` arm rebuilt the plan as a contiguous one; the operator saw
//! twenty-eight non-overlapping dwells and believed they had a sliding window.
//!
//! The example is compiled into this test rather than run as a subprocess
//! because Cargo does not expose an example's binary path to an integration
//! test, and because the parse is a pure function and deserves to be tested as
//! one.

#[path = "../examples/iq_moments_probe.rs"]
#[allow(dead_code)]
mod probe;

use nexrad_io::iq_moments::DwellPlan;
use nexrad_io::iq_moments::estimator::SnrCensor;
use nexrad_io::iq_moments::taper::Taper;
use probe::parse_options;

fn args(items: &[&str]) -> Vec<String> {
    items.iter().map(|item| (*item).to_owned()).collect()
}

#[test]
fn stride_and_dwell_mean_the_same_thing_in_either_order() {
    let expected = DwellPlan::sliding(64, 16);
    for order in [
        args(&["dump.iqd", "--stride", "16", "--dwell", "64"]),
        args(&["dump.iqd", "--dwell", "64", "--stride", "16"]),
    ] {
        let options = parse_options(&order).expect("parses");
        assert_eq!(
            options.dwell_plan(),
            expected,
            "flags in the order {order:?} should still ask for a sliding window"
        );
        assert_eq!(options.config().dwell, expected);
    }
}

#[test]
fn a_dwell_on_its_own_is_still_non_overlapping() {
    let options = parse_options(&args(&["dump.iqd", "--dwell", "32"])).expect("parses");
    assert_eq!(options.dwell_plan(), DwellPlan::contiguous(32));
    assert_eq!(options.dwell_plan().stride, 32);
}

#[test]
fn a_stride_on_its_own_slides_the_default_dwell() {
    let options = parse_options(&args(&["dump.iqd", "--stride", "8"])).expect("parses");
    let default = DwellPlan::default();
    assert_eq!(options.dwell_plan(), DwellPlan::sliding(default.pulses, 8));
}

#[test]
fn no_flags_at_all_is_the_default_plan_and_an_open_censor() {
    let options = parse_options(&args(&["dump.iqd"])).expect("parses");
    assert_eq!(options.dwell_plan(), DwellPlan::default());
    // The probe's whole purpose is seeing below the operational threshold.
    assert_eq!(options.config().censor, SnrCensor::Off);
    assert_eq!(options.path, "dump.iqd");
}

#[test]
fn every_flag_survives_being_given_alongside_every_other() {
    let options = parse_options(&args(&[
        "dump.iqd",
        "--taper",
        "hann",
        "--stride",
        "16",
        "--censor",
        "2",
        "--dwell",
        "128",
        "--burst",
        "2",
        "--dwell-index",
        "3",
        "--spectrum",
        "17",
    ]))
    .expect("parses");
    assert_eq!(options.dwell_plan(), DwellPlan::sliding(128, 16));
    assert_eq!(options.taper, Some(Taper::VonHann));
    assert_eq!(options.censor, Some(SnrCensor::MinDb(2.0)));
    assert_eq!(options.burst_samples, Some(2));
    assert_eq!(options.dwell_index, 3);
    assert_eq!(options.spectrum_gate, Some(17));

    let config = options.config();
    assert_eq!(config.taper, Taper::VonHann);
    assert_eq!(config.burst_samples, 2);
}

#[test]
fn a_bad_flag_is_a_message_rather_than_a_panic() {
    assert!(parse_options(&args(&[])).is_err());
    assert!(parse_options(&args(&["dump.iqd", "--nonsense", "1"])).is_err());
    assert!(parse_options(&args(&["dump.iqd", "--dwell", "many"])).is_err());
    assert!(parse_options(&args(&["dump.iqd", "--taper", "gaussian"])).is_err());
    assert!(parse_options(&args(&["dump.iqd", "--dwell"])).is_err());
}
