//! Render one sweep of a CfRadial 1.x file to a PNG.
//!
//! CfRadial carries fields under whatever names its acquisition system used,
//! so the moment argument accepts both the canonical short names (`REF`,
//! `VEL`, `RHO`, ...) and a raw CF field name (`reflectivity`,
//! `linear_depolarization_ratio`, ...) for fields that have no canonical
//! stem. With no moment argument it renders the first moment in the cut,
//! which is the quickest way to eyeball an unfamiliar file.
//!
//! usage: cargo run --release -p render2d --example render_cfradial_png \
//!            -- <cfradial.nc> <out.png> [cut-index] [moment]

use std::path::{Path, PathBuf};

use radar_core::MomentType;
use render2d::{RasterOptions, render_moment_png};

fn main() {
    let mut args = std::env::args_os().skip(1).map(PathBuf::from);
    let (Some(input), Some(output)) = (args.next(), args.next()) else {
        eprintln!(
            "usage: cargo run --release -p render2d --example render_cfradial_png -- <cfradial.nc> <out.png> [cut-index] [moment]"
        );
        std::process::exit(2);
    };
    let cut_index = std::env::args()
        .nth(3)
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(0);
    let requested = std::env::args().nth(4);

    match run(&input, &output, cut_index, requested.as_deref()) {
        Ok(moment) => println!(
            "wrote {} (cut {cut_index}, moment {moment})",
            output.display()
        ),
        Err(err) => {
            eprintln!("render failed: {err}");
            std::process::exit(1);
        }
    }
}

fn run(
    input: &Path,
    output: &Path,
    cut_index: usize,
    requested: Option<&str>,
) -> Result<MomentType, Box<dyn std::error::Error>> {
    let bytes = std::fs::read(input)?;
    let volume = nexrad_io::cfradial::decode_cfradial1_volume(&bytes)?;
    let cut = volume
        .cuts
        .get(cut_index)
        .ok_or_else(|| format!("cut {cut_index} of {}", volume.cuts.len()))?;

    let moment = match requested {
        Some(name) => resolve_moment(name),
        None => cut
            .moments
            .keys()
            .next()
            .cloned()
            .ok_or("cut carries no moments")?,
    };

    println!(
        "{}: site={} cuts={} cut[{cut_index}] fixed={:.3} deg rays={} moments={:?}",
        input.display(),
        volume.site.id,
        volume.cuts.len(),
        cut.elevation_deg,
        cut.radials.len(),
        cut.moments
            .keys()
            .map(MomentType::short_name)
            .collect::<Vec<_>>(),
    );

    render_moment_png(
        &volume,
        cut_index,
        moment.clone(),
        output,
        RasterOptions::default(),
    )?;
    Ok(moment)
}

/// Canonical short name first, otherwise the CF field name verbatim — an
/// unmatched CF name is exactly how the decoder keyed the moment.
fn resolve_moment(name: &str) -> MomentType {
    match MomentType::from_nexrad_name(&name.to_ascii_uppercase()) {
        MomentType::Unknown(_) => MomentType::Unknown(name.to_owned()),
        canonical => canonical,
    }
}
