//! The box-geometry controls: how big the 3D box is, and where it sits.
//!
//! These two belong together and are separated from the rest of the toolbar for
//! a reason beyond tidiness: they are the only controls that invalidate the
//! uploaded box. Both the box half-width and the box centre are part of
//! [`super::Vol3dVolumeKey`], so changing either has to drop `volume_key` or
//! the pane keeps drawing the box it built for the previous geometry. Keeping
//! the two pickers and their invalidation in one place is what stops a third
//! control being added later that changes the key and forgets to clear it.

use eframe::egui;

use super::{Vol3d, Vol3dBoxCenter};

/// Draw the box size and box centre pickers, invalidating the uploaded box when
/// either changes.
pub(crate) fn box_geometry(ui: &mut egui::Ui, vol3d: &mut Vol3d) {
    let previous_half_km = vol3d.box_half_km;
    // The label carries the voxel edge because at a fixed 192 lattice the box
    // size IS the resolution, and that is the half of the choice an operator
    // cannot otherwise see on screen.
    egui::ComboBox::from_id_salt("vol3d-box-size")
        .selected_text(format!(
            "{:.0} km box ({:.0} m)",
            vol3d.box_half_km * 2.0,
            super::box_voxel_m(vol3d.box_half_km)
        ))
        .width(160.0)
        .show_ui(ui, |ui| {
            for side_km in super::BOX_SIZE_CHOICES_KM {
                ui.selectable_value(
                    &mut vol3d.box_half_km,
                    side_km * 0.5,
                    format!(
                        "{side_km:.0} km box ({:.0} m)",
                        super::box_voxel_m(side_km * 0.5)
                    ),
                );
            }
        });

    let previous_center_mode = vol3d.box_center_mode;
    egui::ComboBox::from_id_salt("vol3d-box-center")
        .selected_text(vol3d.box_center_mode.label())
        .width(130.0)
        .show_ui(ui, |ui| {
            for mode in Vol3dBoxCenter::ALL {
                ui.selectable_value(&mut vol3d.box_center_mode, mode, mode.label());
            }
        });

    if vol3d.box_center_mode != previous_center_mode {
        // Storm mode has to re-measure rather than reuse the centre it cached
        // before the operator pinned or radar-centred the box.
        vol3d.auto_center_key = None;
        vol3d.volume_key = None;
        vol3d.last_top_deg = 0.0;
    }
    if vol3d.box_half_km != previous_half_km {
        vol3d.volume_key = None;
        vol3d.last_top_deg = 0.0;
    }
}

#[cfg(test)]
mod tests {
    /// Every size the picker offers must be reachable from the default, and the
    /// default must be one of them. The default half-width moved from 60 to 30
    /// while the list still read [60, 120, 180], which left the combo box
    /// showing a size that was not selected and no way back to the default.
    #[test]
    fn the_default_box_size_is_one_the_picker_offers() {
        let default_side_km = super::super::BOX_HALF_KM * 2.0;
        assert!(
            super::super::BOX_SIZE_CHOICES_KM.contains(&default_side_km),
            "the default {default_side_km} km box is not in the picker's list {:?}",
            super::super::BOX_SIZE_CHOICES_KM
        );
    }

    /// The label is the only place the voxel edge appears, so it has to be the
    /// real one. At the fixed lattice a 60 km box is 312 m and a 360 km box is
    /// 1875 m; if these ever disagree the operator is reading a resolution the
    /// box does not have.
    #[test]
    fn the_voxel_edge_shown_is_the_one_the_box_is_built_at() {
        for side_km in super::super::BOX_SIZE_CHOICES_KM {
            let voxel_m = super::super::box_voxel_m(side_km * 0.5);
            let expected = side_km * 1000.0 / super::super::BOX_N as f32;
            assert!(
                (voxel_m - expected).abs() < 0.5,
                "{side_km} km box: label says {voxel_m} m, lattice gives {expected} m"
            );
        }
    }
}
