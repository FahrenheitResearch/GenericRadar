//! Cross-sections: the analyst draws a line on a 2D pane and a separate
//! window shows the vertical slice of the current product's volume along it.
//!
//! The pure sampling — volume plus line to slice grid, 4/3-earth beam
//! geometry, the between-tilt honesty rules — lives in `render2d::xsection`
//! with its citations. This module owns everything stateful: the line and its
//! gestures on the radar panes, the background slice builds with the same
//! generation gating every pane uses, and the section window with its height
//! ladder, distance axis and cursor readout.
//!
//! Deliberately free of `crate::` paths: the application hands everything in
//! through [`XSectionInput`] (exactly as the 3D explorer's pane does), so the
//! product registry, colour table and volume history stay owned by `app.rs`
//! and the same colour table that paints the pane paints the slice. That
//! independence is also what lets `tests/xsection_unit.rs` compile this file
//! before the `mod xsection;` line lands in `main.rs` — and it is why the
//! submodules below are inline blocks rather than files: an inline module
//! resolves identically under both roots.
//!
//! Gesture design, so nothing collides with what the panes already bind:
//! placing the line is an ARMED mode (a toolbar toggle, like Vrot) that
//! consumes two plain clicks — plain clicks otherwise only select a pane, and
//! site clicks and Ctrl+click are consumed before the app asks us. Endpoint
//! dragging never touches the pane's pan-drag: each endpoint is its own egui
//! widget registered after (above) the pane's, so a drag that starts on an
//! endpoint belongs to the endpoint and a drag that starts anywhere else pans
//! exactly as before. Touch: endpoints are 28-point targets (over the 24 pt
//! floor), drags are single-finger, and nothing essential hides behind hover.

use std::sync::mpsc;

use eframe::egui;
use radar_core::MomentType;
use render2d::StormMotion;

pub use build::XsCandidate;
pub use line::SectionLine;

/// Everything the cross-section needs from the application per frame.
pub struct XSectionInput<'a> {
    /// Volumes the slice may be built from, oldest first, exactly one marked
    /// `displayed`. Same contract as the 3D explorer's candidate list.
    pub candidates: &'a [XsCandidate<'a>],
    /// The moment to slice — the displayed product's source moment.
    pub moment: MomentType,
    /// Short product name for the header and the rebuild key ("REF", "DVEL").
    pub product_label: String,
    /// True for DVEL/DSRV: every velocity tilt is dealiased once per volume
    /// (memoised) before slicing.
    pub uses_dealiased_velocity: bool,
    /// `Some` for SRV/DSRV: the motion vector to subtract, already converted
    /// to the TOWARD convention exactly as the 2D renderer does it.
    pub storm_motion: Option<StormMotion>,
    /// The same table the pane is painted with (`palettes::table_for`).
    pub color_table: &'a color_tables::ColorTable,
    /// Units and formatting for the readout, from the product registry.
    pub domain: product_engine::DisplayDomain,
}

/// Cross-section state: the line, the armed placement mode, and the slice
/// build pipeline. One per application, shown in its own window.
#[derive(Default)]
pub struct XSection {
    /// The section window is on screen.
    pub open: bool,
    /// Clicks on radar panes place line endpoints instead of selecting panes.
    pub armed: bool,
    /// The section line, radar-local kilometres. `None` until placed.
    pub line: Option<SectionLine>,
    /// First endpoint of a line being placed, awaiting the second click.
    pending_first: Option<(f64, f64)>,
    /// Finished slice results arriving from the build worker.
    rx: Option<mpsc::Receiver<build::BuiltSlice>>,
    /// The newest finished build: slice values for the readout, image for the
    /// texture, provenance for the header.
    built: Option<build::BuiltSlice>,
    /// GPU texture of `built.image`.
    texture: Option<egui::TextureHandle>,
    /// What the in-flight or latest build was keyed on. A key mismatch is what
    /// schedules a rebuild — the same "same data, same picture" gating the
    /// panes use, extended with the line and the palette signature.
    key: Option<build::SliceKey>,
    /// Dealiased velocity tilts of the last volume touched, so an endpoint
    /// drag pays the dealias once per volume rather than once per frame.
    dealias: Option<build::DealiasMemo>,
    /// What fills the column between the flown beams. Defaults to
    /// [`render2d::xsection::SliceVerticalFill::Beams`] — the native picture,
    /// one band per beam — with the smooth reconstruction one click away.
    fill: render2d::xsection::SliceVerticalFill,
    /// One line of state for the window header.
    pub status: String,
}

impl XSection {
    /// Toolbar toggle. Arming starts a fresh line; disarming abandons a
    /// half-placed one.
    pub fn toggle_armed(&mut self) {
        self.armed = !self.armed;
        self.pending_first = None;
        if self.armed {
            self.open = true;
        }
    }

    /// Whether pane clicks currently belong to endpoint placement.
    pub fn wants_pane_clicks(&self) -> bool {
        self.armed
    }

    /// Take one placement click at a radar-local position. Returns true when
    /// the click was consumed. The first click sets A, the second sets B and
    /// disarms, so ordinary pane selection comes straight back.
    pub fn handle_pane_click(&mut self, world_km: (f64, f64)) -> bool {
        if !self.armed || !world_km.0.is_finite() || !world_km.1.is_finite() {
            return false;
        }
        match self.pending_first.take() {
            None => {
                self.pending_first = Some(world_km);
            }
            Some(first) => {
                self.line = Some(SectionLine {
                    a_km: first,
                    b_km: world_km,
                });
                self.armed = false;
                self.open = true;
            }
        }
        true
    }

    /// Drop the line and everything derived from it. Dropping the receiver
    /// also orphans any in-flight build: its send fails harmlessly and the
    /// pipeline is immediately free for the next line.
    pub fn clear_line(&mut self) {
        self.line = None;
        self.pending_first = None;
        self.built = None;
        self.texture = None;
        self.key = None;
        self.rx = None;
        self.status.clear();
    }

    /// Draw the section line and its draggable endpoints over one radar pane.
    /// Call after `draw_pane` for every visible pane; the endpoints register
    /// later than the pane's own interact region, which is what routes an
    /// endpoint drag to the endpoint instead of panning the camera.
    pub fn draw_pane_overlay(
        &mut self,
        ui: &mut egui::Ui,
        pane_index: usize,
        rect: egui::Rect,
        camera: analyst_runtime::Camera2D,
        viewport: analyst_runtime::ViewportMetrics,
    ) {
        line::draw_pane_overlay(self, ui, pane_index, rect, camera, viewport);
    }

    /// The cross-section window: drains finished builds, schedules the next
    /// one when anything it depends on changed, and draws the slice.
    pub fn window(&mut self, context: &egui::Context, input: &XSectionInput<'_>) {
        if !self.open {
            return;
        }
        build::drain_and_drive(self, context, input);
        let mut open = self.open;
        egui::Window::new("Cross Section")
            .open(&mut open)
            .default_size([780.0, 430.0])
            .show(context, |ui| {
                draw::draw_window_contents(self, ui, input);
            });
        if open { self.open = true } else { self.close() }
    }

    /// The window was dismissed. Closing is walking away from the tool, so
    /// the whole tool goes away: the placement cursor comes down (leaving
    /// `armed` set would keep stealing pane clicks, and every completed pair
    /// of them would place a new line and reopen the window) and the line
    /// with its A/B handles comes off the glass (2026-08-19 field report:
    /// "the x section points stay on after you exit the window"). A section
    /// is cheap to place again; a marker nothing on screen explains is not.
    fn close(&mut self) {
        self.open = false;
        self.armed = false;
        self.clear_line();
    }
}

/// The section line on the radar panes: drawing, placement feedback, and
/// finger-sized draggable endpoints.
mod line {
    use analyst_runtime::{Camera2D, ScreenPoint, ViewportMetrics, WorldPoint};
    use eframe::egui;

    use super::XSection;

    /// Endpoint hit target, screen points. Mobile is a standing requirement,
    /// so this stays above the 24-point floor a fingertip needs.
    pub(super) const HANDLE_HIT_POINTS: f32 = 28.0;
    /// Visible endpoint disc radius, screen points.
    const HANDLE_RADIUS: f32 = 7.0;
    /// The pane header height (`pane_canvas::HEADER_HEIGHT` is private; this
    /// mirrors it). The overlay never draws into the header strip.
    const HEADER_INSET: f32 = 26.0;

    const LINE_INK: egui::Color32 = egui::Color32::from_rgb(255, 214, 79);
    const LINE_HALO: egui::Color32 = egui::Color32::from_rgba_premultiplied(0, 0, 0, 160);
    const LABEL_INK: egui::Color32 = egui::Color32::from_rgb(240, 244, 247);

    /// The cross-section line, radar-local kilometres.
    #[derive(Clone, Copy, Debug, PartialEq)]
    pub struct SectionLine {
        pub a_km: (f64, f64),
        pub b_km: (f64, f64),
    }

    impl SectionLine {
        pub fn length_km(&self) -> f64 {
            (self.b_km.0 - self.a_km.0).hypot(self.b_km.1 - self.a_km.1)
        }

        pub fn set_endpoint(&mut self, which: usize, world_km: (f64, f64)) {
            if !world_km.0.is_finite() || !world_km.1.is_finite() {
                return;
            }
            if which == 0 {
                self.a_km = world_km;
            } else {
                self.b_km = world_km;
            }
        }
    }

    /// World kilometres to a screen position inside `rect`, through the same
    /// globe warp the pane painted its own overlays with. `None` off the
    /// globe.
    fn world_to_screen(
        world_km: (f64, f64),
        rect: egui::Rect,
        camera: Camera2D,
        viewport: ViewportMetrics,
    ) -> Option<egui::Pos2> {
        let blend =
            map_scene::projection::globe::blend_for_pane(camera.sanitized().km_per_point, viewport);
        let warped = map_scene::projection::globe::warp_world(
            WorldPoint::new(world_km.0, world_km.1),
            blend,
        )?;
        let screen = camera.world_to_screen(warped, viewport);
        Some(egui::pos2(rect.left() + screen.x, rect.top() + screen.y))
    }

    /// A screen position inside `rect` back to world kilometres. `None` off
    /// the globe, where an endpoint must refuse to land rather than jump to
    /// the limb.
    fn screen_to_world(
        position: egui::Pos2,
        rect: egui::Rect,
        camera: Camera2D,
        viewport: ViewportMetrics,
    ) -> Option<(f64, f64)> {
        let local = ScreenPoint::new(position.x - rect.left(), position.y - rect.top());
        let blend =
            map_scene::projection::globe::blend_for_pane(camera.sanitized().km_per_point, viewport);
        let world = map_scene::projection::globe::unwarp_world(
            camera.screen_to_world(local, viewport),
            blend,
        )?;
        Some((world.east_km, world.north_km))
    }

    /// Draw the line, the placement feedback, and the endpoint drag widgets
    /// over one pane. See the module docs of [`super`] for why the widget
    /// registration order makes endpoint drags and camera pans
    /// collision-free.
    pub(super) fn draw_pane_overlay(
        xs: &mut XSection,
        ui: &mut egui::Ui,
        pane_index: usize,
        rect: egui::Rect,
        camera: Camera2D,
        viewport: ViewportMetrics,
    ) {
        // Below the pane header, so the title and status stay readable.
        let body = egui::Rect::from_min_max(
            egui::pos2(rect.left(), (rect.top() + HEADER_INSET).min(rect.bottom())),
            rect.max,
        );
        let painter = ui.painter_at(body);

        // Placement feedback: the pending first point, and a rubber band to
        // the pointer where one exists (desktop sugar — the second tap
        // completes the line without it).
        if xs.armed {
            if let Some(first) = xs.pending_first
                && let Some(a) = world_to_screen(first, rect, camera, viewport)
            {
                if let Some(pointer) = ui.input(|input| input.pointer.hover_pos())
                    && body.contains(pointer)
                {
                    painter.line_segment([a, pointer], egui::Stroke::new(1.5, LINE_INK));
                }
                draw_handle(&painter, a, "A", false);
            }
            return;
        }

        let Some(section) = xs.line else {
            return;
        };
        let a = world_to_screen(section.a_km, rect, camera, viewport);
        let b = world_to_screen(section.b_km, rect, camera, viewport);
        if let (Some(a), Some(b)) = (a, b) {
            // Halo under ink, so the line reads on bright echo and dark map
            // alike.
            painter.line_segment([a, b], egui::Stroke::new(4.0, LINE_HALO));
            painter.line_segment([a, b], egui::Stroke::new(2.0, LINE_INK));
            let mid = egui::pos2((a.x + b.x) * 0.5, (a.y + b.y) * 0.5);
            painter.text(
                mid + egui::vec2(0.0, -10.0),
                egui::Align2::CENTER_BOTTOM,
                format!("{:.0} km", section.length_km()),
                egui::FontId::monospace(11.0),
                LABEL_INK,
            );
        }

        // Endpoint drag widgets, registered AFTER the pane's own interact
        // region so a press on a handle belongs to the handle.
        for (which, label, position) in [(0_usize, "A", a), (1_usize, "B", b)] {
            let Some(position) = position else {
                continue;
            };
            if !body.contains(position) {
                // The endpoint is off this pane; nothing to grab here.
                continue;
            }
            let hit = egui::Rect::from_center_size(
                position,
                egui::vec2(HANDLE_HIT_POINTS, HANDLE_HIT_POINTS),
            );
            let id = ui.id().with(("xsection-endpoint", pane_index, which));
            let response = ui.interact(hit, id, egui::Sense::click_and_drag());
            if response.dragged()
                && let Some(pointer) = response.interact_pointer_pos()
                && let Some(world) = screen_to_world(pointer, rect, camera, viewport)
                && let Some(section) = xs.line.as_mut()
            {
                section.set_endpoint(which, world);
            }
            if response.hovered() || response.dragged() {
                ui.ctx().set_cursor_icon(if response.dragged() {
                    egui::CursorIcon::Grabbing
                } else {
                    egui::CursorIcon::Grab
                });
            }
            draw_handle(
                &painter,
                position,
                label,
                response.hovered() || response.dragged(),
            );
        }
    }

    fn draw_handle(painter: &egui::Painter, position: egui::Pos2, label: &str, emphasized: bool) {
        let radius = if emphasized {
            HANDLE_RADIUS + 1.5
        } else {
            HANDLE_RADIUS
        };
        painter.circle_filled(
            position,
            radius,
            egui::Color32::from_rgba_premultiplied(0, 0, 0, 140),
        );
        painter.circle_stroke(position, radius, egui::Stroke::new(2.0, LINE_INK));
        painter.text(
            position + egui::vec2(0.0, -radius - 2.0),
            egui::Align2::CENTER_BOTTOM,
            label,
            egui::FontId::monospace(11.0),
            LABEL_INK,
        );
    }
}

/// Background slice builds: which volume to slice, when to rebuild, and the
/// worker that turns a volume plus a line into coloured pixels.
///
/// The gating mirrors the panes and the 3D explorer: a build is keyed on
/// everything that can change the picture — the volume's identity AND extent
/// (a live volume grows in place under one identity), the product, the
/// palette signature, the line, the storm motion — and a key mismatch with
/// one worker in flight at a time is what makes the slice follow a live
/// volume without ever stacking up stale work.
mod build {
    use std::sync::{Arc, mpsc};
    use std::thread;
    use std::time::Instant;

    use eframe::egui;
    use radar_core::{MomentGrid, MomentType, RadarVolume};
    use render2d::xsection::{
        InterpPolicy, SliceRequest, SliceSmoothing, SliceVerticalFill, SliceVolume, sample_slice,
    };

    use super::{XSection, XSectionInput};

    /// Slice raster size. 640 x 320 samples a 15-tilt volume in ~2.4 ms
    /// (measured, `render2d::xsection` real-volume test), so a live volume
    /// and an endpoint drag both stay fluid; the window scales the texture to
    /// fit.
    pub(super) const SLICE_WIDTH: usize = 640;
    pub(super) const SLICE_HEIGHT: usize = 320;
    /// Top of the slice, metres above the radar — the 3D explorer's box top,
    /// so the two vertical tools agree about what "the top" is.
    pub(super) const SLICE_TOP_M: f32 = 18_000.0;

    /// How far back a deeper volume may be pulled from when the displayed one
    /// is still filling. The 3D explorer's rationale: two arrived tilts are
    /// not a storm, and the volume one scan back is at most this much older.
    const MAX_LOOKBACK_SECONDS: i64 = 900;

    /// One volume the slice may be built from. Same contract as the 3D
    /// explorer's candidate list: oldest first, exactly one `displayed`.
    pub struct XsCandidate<'a> {
        pub volume: &'a Arc<RadarVolume>,
        pub displayed: bool,
    }

    /// Everything a finished build carries back to the UI thread.
    pub(super) struct BuiltSlice {
        pub slice: render2d::xsection::Slice,
        pub image: egui::ColorImage,
        pub tilts: usize,
        pub build_ms: f32,
        pub provenance: String,
        pub dealias: Option<DealiasMemo>,
        /// Which vertical fill this picture is, so the texture is filtered
        /// the way the picture wants: a beams slice is a grid of measured
        /// bands and must be magnified with hard edges, a smooth slice is a
        /// continuous field and may be interpolated.
        pub fill: SliceVerticalFill,
    }

    /// What the last build was keyed on. Quantized so float jitter cannot
    /// spin rebuilds, exact enough that any visible change re-keys.
    #[derive(Clone, Debug, Eq, PartialEq)]
    pub(super) struct SliceKey {
        site: String,
        volume_time_ms: i64,
        cuts: usize,
        radials: usize,
        product: String,
        palette: u64,
        /// Line endpoints in whole metres.
        line_m: [i64; 4],
        /// Storm motion in tenths of a degree and tenths of a m/s.
        storm: Option<(i32, i32)>,
        dealiased: bool,
        /// Beams or smooth. In the key because the toggle must repaint: it
        /// changes the picture without changing the volume, the line or the
        /// palette, and nothing else here would notice.
        fill: SliceVerticalFill,
    }

    /// Dealiased velocity tilts of one volume, indexed by cut. Costs on the
    /// order of 100 ms for a full volume, so it is paid once per volume
    /// extent and shared with every subsequent build — which is what keeps an
    /// endpoint drag on DVEL fluid.
    #[derive(Clone)]
    pub(super) struct DealiasMemo {
        stamp: (String, i64, usize, usize),
        grids: Arc<Vec<Option<MomentGrid>>>,
    }

    fn volume_stamp(volume: &RadarVolume) -> (String, i64, usize, usize) {
        (
            volume.site.id.clone(),
            volume.volume_time.timestamp_millis(),
            volume.cuts.len(),
            volume.cuts.iter().map(|cut| cut.radials.len()).sum(),
        )
    }

    /// Distinct commanded tilts carrying `moment`, counting split-cut legs
    /// and SAILS revisits of one elevation as one tilt. Clustered by gap
    /// rather than bucketed, because the two legs of one tilt (0.48° and
    /// 0.52°) can straddle any fixed bucket boundary.
    pub(super) fn tilt_count(volume: &RadarVolume, moment: &MomentType) -> usize {
        let mut elevations: Vec<f32> = volume
            .cuts
            .iter()
            .filter(|cut| cut.moments.contains_key(moment))
            .map(|cut| cut.elevation_deg)
            .collect();
        elevations.sort_by(|a, b| a.total_cmp(b));
        let mut tilts = 0;
        let mut previous = f32::NEG_INFINITY;
        for elevation in elevations {
            // The same 0.2° rule the sampler's split-cut merge uses: WSR-88D
            // VCPs space distinct tilts >= 0.4° apart.
            if elevation - previous >= 0.2 {
                tilts += 1;
            }
            previous = elevation;
        }
        tilts
    }

    /// Which candidate to slice. Anchored at the displayed frame (never the
    /// operator's future); a same-site volume within the lookback window wins
    /// only by carrying strictly more tilts of the moment — the deepest
    /// recent picture, exactly the 3D explorer's rule in miniature.
    pub(super) fn choose_volume<'a>(
        candidates: &'a [XsCandidate<'a>],
        moment: &MomentType,
    ) -> Option<&'a Arc<RadarVolume>> {
        let anchor = candidates
            .iter()
            .find(|candidate| candidate.displayed)
            .or_else(|| {
                candidates
                    .iter()
                    .max_by_key(|candidate| candidate.volume.volume_time)
            })?;
        let mut best: Option<(usize, i64, &'a Arc<RadarVolume>)> = None;
        for candidate in candidates {
            if !candidate
                .volume
                .site
                .id
                .eq_ignore_ascii_case(&anchor.volume.site.id)
            {
                continue;
            }
            let age = (anchor.volume.volume_time - candidate.volume.volume_time).num_seconds();
            if !(0..=MAX_LOOKBACK_SECONDS).contains(&age) {
                continue;
            }
            let tilts = tilt_count(candidate.volume, moment);
            if tilts == 0 {
                continue;
            }
            let better = match &best {
                None => true,
                Some((best_tilts, best_age, _)) => {
                    tilts > *best_tilts || (tilts == *best_tilts && age < *best_age)
                }
            };
            if better {
                best = Some((tilts, age, candidate.volume));
            }
        }
        best.map(|(_, _, volume)| volume)
    }

    /// The interpolation guard a moment needs when it is resampled vertically
    /// — the same mapping the 3D explorer uses.
    pub(super) fn interp_policy(moment: &MomentType) -> InterpPolicy {
        match moment {
            MomentType::Velocity => InterpPolicy::VelocityGuard,
            MomentType::CorrelationCoefficient => InterpPolicy::CcGuard,
            _ => InterpPolicy::LinearAngle,
        }
    }

    fn slice_key(
        volume: &RadarVolume,
        input: &XSectionInput<'_>,
        line: super::SectionLine,
        fill: SliceVerticalFill,
    ) -> SliceKey {
        let stamp = volume_stamp(volume);
        SliceKey {
            site: stamp.0,
            volume_time_ms: stamp.1,
            cuts: stamp.2,
            radials: stamp.3,
            product: input.product_label.clone(),
            palette: input.color_table.signature(),
            line_m: [
                (line.a_km.0 * 1000.0).round() as i64,
                (line.a_km.1 * 1000.0).round() as i64,
                (line.b_km.0 * 1000.0).round() as i64,
                (line.b_km.1 * 1000.0).round() as i64,
            ],
            storm: input.storm_motion.map(|motion| {
                (
                    (motion.direction_deg * 10.0).round() as i32,
                    (motion.speed_mps * 10.0).round() as i32,
                )
            }),
            dealiased: input.uses_dealiased_velocity,
            fill,
        }
    }

    /// Drain a finished build, then start the next one if anything changed.
    /// Called once per frame while the window is open.
    pub(super) fn drain_and_drive(
        xs: &mut XSection,
        context: &egui::Context,
        input: &XSectionInput<'_>,
    ) {
        // Drain first, so a finished slice lands on the frame it arrives.
        match xs.rx.as_ref().map(|receiver| receiver.try_recv()) {
            Some(Ok(built)) => {
                xs.rx = None;
                xs.dealias = built.dealias.clone().or_else(|| xs.dealias.take());
                xs.status = format!(
                    "{} · {} tilts · {} · {:.1} ms",
                    built.provenance,
                    built.tilts,
                    fill_label(built.fill),
                    built.build_ms
                );
                // NEAREST for the beams picture: the slice raster is one
                // sample per beam band, and magnifying it with a linear
                // filter would smear the band edges back into the gradient
                // the beams fill exists to remove.
                let options = match built.fill {
                    SliceVerticalFill::Beams => egui::TextureOptions::NEAREST,
                    SliceVerticalFill::Interpolated => egui::TextureOptions::LINEAR,
                };
                match &mut xs.texture {
                    Some(texture) => texture.set(built.image.clone(), options),
                    None => {
                        xs.texture = Some(context.load_texture(
                            "xsection-slice",
                            built.image.clone(),
                            options,
                        ));
                    }
                }
                xs.built = Some(built);
            }
            Some(Err(mpsc::TryRecvError::Disconnected)) => {
                // The worker died without a result: the volume had no
                // sliceable tilt, or the build panicked. Release the pipeline
                // — leaving `rx` occupied would show "sampling…" forever and
                // block every future build — and keep the key, so the same
                // doomed build is not respawned every frame; any real change
                // re-keys and tries again.
                xs.rx = None;
                xs.status = "no slice: this volume gave nothing to sample".to_owned();
            }
            Some(Err(mpsc::TryRecvError::Empty)) | None => {}
        }

        let Some(line) = xs.line else {
            return;
        };
        if xs.rx.is_some() {
            // One build at a time: when it lands, the key comparison below
            // runs again and schedules the next one. Self-pacing, like the
            // panes.
            return;
        }
        let Some(volume) = choose_volume(input.candidates, &input.moment) else {
            if xs.built.is_none() {
                xs.status = format!("no volume carries {}", input.moment);
            }
            return;
        };
        let fill = xs.fill;
        let key = slice_key(volume, input, line, fill);
        if xs.key.as_ref() == Some(&key) {
            return;
        }
        xs.key = Some(key);

        let (sender, receiver) = mpsc::channel();
        xs.rx = Some(receiver);
        let context = context.clone();
        let job = BuildJob {
            volume: Arc::clone(volume),
            moment: input.moment.clone(),
            table: input.color_table.clone(),
            storm_motion: input.storm_motion,
            dealiased: input.uses_dealiased_velocity,
            memo: xs.dealias.clone(),
            request: SliceRequest {
                start_km: (line.a_km.0 as f32, line.a_km.1 as f32),
                end_km: (line.b_km.0 as f32, line.b_km.1 as f32),
                width: SLICE_WIDTH,
                height: SLICE_HEIGHT,
                top_m: SLICE_TOP_M,
            },
            fill,
        };

        thread::spawn(move || {
            if let Some(built) = build_slice(job) {
                let _ = sender.send(built);
            }
            context.request_repaint();
        });
    }

    /// Everything one background build owns. A struct rather than eight
    /// arguments: the worker takes the whole job across the thread boundary,
    /// so the job is the natural unit.
    pub(super) struct BuildJob {
        volume: Arc<RadarVolume>,
        moment: MomentType,
        table: color_tables::ColorTable,
        storm_motion: Option<render2d::StormMotion>,
        dealiased: bool,
        memo: Option<DealiasMemo>,
        request: SliceRequest,
        fill: SliceVerticalFill,
    }

    /// The worker body: dealias if the product needs it (memoised per volume
    /// extent), sample, apply storm motion, colorize through the pane's
    /// table.
    fn build_slice(job: BuildJob) -> Option<BuiltSlice> {
        let BuildJob {
            volume,
            moment,
            table,
            storm_motion,
            dealiased,
            memo,
            request,
            fill,
        } = job;
        let started = Instant::now();
        let stamp = volume_stamp(&volume);

        let memo = if dealiased {
            Some(match memo.filter(|memo| memo.stamp == stamp) {
                Some(memo) => memo,
                None => DealiasMemo {
                    stamp: stamp.clone(),
                    grids: Arc::new(
                        volume
                            .cuts
                            .iter()
                            .map(|cut| {
                                cut.moments
                                    .get(&MomentType::Velocity)
                                    .map(|grid| render2d::dealias_velocity_grid(cut, grid))
                            })
                            .collect(),
                    ),
                },
            })
        } else {
            None
        };

        let prepared = match &memo {
            Some(memo) => SliceVolume::from_indexed_grids(
                &volume,
                memo.grids
                    .iter()
                    .enumerate()
                    .filter_map(|(cut_index, grid)| grid.as_ref().map(|grid| (cut_index, grid))),
            ),
            None => SliceVolume::from_volume(&volume, &moment),
        };
        let tilts = prepared.tilt_count();
        let mut slice = sample_slice(
            &prepared,
            &request,
            interp_policy(&moment),
            SliceSmoothing::Native,
            fill,
        )?;

        // Storm-relative: subtract the motion component along each column's
        // radial — the same azimuth-only projection the 2D renderer applies,
        // so the slice and the pane say the same number about the same gate.
        if let Some(motion) = storm_motion {
            for x in 0..slice.width {
                let azimuth = slice.azimuth_deg_at_col(x);
                for y in 0..slice.height {
                    let index = y * slice.width + x;
                    let value = slice.values[index];
                    if value.is_finite() {
                        slice.values[index] =
                            render2d::storm_relative_velocity_mps(value, azimuth, motion);
                    }
                }
            }
        }

        let image = colorize(&slice, &table);
        let build_ms = started.elapsed().as_secs_f32() * 1_000.0;
        let provenance = format!(
            "{} {}",
            volume.site.id,
            volume.volume_time.format("%H:%M:%SZ")
        );
        Some(BuiltSlice {
            slice,
            image,
            tilts,
            build_ms,
            provenance,
            dealias: memo,
            fill,
        })
    }

    /// How a fill mode is named in the header and on its button.
    pub(super) fn fill_label(fill: SliceVerticalFill) -> &'static str {
        match fill {
            SliceVerticalFill::Beams => "beams",
            SliceVerticalFill::Interpolated => "smooth",
        }
    }

    /// Slice values through the pane's colour table. NaN — the radar saw
    /// nothing — stays fully transparent, so the beam-gap wedges and the cone
    /// of silence read as the window background, not as a colour.
    pub(super) fn colorize(
        slice: &render2d::xsection::Slice,
        table: &color_tables::ColorTable,
    ) -> egui::ColorImage {
        let pixels = slice
            .values
            .iter()
            .map(|value| {
                if !value.is_finite() {
                    return egui::Color32::TRANSPARENT;
                }
                let color = table.sample(*value);
                egui::Color32::from_rgba_unmultiplied(color.r, color.g, color.b, color.a)
            })
            .collect();
        egui::ColorImage::new([slice.width, slice.height], pixels)
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use radar_core::{ElevationCut, GateRange, MomentStorage, RadarSite};

        fn volume(site: &str, seconds: i64, elevations: &[f32]) -> Arc<RadarVolume> {
            let mut volume = RadarVolume::new(
                RadarSite::new(site),
                chrono::DateTime::from_timestamp(seconds, 0).expect("valid time"),
            );
            for &elevation in elevations {
                let mut cut = ElevationCut::new(elevation, None);
                let gate_range = GateRange {
                    first_gate_m: 0,
                    gate_spacing_m: 1_000,
                    gate_count: 4,
                };
                cut.moments.insert(
                    MomentType::Reflectivity,
                    MomentGrid {
                        moment: MomentType::Reflectivity,
                        gate_range,
                        scale: 1.0,
                        offset: 0.0,
                        nodata: None,
                        range_folded: None,
                        radial_indices: vec![0],
                        storage: MomentStorage::F32(vec![30.0; 4]),
                    },
                );
                volume.cuts.push(cut);
            }
            Arc::new(volume)
        }

        #[test]
        fn split_cut_legs_count_as_one_tilt() {
            let volume = volume("KTLX", 0, &[0.48, 0.52, 0.9, 1.3]);
            assert_eq!(tilt_count(&volume, &MomentType::Reflectivity), 3);
            assert_eq!(tilt_count(&volume, &MomentType::Velocity), 0);
        }

        #[test]
        fn the_deepest_recent_same_site_volume_wins() {
            let shallow_now = volume("KTLX", 1_000, &[0.5]);
            let deep_recent = volume("KTLX", 700, &[0.5, 0.9, 1.3, 2.4, 3.1]);
            let other_site = volume("KICT", 990, &[0.5, 0.9, 1.3, 2.4, 3.1, 4.0]);
            let candidates = [
                XsCandidate {
                    volume: &deep_recent,
                    displayed: false,
                },
                XsCandidate {
                    volume: &other_site,
                    displayed: false,
                },
                XsCandidate {
                    volume: &shallow_now,
                    displayed: true,
                },
            ];
            let chosen =
                choose_volume(&candidates, &MomentType::Reflectivity).expect("a volume is chosen");
            assert!(
                Arc::ptr_eq(chosen, &deep_recent),
                "the deeper same-site volume within lookback wins over the fragment"
            );
        }

        #[test]
        fn a_volume_from_the_operators_future_is_never_used() {
            let displayed = volume("KTLX", 500, &[0.5, 0.9]);
            let future = volume("KTLX", 800, &[0.5, 0.9, 1.3, 2.4]);
            let candidates = [
                XsCandidate {
                    volume: &displayed,
                    displayed: true,
                },
                XsCandidate {
                    volume: &future,
                    displayed: false,
                },
            ];
            let chosen =
                choose_volume(&candidates, &MomentType::Reflectivity).expect("a volume is chosen");
            assert!(
                Arc::ptr_eq(chosen, &displayed),
                "scrubbing back must not build tomorrow's slice"
            );
        }

        #[test]
        fn equal_depth_prefers_the_volume_on_screen() {
            let displayed = volume("KTLX", 1_000, &[0.5, 0.9, 1.3]);
            let older = volume("KTLX", 700, &[0.5, 0.9, 1.3]);
            let candidates = [
                XsCandidate {
                    volume: &older,
                    displayed: false,
                },
                XsCandidate {
                    volume: &displayed,
                    displayed: true,
                },
            ];
            let chosen =
                choose_volume(&candidates, &MomentType::Reflectivity).expect("a volume is chosen");
            assert!(Arc::ptr_eq(chosen, &displayed));
        }

        #[test]
        fn the_policy_guards_follow_the_moment() {
            // `InterpPolicy` derives PartialEq but not Debug, so plain asserts.
            assert!(interp_policy(&MomentType::Velocity) == InterpPolicy::VelocityGuard);
            assert!(interp_policy(&MomentType::CorrelationCoefficient) == InterpPolicy::CcGuard);
            assert!(interp_policy(&MomentType::Reflectivity) == InterpPolicy::LinearAngle);
        }

        /// A worker that dies without sending (unsliceable volume, panic)
        /// drops its channel. The drain must release the pipeline — before
        /// this was pinned, the receiver stayed occupied, the header said
        /// "sampling…" forever and no rebuild could ever start.
        #[test]
        fn a_dead_worker_releases_the_pipeline() {
            let (sender, receiver) = mpsc::channel::<BuiltSlice>();
            drop(sender);
            let mut xs = super::super::XSection {
                line: Some(super::super::SectionLine {
                    a_km: (0.0, 0.0),
                    b_km: (10.0, 0.0),
                }),
                rx: Some(receiver),
                ..Default::default()
            };

            let context = egui::Context::default();
            let tables = color_tables::ColorTableSet::default();
            let domain = product_engine::ProductRegistry::builtin()
                .get("REF")
                .expect("REF exists")
                .domain;
            let input = XSectionInput {
                candidates: &[],
                moment: MomentType::Reflectivity,
                product_label: "REF".to_owned(),
                uses_dealiased_velocity: false,
                storm_motion: None,
                color_table: tables.for_family(color_tables::ColorTableFamily::Reflectivity),
                domain,
            };
            drain_and_drive(&mut xs, &context, &input);
            assert!(
                xs.rx.is_none(),
                "a dead worker must not hold the build pipeline forever"
            );
            assert!(!xs.status.is_empty(), "the header states what happened");
        }

        /// The toggle must repaint. Nothing else in the key moves when the
        /// analyst switches Beams to Smooth — same volume, same line, same
        /// palette — so if the fill were missing from the key the button
        /// would do nothing at all.
        #[test]
        fn the_fill_mode_is_part_of_the_rebuild_key() {
            let volume = volume("KUEX", 1_700_000_000, &[0.5, 1.5, 2.4]);
            let tables = color_tables::ColorTableSet::default();
            let domain = product_engine::ProductRegistry::builtin()
                .get("REF")
                .expect("REF exists")
                .domain;
            let candidates = [XsCandidate {
                volume: &volume,
                displayed: true,
            }];
            let input = XSectionInput {
                candidates: &candidates,
                moment: MomentType::Reflectivity,
                product_label: "REF".to_owned(),
                uses_dealiased_velocity: false,
                storm_motion: None,
                color_table: tables.for_family(color_tables::ColorTableFamily::Reflectivity),
                domain,
            };
            let line = super::super::SectionLine {
                a_km: (-20.0, 5.0),
                b_km: (40.0, 12.0),
            };
            let beams = slice_key(&volume, &input, line, SliceVerticalFill::Beams);
            let smooth = slice_key(&volume, &input, line, SliceVerticalFill::Interpolated);
            assert_ne!(beams, smooth, "toggling the fill must schedule a rebuild");
            assert_eq!(
                beams,
                slice_key(&volume, &input, line, SliceVerticalFill::Beams),
                "nothing else about the key drifted"
            );
        }

        #[test]
        fn the_section_starts_in_the_native_beams_fill() {
            assert_eq!(XSection::default().fill, SliceVerticalFill::Beams);
            assert_eq!(fill_label(SliceVerticalFill::Beams), "beams");
            assert_eq!(fill_label(SliceVerticalFill::Interpolated), "smooth");
        }

        #[test]
        fn absent_cells_colorize_to_full_transparency() {
            let slice = render2d::xsection::Slice {
                width: 2,
                height: 1,
                top_m: 1_000.0,
                length_m: 1_000.0,
                start_km: (0.0, 0.0),
                end_km: (1.0, 0.0),
                values: vec![f32::NAN, 45.0],
            };
            let tables = color_tables::ColorTableSet::default();
            let image = colorize(
                &slice,
                tables.for_family(color_tables::ColorTableFamily::Reflectivity),
            );
            assert_eq!(image.pixels[0], egui::Color32::TRANSPARENT);
            assert_ne!(image.pixels[1], egui::Color32::TRANSPARENT);
        }
    }
}

/// The section window: the slice texture with a height ladder in km ARL, a
/// distance axis in km, and the value readout under the cursor.
///
/// Absent cells are transparent in the texture, so the beam-gap wedges, the
/// cone of silence and everything below the lowest beam read as the window's
/// own dark ground — the radar saw nothing there and the window paints
/// nothing there.
mod draw {
    use eframe::egui;
    use render2d::xsection::SliceVerticalFill;

    use super::{XSection, XSectionInput, build};

    /// The fill toggle's buttons. Height is the touch floor: the section
    /// window is used on a tablet in the field, and a mode switch that a
    /// fingertip cannot hit is a mode switch that does not exist. Plain
    /// `egui::Button`s, so the application's theme restyles them with
    /// everything else.
    const TOGGLE_HEIGHT: f32 = 26.0;
    const TOGGLE_WIDTH: f32 = 62.0;

    /// Margins around the plot, screen points.
    const AXIS_LEFT: f32 = 46.0;
    const AXIS_BOTTOM: f32 = 22.0;
    const PAD_TOP: f32 = 8.0;
    const PAD_RIGHT: f32 = 12.0;

    /// Height ladder rung spacing, metres.
    const HEIGHT_TICK_M: f32 = 2_000.0;

    const CANVAS_GROUND: egui::Color32 = egui::Color32::from_rgb(10, 13, 17);
    const GRID_INK: egui::Color32 = egui::Color32::from_rgba_premultiplied(28, 30, 34, 40);
    const AXIS_INK: egui::Color32 = egui::Color32::from_rgb(166, 184, 196);
    const READOUT_INK: egui::Color32 = egui::Color32::from_rgb(239, 243, 246);

    pub(super) fn draw_window_contents(
        xs: &mut XSection,
        ui: &mut egui::Ui,
        input: &XSectionInput<'_>,
    ) {
        ui.horizontal_wrapped(|ui| {
            ui.strong(format!("{} slice", input.product_label));
            if xs.rx.is_some() {
                ui.weak("sampling…");
            } else if !xs.status.is_empty() {
                ui.weak(&xs.status);
            }
            if ui
                .button(if xs.armed {
                    "Click 2 points…"
                } else {
                    "Draw line"
                })
                .on_hover_text(
                    "Arm, then click two points on a radar pane. \
                     Drag the A and B handles afterwards to adjust.",
                )
                .clicked()
            {
                xs.toggle_armed();
            }
            if xs.line.is_some() && ui.button("Clear").clicked() {
                xs.clear_line();
            }
            ui.separator();
            // What fills the column between the flown beams. Beams is the
            // default and the native picture; Smooth keeps the interpolated
            // reconstruction one click away. Changing it re-keys the build,
            // so the slice repaints without any other input changing.
            for fill in [SliceVerticalFill::Beams, SliceVerticalFill::Interpolated] {
                let selected = xs.fill == fill;
                let (label, hover) = match fill {
                    SliceVerticalFill::Beams => (
                        "Beams",
                        "Native: every pixel is the value of the beam that covered it. \
                         Discrete beams with hard edges, and empty air where no beam looked.",
                    ),
                    SliceVerticalFill::Interpolated => (
                        "Smooth",
                        "Interpolated: linear in elevation angle between the beams \
                         (Zhang, Howard & Gourley 2005). Continuous, but the smoothness \
                         between beams is inference, not measurement.",
                    ),
                };
                let response = ui
                    .add_sized(
                        [TOGGLE_WIDTH, TOGGLE_HEIGHT],
                        egui::Button::selectable(selected, label),
                    )
                    .on_hover_text(hover);
                if response.clicked() && !selected {
                    xs.fill = fill;
                }
            }
        });
        ui.separator();

        let Some(section) = xs.line else {
            ui.weak("No line yet: press Draw line, then click two points on a radar pane.");
            return;
        };

        let size = ui.available_size();
        let (rect, response) = ui.allocate_exact_size(
            egui::vec2(size.x.max(240.0), size.y.max(140.0)),
            egui::Sense::click_and_drag(),
        );
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 0.0, CANVAS_GROUND);

        let plot = egui::Rect::from_min_max(
            egui::pos2(rect.left() + AXIS_LEFT, rect.top() + PAD_TOP),
            egui::pos2(rect.right() - PAD_RIGHT, rect.bottom() - AXIS_BOTTOM),
        );
        if plot.width() < 40.0 || plot.height() < 40.0 {
            return;
        }

        if let Some(texture) = &xs.texture {
            painter.image(
                texture.id(),
                plot,
                egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                egui::Color32::WHITE,
            );
        }

        let top_m = xs
            .built
            .as_ref()
            .map(|built| built.slice.top_m)
            .unwrap_or(build::SLICE_TOP_M);

        // Height ladder, km ARL. Rungs across the plot so a feature's
        // altitude can be read without leaving it.
        for z_m in height_ticks_m(top_m) {
            let y = plot.bottom() - z_m / top_m * plot.height();
            painter.line_segment(
                [egui::pos2(plot.left(), y), egui::pos2(plot.right(), y)],
                egui::Stroke::new(1.0, GRID_INK),
            );
            painter.text(
                egui::pos2(plot.left() - 6.0, y),
                egui::Align2::RIGHT_CENTER,
                format!("{:.0}", z_m / 1000.0),
                egui::FontId::monospace(10.0),
                AXIS_INK,
            );
        }
        painter.text(
            egui::pos2(rect.left() + 4.0, rect.top() + 2.0),
            egui::Align2::LEFT_TOP,
            "km ARL",
            egui::FontId::monospace(10.0),
            AXIS_INK,
        );

        // Distance axis, km from A.
        let length_km = section.length_km() as f32;
        if length_km > 0.0 {
            let step_km = nice_distance_step_km(length_km);
            let mut d_km = 0.0f32;
            while d_km <= length_km + 0.01 {
                let x = plot.left() + (d_km / length_km).min(1.0) * plot.width();
                painter.line_segment(
                    [
                        egui::pos2(x, plot.bottom()),
                        egui::pos2(x, plot.bottom() + 4.0),
                    ],
                    egui::Stroke::new(1.0, AXIS_INK),
                );
                painter.text(
                    egui::pos2(x, plot.bottom() + 6.0),
                    egui::Align2::CENTER_TOP,
                    format!("{d_km:.0}"),
                    egui::FontId::monospace(10.0),
                    AXIS_INK,
                );
                d_km += step_km;
            }
        }
        // Which end is which, matching the pane labels.
        painter.text(
            egui::pos2(plot.left(), rect.bottom() - 2.0),
            egui::Align2::LEFT_BOTTOM,
            "A",
            egui::FontId::monospace(10.0),
            READOUT_INK,
        );
        painter.text(
            egui::pos2(plot.right(), rect.bottom() - 2.0),
            egui::Align2::RIGHT_BOTTOM,
            "B  km",
            egui::FontId::monospace(10.0),
            READOUT_INK,
        );
        painter.rect_stroke(
            plot,
            0.0,
            egui::Stroke::new(1.0, egui::Color32::from_rgb(45, 57, 67)),
            egui::StrokeKind::Middle,
        );

        // The readout: complete without hover on touch — a press or drag on
        // the slice reports the same numbers a mouse hover does.
        let pointer = response
            .hover_pos()
            .or_else(|| response.interact_pointer_pos());
        if let (Some(pointer), Some(built)) = (pointer, xs.built.as_ref())
            && plot.contains(pointer)
        {
            let slice = &built.slice;
            if let Some((column, row)) = cell_for_pointer(plot, pointer, slice.width, slice.height)
            {
                // A thin crosshair so the reported cell is unambiguous.
                painter.line_segment(
                    [
                        egui::pos2(pointer.x, plot.top()),
                        egui::pos2(pointer.x, plot.bottom()),
                    ],
                    egui::Stroke::new(1.0, egui::Color32::from_rgba_premultiplied(70, 74, 80, 90)),
                );
                let text = format_readout(
                    slice.value_at(column, row),
                    &input.domain,
                    f64::from(slice.height_m_at_row(row)),
                    f64::from(slice.distance_m_at_col(column)),
                );
                let anchor = egui::pos2(plot.left() + 8.0, plot.bottom() - 8.0);
                let galley =
                    painter.layout_no_wrap(text, egui::FontId::monospace(12.0), READOUT_INK);
                let plate = egui::Rect::from_min_size(
                    egui::pos2(anchor.x - 4.0, anchor.y - galley.size().y - 2.0),
                    galley.size() + egui::vec2(8.0, 6.0),
                );
                painter.rect_filled(plate, 2.0, egui::Color32::from_black_alpha(160));
                painter.galley(
                    egui::pos2(anchor.x, anchor.y - galley.size().y + 1.0),
                    galley,
                    READOUT_INK,
                );
            }
        }
    }

    /// Height rungs from one spacing up to (not including) the top.
    pub(super) fn height_ticks_m(top_m: f32) -> Vec<f32> {
        let mut ticks = Vec::new();
        let mut z = HEIGHT_TICK_M;
        while z < top_m {
            ticks.push(z);
            z += HEIGHT_TICK_M;
        }
        ticks
    }

    /// A distance tick spacing that yields a handful of readable labels at
    /// any line length.
    pub(super) fn nice_distance_step_km(length_km: f32) -> f32 {
        for step in [1.0f32, 2.0, 5.0, 10.0, 20.0, 25.0, 50.0, 100.0, 200.0] {
            if length_km / step <= 8.0 {
                return step;
            }
        }
        500.0
    }

    /// The slice cell under a pointer inside the plot rectangle.
    pub(super) fn cell_for_pointer(
        plot: egui::Rect,
        pointer: egui::Pos2,
        width: usize,
        height: usize,
    ) -> Option<(usize, usize)> {
        if width < 2 || height < 2 || plot.width() <= 0.0 || plot.height() <= 0.0 {
            return None;
        }
        let fx = ((pointer.x - plot.left()) / plot.width()).clamp(0.0, 1.0);
        let fy = ((pointer.y - plot.top()) / plot.height()).clamp(0.0, 1.0);
        Some((
            (fx * (width - 1) as f32).round() as usize,
            (fy * (height - 1) as f32).round() as usize,
        ))
    }

    /// "47.5 dBZ · 8.2 km ARL · 31.4 km" — or an honest "no data" where the
    /// radar saw nothing, which is different from zero.
    pub(super) fn format_readout(
        value: f32,
        domain: &product_engine::DisplayDomain,
        height_m: f64,
        distance_m: f64,
    ) -> String {
        let position = format!(
            "{:.1} km ARL · {:.1} km",
            height_m / 1000.0,
            distance_m / 1000.0
        );
        if value.is_finite() {
            let unit = domain.display_unit.label();
            if unit.is_empty() {
                format!("{} · {position}", domain.format_display_value(value))
            } else {
                format!("{} {unit} · {position}", domain.format_display_value(value))
            }
        } else {
            format!("no data · {position}")
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn the_height_ladder_spans_the_slice_without_touching_its_edges() {
            let ticks = height_ticks_m(18_000.0);
            assert_eq!(ticks.first().copied(), Some(2_000.0));
            assert_eq!(ticks.last().copied(), Some(16_000.0));
            assert_eq!(ticks.len(), 8);
        }

        #[test]
        fn distance_steps_stay_readable_at_every_line_length() {
            for (length, expected) in [(6.0, 1.0), (30.0, 5.0), (80.0, 10.0), (400.0, 50.0)] {
                assert_eq!(nice_distance_step_km(length), expected, "{length} km line");
            }
            // Never more than 9 labels, and always at least one.
            for length in [1.0f32, 12.0, 47.0, 133.0, 380.0, 900.0] {
                let step = nice_distance_step_km(length);
                let labels = (length / step).floor() + 1.0;
                assert!(labels <= 9.0, "{length} km: {labels} labels");
                assert!(labels >= 1.0);
            }
        }

        #[test]
        fn the_pointer_maps_to_the_cell_under_it() {
            let plot = egui::Rect::from_min_max(egui::pos2(10.0, 10.0), egui::pos2(110.0, 60.0));
            assert_eq!(
                cell_for_pointer(plot, egui::pos2(10.0, 10.0), 100, 50),
                Some((0, 0))
            );
            assert_eq!(
                cell_for_pointer(plot, egui::pos2(110.0, 60.0), 100, 50),
                Some((99, 49))
            );
            assert_eq!(
                cell_for_pointer(plot, egui::pos2(60.0, 35.0), 100, 50),
                Some((50, 25)),
                "the middle of the plot is the middle cell, halves rounding up"
            );
        }

        /// The fill toggle is a field control: a fingertip has to hit it.
        /// A const block, so shrinking the button refuses to even compile.
        #[test]
        fn the_fill_toggle_meets_the_touch_floor() {
            const {
                assert!(TOGGLE_HEIGHT >= 24.0);
                assert!(TOGGLE_WIDTH >= 24.0);
            }
        }

        #[test]
        fn the_readout_reports_absence_as_absence() {
            let domain = product_engine::ProductRegistry::builtin()
                .get("REF")
                .expect("REF exists")
                .domain;
            let text = format_readout(f32::NAN, &domain, 5_000.0, 12_000.0);
            assert!(text.starts_with("no data"), "{text}");
            assert!(text.contains("5.0 km ARL"), "{text}");
            let text = format_readout(47.5, &domain, 8_200.0, 31_400.0);
            assert!(text.contains("dBZ"), "{text}");
            assert!(text.contains("47.5"), "{text}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn two_armed_clicks_place_the_line_and_disarm() {
        let mut xs = XSection::default();
        assert!(
            !xs.handle_pane_click((10.0, 5.0)),
            "an unarmed click is not consumed"
        );
        xs.toggle_armed();
        assert!(xs.armed && xs.open, "arming opens the window");
        assert!(xs.handle_pane_click((10.0, 5.0)), "first click consumed");
        assert!(xs.line.is_none(), "one click is not a line yet");
        assert!(xs.handle_pane_click((40.0, -5.0)), "second click consumed");
        let line = xs.line.expect("two clicks place the line");
        assert_eq!(line.a_km, (10.0, 5.0));
        assert_eq!(line.b_km, (40.0, -5.0));
        assert!(!xs.armed, "placement disarms so pane clicks return");
        assert!(
            !xs.handle_pane_click((0.0, 0.0)),
            "after placement clicks pass through again"
        );
    }

    /// The field bugs of 2026-08-19, both halves: close the window while
    /// armed and every later pane click kept placing endpoints, each
    /// completed pair reopening the window with a line nobody asked for; and
    /// a PLACED line kept its A/B handles on the panes after the window was
    /// gone, with nothing on screen to remove them.
    #[test]
    fn closing_the_window_disarms_placement_and_takes_the_line_off_the_glass() {
        let mut xs = XSection::default();
        xs.toggle_armed();
        xs.handle_pane_click((10.0, 5.0));
        xs.close();
        assert!(!xs.open && !xs.armed, "closing put the cursor down");
        assert!(xs.pending_first.is_none(), "the half line went with it");
        assert!(
            !xs.handle_pane_click((40.0, -5.0)),
            "clicks after closing belong to the panes again"
        );
        assert!(xs.line.is_none(), "no line appeared from beyond the grave");

        // The placed-line half: exit the window with a finished section on
        // the map, and the section leaves with it.
        xs.toggle_armed();
        xs.handle_pane_click((10.0, 5.0));
        xs.handle_pane_click((40.0, -5.0));
        assert!(xs.line.is_some(), "two clicks placed a line");
        xs.close();
        assert!(
            xs.line.is_none(),
            "the line and its endpoints must come off the glass with the window"
        );
        assert!(xs.status.is_empty(), "no status for a section that is gone");
    }

    #[test]
    fn disarming_abandons_a_half_placed_line() {
        let mut xs = XSection::default();
        xs.toggle_armed();
        xs.handle_pane_click((10.0, 5.0));
        xs.toggle_armed();
        assert!(xs.pending_first.is_none(), "the half line is gone");
        xs.toggle_armed();
        xs.handle_pane_click((1.0, 1.0));
        xs.handle_pane_click((2.0, 2.0));
        let line = xs.line.expect("a fresh pair places cleanly");
        assert_eq!(line.a_km, (1.0, 1.0));
    }

    #[test]
    fn a_non_finite_click_places_nothing() {
        let mut xs = XSection::default();
        xs.toggle_armed();
        assert!(!xs.handle_pane_click((f64::NAN, 0.0)));
        assert!(xs.pending_first.is_none());
    }

    #[test]
    fn a_section_line_measures_its_own_length() {
        let line = SectionLine {
            a_km: (0.0, 0.0),
            b_km: (30.0, 40.0),
        };
        assert!((line.length_km() - 50.0).abs() < 1e-9);
    }

    #[test]
    fn endpoints_move_only_to_finite_positions() {
        let mut line = SectionLine {
            a_km: (0.0, 0.0),
            b_km: (10.0, 0.0),
        };
        line.set_endpoint(0, (5.0, 5.0));
        assert_eq!(line.a_km, (5.0, 5.0));
        line.set_endpoint(1, (f64::NAN, 2.0));
        assert_eq!(line.b_km, (10.0, 0.0), "NaN must not eat an endpoint");
    }

    #[test]
    fn the_endpoint_hit_target_meets_the_touch_floor() {
        // MOBILE IS A STANDING REQUIREMENT: >= 24 points for a fingertip.
        // A const block, so shrinking the handle refuses to even compile.
        const {
            assert!(line::HANDLE_HIT_POINTS >= 24.0);
        }
    }

    #[test]
    fn clearing_the_line_clears_everything_derived_from_it() {
        let mut xs = XSection::default();
        xs.toggle_armed();
        xs.handle_pane_click((0.0, 0.0));
        xs.handle_pane_click((30.0, 30.0));
        xs.status = "built".to_owned();
        xs.clear_line();
        assert!(xs.line.is_none());
        assert!(xs.built.is_none());
        assert!(xs.key.is_none());
        assert!(xs.status.is_empty());
    }
}
