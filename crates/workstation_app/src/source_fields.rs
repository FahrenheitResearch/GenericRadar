//! Producer-native fields preserved by research-data decoders.
//!
//! DORADE, CfRadial and ODIM can carry fields this build has never heard of.
//! The decoder keeps those as [`MomentType::Unknown`] together with whatever
//! description and unit token the container supplied. This catalog is the UI
//! boundary for that preserved information, whether or not a second canonical
//! identity is also available: it groups only exact producer names and exact
//! metadata tuples. It does not normalize case, invent a unit, or fold a
//! producer field into a canonical product that merely looks similar.

use std::collections::BTreeMap;

use product_engine::{
    AffineTransform, DisplayDomain, DisplayUnit, PhysicalUnit, PlausibleRange, TickHint, ValueRange,
};
use radar_core::{MomentGrid, MomentType, ProductId, RadarVolume};

const SOURCE_PRODUCT_PREFIX: &str = "SOURCE_FIELD:";

/// Every exact source field named by one decoded volume.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SourceFieldCatalog {
    fields: Vec<SourceField>,
}

/// One exact producer field name and the metadata variants attached to it.
#[derive(Clone, Debug, PartialEq)]
pub struct SourceField {
    pub producer_name: String,
    /// More than one entry means the producer changed description or units
    /// between cuts. Keeping each tuple visible is safer than choosing one and
    /// silently claiming it describes the whole volume.
    pub metadata: Vec<SourceFieldMetadata>,
    /// One entry per cut carrying this exact source key. The underlying moment
    /// may be canonical (for example ZH1C -> DBZH1) while producer_name remains
    /// the selectable native identity.
    pub occurrences: Vec<SourceFieldOccurrence>,
}

/// One exact description/unit tuple and the cuts on which it occurs.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SourceFieldMetadata {
    pub producer_description: Option<String>,
    pub producer_units: Option<String>,
    pub cut_indices: Vec<usize>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SourceFieldOccurrence {
    pub cut_index: usize,
    pub moment: MomentType,
    pub finite_count: usize,
    pub finite_min: Option<f32>,
    pub finite_max: Option<f32>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SourceFieldDisplay {
    pub producer_name: String,
    pub producer_description: Option<String>,
    pub producer_units: Option<String>,
    pub moment: MomentType,
    pub finite_count: usize,
    pub finite_min: f32,
    pub finite_max: f32,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct MetadataKey {
    producer_description: Option<String>,
    producer_units: Option<String>,
}

#[derive(Default)]
struct FieldBuilder {
    metadata: BTreeMap<MetadataKey, Vec<usize>>,
    occurrences: Vec<SourceFieldOccurrence>,
}

impl SourceFieldCatalog {
    /// Measure a decoded volume without interpreting any producer token.
    pub fn from_volume(volume: &RadarVolume) -> Self {
        let mut grouped: BTreeMap<String, FieldBuilder> = BTreeMap::new();

        for (cut_index, cut) in volume.cuts.iter().enumerate() {
            for (moment, grid) in &cut.moments {
                let Some(producer_name) = producer_name(grid) else {
                    continue;
                };
                let key = MetadataKey {
                    producer_description: grid.producer_description.clone(),
                    producer_units: grid.producer_units.clone(),
                };
                let builder = grouped.entry(producer_name.to_owned()).or_default();
                let cuts = builder.metadata.entry(key).or_default();
                if cuts.last().copied() != Some(cut_index) {
                    cuts.push(cut_index);
                }
                let (finite_count, finite_min, finite_max) = finite_summary(grid);
                builder.occurrences.push(SourceFieldOccurrence {
                    cut_index,
                    moment: moment.clone(),
                    finite_count,
                    finite_min,
                    finite_max,
                });
            }
        }

        let fields = grouped
            .into_iter()
            .map(|(producer_name, builder)| SourceField {
                producer_name,
                metadata: builder
                    .metadata
                    .into_iter()
                    .map(|(key, cut_indices)| SourceFieldMetadata {
                        producer_description: key.producer_description,
                        producer_units: key.producer_units,
                        cut_indices,
                    })
                    .collect(),
                occurrences: builder.occurrences,
            })
            .collect();
        Self { fields }
    }

    pub fn fields(&self) -> &[SourceField] {
        &self.fields
    }

    pub fn find(&self, producer_name: &str) -> Option<&SourceField> {
        self.fields
            .binary_search_by(|field| field.producer_name.as_str().cmp(producer_name))
            .ok()
            .map(|index| &self.fields[index])
    }

    pub fn display_on_cut(
        &self,
        producer_name: &str,
        cut_index: usize,
    ) -> Option<SourceFieldDisplay> {
        let field = self.find(producer_name)?;
        let occurrence = field
            .occurrences
            .iter()
            .find(|occurrence| occurrence.cut_index == cut_index)?;
        let metadata = field
            .metadata
            .iter()
            .find(|metadata| metadata.cut_indices.contains(&cut_index));
        Some(SourceFieldDisplay {
            producer_name: producer_name.to_owned(),
            producer_description: metadata
                .and_then(|metadata| metadata.producer_description.clone()),
            producer_units: metadata.and_then(|metadata| metadata.producer_units.clone()),
            moment: occurrence.moment.clone(),
            finite_count: occurrence.finite_count,
            finite_min: occurrence.finite_min?,
            finite_max: occurrence.finite_max?,
        })
    }
}

/// Namespace a native name inside the pane's existing ProductId storage.
pub fn product_id(producer_name: &str) -> ProductId {
    ProductId(format!("{SOURCE_PRODUCT_PREFIX}{producer_name}"))
}

/// Recover a native name exactly. Unlike registry lookup this is deliberately
/// case-sensitive: producer names differing only by case remain distinct.
pub fn producer_name_from_product_id(id: &ProductId) -> Option<&str> {
    id.0.strip_prefix(SOURCE_PRODUCT_PREFIX)
        .filter(|name| !name.is_empty())
}

pub fn grid_in_cut<'a>(
    volume: &'a RadarVolume,
    cut_index: usize,
    wanted_name: &str,
) -> Option<(&'a MomentType, &'a MomentGrid)> {
    volume
        .cuts
        .get(cut_index)?
        .moments
        .iter()
        .find(|(_, grid)| producer_name(grid).is_some_and(|name| name == wanted_name))
}

/// Numeric identity domain for a generic source display. `Dimensionless` here
/// is only the engine's no-conversion sentinel; the UI prints the producer unit
/// token beside the field and never calls the field dimensionless.
pub fn numeric_domain(minimum: f32, maximum: f32) -> DisplayDomain {
    let (minimum, maximum) = crate::palettes::drawable_source_range(minimum, maximum);
    DisplayDomain {
        engine_unit: PhysicalUnit::Dimensionless,
        display_unit: DisplayUnit::Dimensionless,
        display_from_engine: AffineTransform::IDENTITY,
        declared_engine_range: ValueRange::new(minimum, maximum),
        plausible: PlausibleRange::new(minimum, maximum, minimum, maximum),
        tick_hint: TickHint::DEFAULT,
        decimals: 3,
    }
}

fn producer_name(grid: &MomentGrid) -> Option<&str> {
    // `Unknown` is an engine classification, not proof that the container
    // named a field. Level 1 diagnostics such as SNR/SQI are computed here
    // and also use Unknown; only an explicit decoder-populated producer_name
    // establishes the producer-native identity advertised by this catalog.
    grid.producer_name.as_deref()
}

fn finite_summary(grid: &MomentGrid) -> (usize, Option<f32>, Option<f32>) {
    let mut count = 0usize;
    let mut minimum = f32::INFINITY;
    let mut maximum = f32::NEG_INFINITY;
    for row in 0..grid.radial_count() {
        for gate in 0..grid.gate_range.gate_count {
            let Some(value) = grid
                .scaled_value(row, gate)
                .filter(|value| value.is_finite())
            else {
                continue;
            };
            count += 1;
            minimum = minimum.min(value);
            maximum = maximum.max(value);
        }
    }
    if count == 0 {
        (0, None, None)
    } else {
        (count, Some(minimum), Some(maximum))
    }
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use radar_core::{
        DowFrequencyProduct, GateRange, MomentGrid, RadarReceiverChannel, RadarSite, ResearchMoment,
    };

    use super::*;

    fn grid(moment: MomentType, description: Option<&str>, units: Option<&str>) -> MomentGrid {
        let producer_name = match &moment {
            MomentType::Unknown(name) => Some(name.clone()),
            _ => None,
        };
        let mut grid = MomentGrid::new_u8(
            moment,
            GateRange {
                first_gate_m: 0,
                gate_spacing_m: 150,
                gate_count: 1,
            },
            1.0,
            0.0,
            None,
            None,
        );
        grid.producer_name = producer_name;
        grid.producer_description = description.map(str::to_owned);
        grid.producer_units = units.map(str::to_owned);
        grid
    }

    fn insert(
        volume: &mut RadarVolume,
        cut_index: usize,
        moment: MomentType,
        description: Option<&str>,
        units: Option<&str>,
    ) {
        while volume.cuts.len() <= cut_index {
            let index = volume.cuts.len();
            volume.push_cut(index as f32, Some(index as u8 + 1));
        }
        volume.cuts[cut_index]
            .moments
            .insert(moment.clone(), grid(moment, description, units));
    }

    #[test]
    fn exact_names_and_metadata_survive_without_aliasing_or_inference() {
        let mut volume = RadarVolume::new(RadarSite::new("TEST"), Utc::now());
        insert(
            &mut volume,
            0,
            MomentType::Unknown("NVM".to_owned()),
            Some("Normalized velocity metric"),
            Some("arb"),
        );
        insert(
            &mut volume,
            1,
            MomentType::Unknown("NVM".to_owned()),
            Some("Normalized velocity metric"),
            Some("arb"),
        );
        insert(
            &mut volume,
            2,
            MomentType::Unknown("NVM".to_owned()),
            None,
            Some("counts"),
        );
        // Case is producer identity, not an alias rule.
        insert(
            &mut volume,
            0,
            MomentType::Unknown("nvm".to_owned()),
            Some("lower-case producer field"),
            None,
        );
        // Canonical and explicitly modeled DOW fields belong to their own
        // registry groups, never to this generic source-field catalog.
        insert(
            &mut volume,
            0,
            MomentType::Reflectivity,
            Some("producer reflectivity"),
            Some("dBZ"),
        );
        insert(
            &mut volume,
            0,
            MomentType::Research(ResearchMoment::DowReceivedPower {
                receiver: RadarReceiverChannel::Horizontal,
                frequency: DowFrequencyProduct::Frequency1,
            }),
            Some("DOW H power frequency 1"),
            Some("dBm"),
        );

        let catalog = SourceFieldCatalog::from_volume(&volume);
        assert_eq!(catalog.fields().len(), 2);
        assert_eq!(catalog.fields[0].producer_name, "NVM");
        assert_eq!(catalog.fields[0].metadata.len(), 2);
        assert_eq!(
            catalog.fields[0].metadata[0],
            SourceFieldMetadata {
                producer_description: None,
                producer_units: Some("counts".to_owned()),
                cut_indices: vec![2],
            }
        );
        assert_eq!(
            catalog.fields[0].metadata[1],
            SourceFieldMetadata {
                producer_description: Some("Normalized velocity metric".to_owned()),
                producer_units: Some("arb".to_owned()),
                cut_indices: vec![0, 1],
            }
        );
        assert_eq!(catalog.fields[1].producer_name, "nvm");
        assert_eq!(catalog.fields[1].metadata[0].producer_units, None);
    }

    #[test]
    fn a_volume_without_unknown_moments_has_no_source_fields() {
        let mut volume = RadarVolume::new(RadarSite::new("TEST"), Utc::now());
        insert(
            &mut volume,
            0,
            MomentType::Velocity,
            Some("radial velocity"),
            Some("m/s"),
        );
        assert!(SourceFieldCatalog::from_volume(&volume).fields().is_empty());
    }

    #[test]
    fn an_internal_unknown_without_a_producer_name_is_not_called_a_source_field() {
        let mut volume = RadarVolume::new(RadarSite::new("TEST"), Utc::now());
        let cut = volume.push_cut(0.5, Some(1));
        let moment = MomentType::Unknown("SQI".to_owned());
        let grid = MomentGrid::new_u8(
            moment.clone(),
            GateRange {
                first_gate_m: 0,
                gate_spacing_m: 150,
                gate_count: 1,
            },
            1.0,
            0.0,
            None,
            None,
        );
        cut.moments.insert(moment, grid);

        assert!(SourceFieldCatalog::from_volume(&volume).fields().is_empty());
    }

    /// Manual acceptance pin for the DOW7 sweep that established the native
    /// field contract. The sample is intentionally not copied into the repo.
    #[ignore = "set DOW_DORADE_SAMPLE to the real DOW7 sweepfile"]
    #[test]
    fn real_dow7_exposes_every_native_field_and_only_measured_aliases() {
        let path = std::env::var("DOW_DORADE_SAMPLE")
            .expect("set DOW_DORADE_SAMPLE to a real DOW7 DORADE sweep");
        let volume = nexrad_io::dorade::decode_dorade_sweep_from_path(std::path::Path::new(&path))
            .expect("decode DOW7 sweep");
        let catalog = SourceFieldCatalog::from_volume(&volume);
        let names: Vec<&str> = catalog
            .fields()
            .iter()
            .map(|field| field.producer_name.as_str())
            .collect();
        assert_eq!(
            names,
            [
                "NCP1", "NCP2", "V1", "V2", "VL1", "VL1_CRR", "VL2", "VL2_CRR", "VS1", "VS1_CRR",
                "VS2", "VS2_CRR", "ZH1C", "ZH2C", "ZV1C", "ZV2C",
            ]
        );

        for field in catalog.fields() {
            assert_eq!(field.metadata.len(), 1, "{} metadata", field.producer_name);
            assert_eq!(
                field.occurrences.len(),
                1,
                "{} occurrences",
                field.producer_name
            );
            let metadata = &field.metadata[0];
            let occurrence = &field.occurrences[0];
            assert!(
                occurrence.finite_count > 0,
                "{} is empty",
                field.producer_name
            );
            assert!(occurrence.finite_min.is_some());
            assert!(occurrence.finite_max.is_some());
            eprintln!(
                "{}\tdescription={}\tproducer_unit_token={}\tfinite={}\tmin={:.6}\tmax={:.6}\tmoment={:?}",
                field.producer_name,
                metadata.producer_description.as_deref().unwrap_or("<none>"),
                metadata.producer_units.as_deref().unwrap_or("<none>"),
                occurrence.finite_count,
                occurrence.finite_min.unwrap(),
                occurrence.finite_max.unwrap(),
                occurrence.moment,
            );
            assert!(grid_in_cut(&volume, 0, &field.producer_name).is_some());
        }

        for (native_name, description) in [
            ("ZH1C", "DBZH1"),
            ("ZH2C", "DBZH2"),
            ("ZV1C", "DBZV1"),
            ("ZV2C", "DBZV2"),
        ] {
            let display = catalog
                .display_on_cut(native_name, 0)
                .unwrap_or_else(|| panic!("{native_name} display"));
            assert_eq!(display.producer_description.as_deref(), Some(description));
            assert_eq!(display.producer_units.as_deref(), Some("none"));
            assert_eq!(
                display.moment,
                MomentType::Research(
                    ResearchMoment::from_producer_name(description).expect("modeled description")
                )
            );
        }

        for native_name in [
            "NCP1", "NCP2", "V1", "V2", "VL1", "VL1_CRR", "VL2", "VL2_CRR", "VS1", "VS1_CRR",
            "VS2", "VS2_CRR",
        ] {
            assert_eq!(
                catalog.display_on_cut(native_name, 0).unwrap().moment,
                MomentType::Unknown(native_name.to_owned()),
                "{native_name} was assigned semantics absent from its PARM metadata"
            );
        }
        for fabricated in ["DBMHM", "DBMVM", "DBZHM", "DBZVM"] {
            assert!(catalog.find(fabricated).is_none());
        }
    }
}
