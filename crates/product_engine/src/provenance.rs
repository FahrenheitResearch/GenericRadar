//! Where a product's numbers come from: which algorithm, which version of it,
//! and which paper it is supposed to implement.
//!
//! This exists so that a disagreement between this application and another one
//! can be settled by reading, rather than argued about. When two radar viewers
//! draw different MESH values for the same storm, the useful question is which
//! relation each implemented and at what version — not which looks better.
//!
//! The version is part of a derived field's cache key. Bumping it whenever the
//! numbers can change is what stops a stale field computed by the old code from
//! being served under the new algorithm's name.

/// A primary source, cited the way it would be cited in a paper.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LiteratureCitation {
    /// Author list as it appears on the paper, e.g. "Witt, A., and coauthors".
    pub authors: &'static str,
    pub year: u16,
    pub title: &'static str,
    /// Journal or conference, with volume and pages where they exist.
    pub venue: &'static str,
    /// A DOI where one exists, otherwise a stable URL. Empty when the source is
    /// a document that has neither, which is itself worth seeing.
    pub doi_or_url: &'static str,
}

impl LiteratureCitation {
    /// A one-line rendering for a tooltip or an export header.
    pub fn to_line(&self) -> String {
        let mut line = format!(
            "{} ({}): {}. {}",
            self.authors, self.year, self.title, self.venue
        );
        if !self.doi_or_url.is_empty() {
            line.push_str(". ");
            line.push_str(self.doi_or_url);
        }
        line
    }
}

/// How firmly an implementation is tied to its source.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AlgorithmStatus {
    /// Decoding or arithmetic fixed by a specification, with no judgement in
    /// it. Base moments are this.
    OperationalDefinition,
    /// A published relation applied faithfully, but in a setting the paper did
    /// not cover — for example a cell-based hail relation applied per grid
    /// column. The numbers are the paper's; the framing is not.
    LiteratureAdaptation,
    /// A signature described qualitatively in the literature, implemented here
    /// as a proxy. Not to be presented as a measurement.
    ExperimentalProxy,
    /// The primary source has not been read. No number may be produced.
    PendingPrimaryVerification,
}

impl AlgorithmStatus {
    pub const fn label(self) -> &'static str {
        match self {
            Self::OperationalDefinition => "operational definition",
            Self::LiteratureAdaptation => "literature adaptation",
            Self::ExperimentalProxy => "experimental proxy",
            Self::PendingPrimaryVerification => "pending primary verification",
        }
    }

    /// Whether a product with this status may print a number at all.
    pub const fn may_produce_values(self) -> bool {
        !matches!(self, Self::PendingPrimaryVerification)
    }
}

/// The identity of the algorithm behind one product.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AlgorithmMetadata {
    /// Stable machine name, e.g. "mesh.witt1998". Enters cache keys and
    /// exports, so it does not change when the display name does.
    pub implementation_id: &'static str,
    /// Bumped whenever the output numbers can change.
    pub version: u16,
    pub status: AlgorithmStatus,
    pub citations: &'static [LiteratureCitation],
}

impl AlgorithmMetadata {
    pub const fn new(
        implementation_id: &'static str,
        version: u16,
        status: AlgorithmStatus,
        citations: &'static [LiteratureCitation],
    ) -> Self {
        Self {
            implementation_id,
            version,
            status,
            citations,
        }
    }
}

/// The NEXRAD Level II format. Base moment decoding is defined here, not
/// derived from a research result.
pub const NEXRAD_LEVEL_II_ICD: LiteratureCitation = LiteratureCitation {
    authors: "NOAA/NWS Radar Operations Center",
    year: 2020,
    title: "Interface Control Document for the Archive II/User",
    venue: "ICD 2620010",
    doi_or_url: "https://www.roc.noaa.gov/public-documents/icds/2620010P.pdf",
};

/// The DOW6/7 dual-frequency processing design: two complete radar chains,
/// independently producing moments before their products are merged
/// downstream.
pub const DIXON_ET_AL_2013: LiteratureCitation = LiteratureCitation {
    authors: "Dixon, M., E. Loew, J. Wurman, and K. Kosiba",
    year: 2013,
    title: "Signal Processing in the DOWs Using Pentek Processors",
    venue: "36th Conference on Radar Meteorology, AMS, paper 16B.5",
    doi_or_url: "https://ams.confex.com/ams/36Radar/webprogram/Paper229282.html",
};

/// The published DOW field dictionary. It defines DBM as received power in
/// dBm and DBZ as equivalent reflectivity factor in dBZ for the horizontal
/// and vertical receiver channels.
pub const NASA_DOW_OLYMPEX_GUIDE_2017: LiteratureCitation = LiteratureCitation {
    authors: "NASA Global Hydrology Resource Center DAAC",
    year: 2017,
    title: "Data User Guide: GPM Ground Validation Doppler on Wheels (DOW) OLYMPEX Radar Data",
    venue: "GPM Ground Validation data documentation",
    doi_or_url: "https://ghrc.nsstc.nasa.gov/uso/ds_details/inactive/gpmdowolyx/gpmdowolyx_dataset_v1.pdf",
};

/// The continuity approach the velocity unfolder in `render2d` follows: walk
/// outward along a radial from a trusted seed and add Nyquist intervals to keep
/// successive gates continuous.
pub const BERGEN_ALBERS_1988: LiteratureCitation = LiteratureCitation {
    authors: "Bergen, W. R., and S. C. Albers",
    year: 1988,
    title: "Two- and Three-Dimensional De-Aliasing of Doppler Radar Velocities",
    venue: "Journal of Atmospheric and Oceanic Technology, 5, 305-319",
    doi_or_url: "10.1175/1520-0426(1988)005<0305:TATDDO>2.0.CO;2",
};

/// The continuous form of vertically integrated liquid.
pub const GREENE_CLARK_1972: LiteratureCitation = LiteratureCitation {
    authors: "Greene, D. R., and R. A. Clark",
    year: 1972,
    title: "Vertically Integrated Liquid Water - A New Analysis Tool",
    venue: "Monthly Weather Review, 100, 548-552",
    doi_or_url: "10.1175/1520-0493(1972)100<0548:VILWNA>2.3.CO;2",
};

/// The discretised layer sum actually used for VIL, and VIL density.
///
/// Cited separately from Greene and Clark because the discrete form is not in
/// their paper. Attributing it to them would be citing a paper that does not
/// contain the equation being implemented.
pub const AMBURN_WOLF_1997: LiteratureCitation = LiteratureCitation {
    authors: "Amburn, S. A., and P. L. Wolf",
    year: 1997,
    title: "VIL Density as a Hail Indicator",
    venue: "Weather and Forecasting, 12, 473-478",
    doi_or_url: "10.1175/1520-0434(1997)012<0473:VDAAHI>2.0.CO;2",
};

/// Echo-top interpolation between bracketing beams.
pub const LAKSHMANAN_ET_AL_2013: LiteratureCitation = LiteratureCitation {
    authors: "Lakshmanan, V., K. Hondl, C. K. Potvin, and D. Preignitz",
    year: 2013,
    title: "An Improved Method for Estimating Radar Echo-Top Height",
    venue: "Weather and Forecasting, 28, 481-488",
    doi_or_url: "10.1175/WAF-D-12-00084.1",
};

/// The severe hail index, MESH and POSH.
pub const WITT_ET_AL_1998: LiteratureCitation = LiteratureCitation {
    authors: "Witt, A., M. D. Eilts, G. J. Stumpf, J. T. Johnson, E. D. Mitchell, and K. W. Thomas",
    year: 1998,
    title: "An Enhanced Hail Detection Algorithm for the WSR-88D",
    venue: "Weather and Forecasting, 13, 286-303",
    doi_or_url: "10.1175/1520-0434(1998)013<0286:AEHDAF>2.0.CO;2",
};

/// The 45 dBZ criterion and the hailpad dataset behind the probability of hail.
pub const WALDVOGEL_ET_AL_1979: LiteratureCitation = LiteratureCitation {
    authors: "Waldvogel, A., B. Federer, and P. Grimm",
    year: 1979,
    title: "Criteria for the Detection of Hail Cells",
    venue: "Journal of Applied Meteorology, 18, 1521-1525",
    doi_or_url: "10.1175/1520-0450(1979)018<1521:CFTDOH>2.0.CO;2",
};

/// Where the probability-of-hail table is actually published.
///
/// Waldvogel et al. (1979) give a binary detection criterion, not a
/// probability curve. The eleven-point table in common use is Foote's Table 1,
/// and citing it to Waldvogel is a mis-attribution this constant exists to
/// prevent.
pub const FOOTE_ET_AL_2005: LiteratureCitation = LiteratureCitation {
    authors: "Foote, G. B., T. W. Krauss, and V. Makitov",
    year: 2005,
    title: "Hail Metrics Using Conventional Radar",
    venue: "85th AMS Annual Meeting, 16th Conf. on Planned and Inadvertent Weather Modification, paper 1.5",
    doi_or_url: "",
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_citation_renders_as_one_readable_line() {
        assert_eq!(
            BERGEN_ALBERS_1988.to_line(),
            "Bergen, W. R., and S. C. Albers (1988): Two- and Three-Dimensional \
             De-Aliasing of Doppler Radar Velocities. Journal of Atmospheric and \
             Oceanic Technology, 5, 305-319. \
             10.1175/1520-0426(1988)005<0305:TATDDO>2.0.CO;2"
        );
    }

    #[test]
    fn a_pending_algorithm_may_not_produce_values() {
        assert!(!AlgorithmStatus::PendingPrimaryVerification.may_produce_values());
        assert!(AlgorithmStatus::ExperimentalProxy.may_produce_values());
        assert!(AlgorithmStatus::OperationalDefinition.may_produce_values());
    }
}
