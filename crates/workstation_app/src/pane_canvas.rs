use std::sync::Arc;

use analyst_runtime::{
    Camera2D, NavInput, PaneId, PaneLayout, ScreenPoint, TRACKPAD_POINTS_PER_NOTCH,
    ViewportMetrics, WheelNotches, WorldPoint, ZoomResponder,
};
use color_tables::hazards::{HAZARD_FILL_ALPHA, HAZARD_STROKE_WIDTH};
use eframe::egui;
use map_scene::gpu::{MapPaintCallback, TilePaintCallback};
use map_scene::{MapChrome, MapGeometry, RadarProjection, TileFrame};

use crate::hazards::PlacedHazard;

const PANE_GAP: f32 = 3.0;
const HEADER_HEIGHT: f32 = 26.0;
const RANGE_RINGS_KM: &[f64] = &[50.0, 100.0, 150.0, 200.0, 300.0, 400.0];

pub struct PaneTexture<'a> {
    pub handle: &'a egui::TextureHandle,
    pub camera: Camera2D,
    pub viewport: ViewportMetrics,
}

/// The analyst's navigation-speed settings, remapped so the tuned response
/// curves in `analyst_runtime::view` keep their shape with the user's numbers
/// in them. Exponents and time scales rather than raw rates because the
/// curves are exponential: `1.2^n` raised to `zoom_exp` is exactly
/// `user^n`, `KEY_ZOOM_RATE^(hold·dt·kzoom_exp)` is exactly `user^(hold·dt)`,
/// and `span·KEY_PAN_FRACTION·(dt·pan_scale)` is exactly `span·user·dt` - the
/// burst behaviour, clamps and anchor rules all survive untouched.
#[derive(Clone, Copy)]
pub struct NavTuning {
    /// Exponent on the wheel factor: `ln(user) / ln(ZOOM_PER_NOTCH)`.
    pub zoom_exp: f32,
    /// Multiplier on dt for the pan-only nav pass.
    pub pan_scale: f32,
    /// Multiplier on dt for the zoom-only nav pass.
    pub kzoom_exp: f32,
    /// Whether double-clicking a pane resets its camera to the home view.
    pub double_click_reset: bool,
}

impl Default for NavTuning {
    fn default() -> Self {
        // Identity: the tuned constants exactly as shipped.
        Self {
            zoom_exp: 1.0,
            pan_scale: 1.0,
            kzoom_exp: 1.0,
            double_click_reset: true,
        }
    }
}

/// When a radar site marker gets its identifier written beside it.
///
/// `Auto` is the shipped rule: active, hovered, or an uncluttered view.
/// `Never` writes no ids at all - the markers stay.
#[derive(Clone, Copy, Default, PartialEq)]
pub enum SiteLabelMode {
    #[default]
    Auto,
    Always,
    Never,
}

/// The retained map underlay for one pane, if the scene has geometry built for
/// the pane's current LOD. `projection` also drives the cursor's lat/lon, so
/// the readout uses the same transform the map was built with.
#[derive(Clone, Default)]
pub struct PaneMap {
    pub geometry: Option<Arc<MapGeometry>>,
    pub projection: Option<RadarProjection>,
    /// The raster tile underlay for this pane, when a provider is selected
    /// and the scene has something projected for it. `None` is the shipped
    /// behaviour, byte for byte: the vector basemap draws straight onto
    /// `chrome.canvas` and nothing here runs.
    pub tiles: Option<Arc<TileFrame>>,
    /// Paint-time colours for the chosen basemap look: the ground the pane
    /// clears to, and the ink and halo its place labels are drawn in.
    ///
    /// Travels beside the style rather than inside it because `MapStyle` is a
    /// geometry input and part of the geometry cache key - a background tweak
    /// must not throw away every retained vertex buffer. Without this the pane
    /// hard-coded one near-black ground and one light ink, so `Daylight`
    /// painted dark lines and dark label text onto a permanently dark pane and
    /// read as an empty screen. `Default` is `MapStylePreset::Slate`'s chrome,
    /// byte for byte the constants this pane used to carry.
    pub chrome: MapChrome,
    /// Radar sites already projected into world kilometres, so the paint pass
    /// only transforms points rather than projecting them.
    pub sites: Arc<[PlacedSite]>,
    /// When a site marker's identifier is written beside it.
    pub site_labels: SiteLabelMode,
    /// The site currently being displayed, drawn as selected.
    pub active_site: Option<String>,
    /// Warning polygons in force, already projected, least severe first.
    pub hazards: Arc<[PlacedHazard]>,
}

/// Everything drawn on top of the radar in screen space: the colour bar and
/// the value readout.
///
/// Screen space, so it never enters the map's geometry identity and a pan
/// cannot invalidate it.
pub struct PaneOverlay<'a> {
    pub legend: Option<&'a crate::legend::LegendLayout>,
    pub table: Option<&'a color_tables::ColorTable>,
    pub product_name: &'a str,
    pub badges: &'a [String],
    /// The formatted value under the cursor, from the previous frame.
    pub probe: Option<&'a str>,
}

/// A radar site at a known world position, ready to draw and hit-test.
#[derive(Clone, Debug)]
pub struct PlacedSite {
    pub id: String,
    pub world: WorldPoint,
}

/// Half-size of a site marker in screen points.
const SITE_MARKER_HALF: f32 = 5.0;
/// Extra slack around a marker so it is easy to hit with the mouse.
const SITE_CLICK_SLACK: f32 = 4.0;
/// Ceiling on markers drawn per pane, so a dense view stays bounded.
const MAX_SITE_MARKERS: usize = 250;

pub struct PaneInteraction {
    pub clicked: bool,
    /// Where the pointer is, in radar-local kilometres, when it is over this
    /// pane. The app probes this on the next frame rather than reaching into
    /// the volume mid-paint; a hover readout one frame behind the cursor is
    /// imperceptible, and scanning a moment grid during layout is not.
    pub hovered_world_km: Option<(f64, f64)>,
    pub camera: Camera2D,
    pub camera_changed: bool,
    pub viewport: ViewportMetrics,
    /// A radar site marker the user clicked, if any.
    pub clicked_site: Option<String>,
    /// Where a Ctrl+click landed, in degrees `(longitude, latitude)`, when the
    /// pane has a projection to place it with.
    pub ctrl_clicked_lon_lat: Option<(f64, f64)>,
}

pub fn pane_rects(canvas: egui::Rect, layout: PaneLayout) -> Vec<(PaneId, egui::Rect)> {
    let pane = |index| PaneId::new(index).expect("pane index is within workstation limit");
    match layout {
        PaneLayout::One => vec![(pane(0), canvas)],
        PaneLayout::TwoHorizontal => {
            let half = (canvas.height() - PANE_GAP) * 0.5;
            let top = egui::Rect::from_min_size(canvas.min, egui::vec2(canvas.width(), half));
            let bottom = egui::Rect::from_min_size(
                egui::pos2(canvas.left(), top.bottom() + PANE_GAP),
                egui::vec2(canvas.width(), half),
            );
            vec![(pane(0), top), (pane(1), bottom)]
        }
        PaneLayout::TwoVertical => {
            let half = (canvas.width() - PANE_GAP) * 0.5;
            let left = egui::Rect::from_min_size(canvas.min, egui::vec2(half, canvas.height()));
            let right = egui::Rect::from_min_size(
                egui::pos2(left.right() + PANE_GAP, canvas.top()),
                egui::vec2(half, canvas.height()),
            );
            vec![(pane(0), left), (pane(1), right)]
        }
        PaneLayout::Four => {
            let width = (canvas.width() - PANE_GAP) * 0.5;
            let height = (canvas.height() - PANE_GAP) * 0.5;
            let top_left = egui::Rect::from_min_size(canvas.min, egui::vec2(width, height));
            let top_right = egui::Rect::from_min_size(
                egui::pos2(top_left.right() + PANE_GAP, canvas.top()),
                egui::vec2(width, height),
            );
            let bottom_left = egui::Rect::from_min_size(
                egui::pos2(canvas.left(), top_left.bottom() + PANE_GAP),
                egui::vec2(width, height),
            );
            let bottom_right = egui::Rect::from_min_size(
                egui::pos2(top_right.left(), top_right.bottom() + PANE_GAP),
                egui::vec2(width, height),
            );
            vec![
                (pane(0), top_left),
                (pane(1), top_right),
                (pane(2), bottom_left),
                (pane(3), bottom_right),
            ]
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub fn draw_pane(
    ui: &mut egui::Ui,
    pane: PaneId,
    rect: egui::Rect,
    active: bool,
    camera: Camera2D,
    tuning: NavTuning,
    texture: Option<PaneTexture<'_>>,
    map: &PaneMap,
    title: &str,
    status: &str,
    overlay: &PaneOverlay<'_>,
) -> PaneInteraction {
    let response = ui.interact(
        rect,
        ui.id().with(("radar-pane", pane.get())),
        egui::Sense::click_and_drag(),
    );
    let viewport = ViewportMetrics {
        width_points: rect.width().max(1.0),
        height_points: rect.height().max(1.0),
        pixels_per_point: ui.ctx().pixels_per_point().max(1.0),
    };
    let mut updated_camera = camera;
    let mut camera_changed = false;

    if response.dragged() {
        let delta = ui.input(|input| input.pointer.delta());
        if delta.length_sq() > 0.0 {
            updated_camera.pan_by_screen_delta(delta.x, delta.y);
            camera_changed = true;
        }
    }

    if response.hovered() {
        // The exponent remaps the tuned 1.2-per-notch response to the
        // analyst's chosen rate - `1.2^n` becomes `user^n` - with the burst
        // acceleration riding along unchanged.
        let factor = wheel_zoom_factor(ui, pane).powf(tuning.zoom_exp);
        if factor != 1.0 {
            // Anchor on the POINTER, not the pane centre: holding the world
            // point under the cursor still is what makes a zoom feel aimed
            // rather than merely applied.
            let pointer = ui
                .input(|input| input.pointer.hover_pos())
                .unwrap_or(rect.center());
            let local = ScreenPoint::new(pointer.x - rect.left(), pointer.y - rect.top());
            updated_camera.zoom_about(factor, local, viewport);
            camera_changed = true;
        }
    }

    // Keyboard flight, for the active pane only. A warning is not the moment to
    // be hunting for a scale with a wheel.
    let nav = keyboard_nav(ui, active);
    if !nav.is_idle() {
        let dt = ui.input(|input| input.stable_dt);
        if nav.reset {
            // Reset wins outright inside `apply_nav`; splitting it would
            // let the zoom pass move a camera the reset just homed.
            camera_changed |= updated_camera.apply_nav(nav, dt, viewport);
        } else {
            // Two passes so pan and zoom each run at their own configured
            // rate: dt scaling is exact because the pan step is linear in dt
            // and the held zoom is exponential in it (see [`NavTuning`]).
            let pan_only = NavInput {
                zoom_hold: 0.0,
                zoom_steps: 0.0,
                ..nav
            };
            let zoom_only = NavInput {
                pan_right: 0.0,
                pan_up: 0.0,
                zoom_steps: nav.zoom_steps * tuning.zoom_exp,
                ..nav
            };
            camera_changed |= updated_camera.apply_nav(pan_only, dt * tuning.pan_scale, viewport);
            camera_changed |= updated_camera.apply_nav(zoom_only, dt * tuning.kzoom_exp, viewport);
        }
        // A held key produces no further events, so without this the flight
        // would stop after one frame and resume on the next mouse twitch.
        ui.ctx().request_repaint();
    }

    if tuning.double_click_reset && response.double_clicked() {
        updated_camera = Camera2D::default();
        camera_changed = true;
    }

    // Ctrl+click asks for the nearest S-band radar, and the site markers have
    // to stand down for it: the marker halo is nine screen points, which at the
    // scale a pane opens on is 36 km of ground, and every TDWR this gesture
    // exists to beat is closer than that to its own downtown. Letting a marker
    // win here answers Ctrl+click on Dallas with TDAL.
    let modifiers = ui.input(|input| input.modifiers);

    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 0.0, chrome_color(map.chrome.canvas));

    // Imagery is the ground itself, so it goes under everything - including
    // the vector boundaries, which are the part that has to stay legible on
    // top of a satellite picture and already do.
    paint_tiles(&painter, rect, pane, updated_camera, viewport, map);

    // Map underlay first: the radar draws over it.
    paint_map(&painter, rect, pane, updated_camera, viewport, map);

    if let Some(texture) = texture {
        paint_transformed_texture(&painter, rect, updated_camera, viewport, texture);
    }
    draw_map_labels(&painter, rect, updated_camera, viewport, map);
    // Warnings over the labels: a tornado box must never be the thing a county
    // name is drawn on top of.
    draw_hazards(&painter, rect, updated_camera, viewport, map);
    let clicked_site = draw_radar_sites(
        ui,
        &painter,
        rect,
        updated_camera,
        viewport,
        map,
        response.clicked() && crate::nearest_site::site_marker_click_allowed(modifiers),
    );
    // Where the Ctrl+click landed. Read here, decided in `nearest_site`:
    // nothing in this file chooses which radar wins.
    let ctrl_clicked_lon_lat = if crate::nearest_site::nearest_s_band_click(
        modifiers,
        response.clicked(),
        response.double_clicked(),
    ) {
        response
            .interact_pointer_pos()
            .filter(|pointer| rect.contains(*pointer))
            .zip(map.projection.as_ref())
            .and_then(|(pointer, projection)| {
                let local = ScreenPoint::new(pointer.x - rect.left(), pointer.y - rect.top());
                // `and_then`, not `map`: a click on the black beyond the globe
                // has no longitude and must select nothing rather than the
                // nearest thing the limb happens to touch. `PaneInteraction`
                // then reports it as an ordinary pane click, which is what a
                // click on nothing should be.
                projection.globe_to_lon_lat(
                    updated_camera.screen_to_world(local, viewport),
                    map_scene::projection::globe::blend_for_pane(
                        updated_camera.sanitized().km_per_point,
                        viewport,
                    ),
                )
            })
    } else {
        None
    };
    draw_range_rings(&painter, rect, updated_camera, viewport, map);
    draw_cursor_readout(
        ui,
        &painter,
        rect,
        updated_camera,
        viewport,
        response.hovered(),
        map.projection.as_ref(),
        chrome_color(map.chrome.readout_ink),
    );
    // Overlays sit above the readout and below the header, so the header and
    // the active-pane border stay on top of everything.
    if let (Some(layout), Some(table)) = (overlay.legend, overlay.table) {
        crate::legend::draw_legend(
            &painter,
            rect,
            layout,
            table,
            overlay.product_name,
            overlay.badges,
        );
    }
    if let Some(probe) = overlay.probe {
        draw_probe_readout(&painter, rect, probe, chrome_color(map.chrome.probe_ink));
    }
    draw_tile_attribution(&painter, rect, map);
    draw_header(&painter, rect, title, status);
    draw_border(&painter, rect, active);

    PaneInteraction {
        // A click that selected a site is consumed by that site, and a
        // Ctrl+click is consumed by the radar switch.
        clicked: response.clicked() && clicked_site.is_none() && ctrl_clicked_lon_lat.is_none(),
        hovered_world_km: hovered_world_km(ui, rect, updated_camera, viewport, response.hovered()),
        camera: updated_camera,
        camera_changed,
        viewport,
        clicked_site,
        ctrl_clicked_lon_lat,
    }
}

/// Turn this frame's wheel and pinch input into one zoom factor for `pane`.
///
/// Read from the RAW wheel events rather than from `smooth_scroll_delta`,
/// because that field has already had two device-specific decisions baked into
/// it that a map cannot use:
///
///   * it is scaled by egui's `line_scroll_speed` and then spread over several
///     frames, so one detent arrives as a stream of fractions and there is no
///     longer any way to tell a wheel notch from a trackpad nudge of the same
///     size -- and a rule tuned for one of those is wrong for the other;
///   * when the zoom modifier is down, egui routes the wheel into `zoom_delta`
///     and leaves `smooth_scroll_delta` at ZERO, which is why Ctrl+scroll used
///     to do nothing at all in this pane.
///
/// The raw events still carry their unit, so `Line` (a detented wheel) and
/// `Point` (a trackpad, or a wheel reporting pixels) can be counted apart --
/// which is the whole basis of the response in `analyst_runtime`. Ctrl+scroll
/// arrives here as an ordinary wheel event and zooms like one.
///
/// The burst state is per pane and lives in egui's temporary memory, so a spin
/// in one pane does not accelerate the next notch in another.
fn wheel_zoom_factor(ui: &egui::Ui, pane: PaneId) -> f32 {
    let (notches, gesture, now) = ui.input(|input| {
        let mut notches = WheelNotches::NONE;
        for event in &input.events {
            let egui::Event::MouseWheel { unit, delta, .. } = event else {
                continue;
            };
            match unit {
                // A detent. The magnitude is 1.0 per notch on an ordinary
                // wheel and a fraction of that on a high-resolution one, so
                // notches are summed rather than counted.
                egui::MouseWheelUnit::Line => notches.detented += delta.y,
                egui::MouseWheelUnit::Point => {
                    notches.continuous += delta.y / TRACKPAD_POINTS_PER_NOTCH;
                }
                // Nothing on a desk sends pages, but a remote desktop or an
                // accessibility device can; treat one as a large swipe.
                egui::MouseWheelUnit::Page => notches.continuous += delta.y * 4.0,
            }
        }
        // A pinch is the one zoom egui reports as a FACTOR rather than as
        // scroll, and it is already smoothed, so it is applied as it arrives.
        // `Event::Zoom` and `multi_touch` are read instead of `zoom_delta`
        // because `zoom_delta` also carries egui's Ctrl+scroll synthesis of
        // the very wheel events counted above, which would zoom twice.
        let mut gesture = 1.0_f32;
        for event in &input.events {
            if let egui::Event::Zoom(factor) = event {
                gesture *= factor;
            }
        }
        if let Some(touch) = input.multi_touch() {
            // Same precedence egui itself uses: a real two-finger measurement
            // beats a synthesised factor.
            gesture = touch.zoom_delta;
        }
        (notches, gesture, input.time)
    });

    let wheel = if notches.is_idle() {
        1.0
    } else {
        let id = ui.id().with(("pane-zoom-response", pane.get()));
        ui.ctx().data_mut(|data| {
            data.get_temp_mut_or_default::<ZoomResponder>(id)
                .factor(notches, now)
        })
    };
    let factor = wheel * gesture;
    if factor.is_finite() && factor > 0.0 {
        factor
    } else {
        1.0
    }
}

/// Read keyboard navigation for the pane the analyst is working in.
///
/// The bindings, and why each one:
///
///   * **Arrow keys and WASD** both pan. Arrows are the discoverable pair and
///     they survive a non-QWERTY layout, where egui reports logical keys and
///     `W` is somewhere else. WASD is there so the left hand can fly the camera
///     while the right hand stays on the mouse -- which is how a warning
///     actually gets worked.
///   * **`+` and `=` zoom in, `-` zooms out.** `=` is bound alongside `+`
///     because `+` is Shift+`=` on most layouts and nobody reaches for Shift to
///     zoom. A tap is one wheel notch; holding flies.
///   * **`Home` resets to the radar** -- the same reset the double-click on the
///     pane already performs. It is the key every application spells "back to
///     the beginning", and it is not a letter, so it cannot be hit while
///     typing.
///
/// Nothing here collides. Grepped: the only other keyboard readers in this
/// application are the product picker (`ArrowUp`/`ArrowDown`/`Enter`/`Escape`,
/// which it CONSUMES in the toolbar, drawn before the canvas) and the popup
/// dismissal (`Escape`). Neither is bound here. The whole block is skipped
/// while any widget wants the keyboard, so typing a site identifier into the
/// toolbar never flies the camera, and it is skipped for inactive panes, so
/// only the pane with the highlighted border responds.
fn keyboard_nav(ui: &egui::Ui, active: bool) -> NavInput {
    if !active || ui.ctx().egui_wants_keyboard_input() {
        return NavInput::default();
    }
    ui.input(|input| {
        // A held modifier means the analyst is aiming at something else -- a
        // window shortcut, a text operation -- so plain keys only. Shift is
        // allowed through because `+` IS Shift on most layouts.
        if input.modifiers.ctrl || input.modifiers.alt || input.modifiers.command {
            return NavInput::default();
        }
        let held = |keys: &[egui::Key]| keys.iter().any(|key| input.key_down(*key));
        let axis = |positive: &[egui::Key], negative: &[egui::Key]| {
            let forward = if held(positive) { 1.0_f32 } else { 0.0 };
            let back = if held(negative) { 1.0_f32 } else { 0.0 };
            forward - back
        };
        // Key REPEAT is excluded from the step count: the operating system
        // repeat rate is not a zoom rate, and the hold below already covers
        // what a held key should do.
        let steps: f32 = input
            .events
            .iter()
            .filter_map(|event| match event {
                egui::Event::Key {
                    key,
                    pressed: true,
                    repeat: false,
                    ..
                } => match key {
                    egui::Key::Plus | egui::Key::Equals => Some(1.0_f32),
                    egui::Key::Minus => Some(-1.0),
                    _ => None,
                },
                _ => None,
            })
            .sum();
        NavInput {
            pan_right: axis(
                &[egui::Key::ArrowRight, egui::Key::D],
                &[egui::Key::ArrowLeft, egui::Key::A],
            ),
            pan_up: axis(
                &[egui::Key::ArrowUp, egui::Key::W],
                &[egui::Key::ArrowDown, egui::Key::S],
            ),
            zoom_hold: axis(&[egui::Key::Plus, egui::Key::Equals], &[egui::Key::Minus]),
            zoom_steps: steps,
            reset: input.key_pressed(egui::Key::Home),
        }
    })
}

/// Draw the warning polygons that reach this pane.
///
/// Positions arrive already projected, so this only applies the camera
/// transform and culls. Hazards are handed over least severe first, so the
/// worst one is painted last and its outline wins where two overlap.
fn draw_hazards(
    painter: &egui::Painter,
    rect: egui::Rect,
    camera: Camera2D,
    viewport: ViewportMetrics,
    map: &PaneMap,
) {
    let globe_blend =
        map_scene::projection::globe::blend_for_pane(camera.sanitized().km_per_point, viewport);
    for hazard in map.hazards.iter() {
        // A polygon is dropped WHOLE if any vertex is behind the limb:
        // `hazard.triangles` indexes into `points`, so removing one vertex
        // would corrupt the fill mesh rather than shorten it.
        let mut behind_limb = false;
        let points: Vec<egui::Pos2> = hazard
            .points
            .iter()
            .map(|world| {
                let world = map_scene::projection::globe::warp_world(*world, globe_blend)
                    .unwrap_or_else(|| {
                        behind_limb = true;
                        *world
                    });
                let screen = camera.world_to_screen(world, viewport);
                egui::pos2(rect.left() + screen.x, rect.top() + screen.y)
            })
            .collect();
        if points.len() < 3 || behind_limb {
            continue;
        }

        // Cull on the PROJECTED bounds rather than on latitude and longitude:
        // at a continental zoom the two differ enough that a lat/lon test drops
        // polygons that are in fact on screen.
        let mut bounds = egui::Rect::NOTHING;
        for point in &points {
            bounds = bounds.union(egui::Rect::from_min_max(*point, *point));
        }
        if !rect.intersects(bounds) {
            continue;
        }

        let color = hazard.color;
        let fill = egui::Color32::from_rgba_unmultiplied(
            color.r(),
            color.g(),
            color.b(),
            HAZARD_FILL_ALPHA,
        );
        let width = if hazard.emphatic {
            HAZARD_STROKE_WIDTH * 1.8
        } else {
            HAZARD_STROKE_WIDTH
        };

        if !hazard.triangles.is_empty() {
            painter.add(egui::Shape::mesh(fill_mesh(
                &points,
                &hazard.triangles,
                fill,
            )));
        }
        // The outline is a CLOSED line rather than the polygon's own stroke: a
        // warning polygon is frequently concave, and a stroke applied to a
        // triangulated shape cuts across the notch.
        let mut outline = points.clone();
        outline.push(points[0]);
        painter.add(egui::Shape::line(outline, egui::Stroke::new(width, color)));

        draw_hazard_motion(painter, &points, hazard, width);
        draw_hazard_tag(painter, &points, hazard, chrome_color(map.chrome.canvas));
    }
}

/// Build the fill from the triangles worked out at placement time.
///
/// The camera is a translate and a scale, so a triangulation computed in world
/// kilometres stays valid in screen points -- which is why this pass only
/// transforms vertices and never re-triangulates.
fn fill_mesh(points: &[egui::Pos2], triangles: &[[u32; 3]], fill: egui::Color32) -> egui::Mesh {
    let mut mesh = egui::Mesh::default();
    for point in points {
        mesh.colored_vertex(*point, fill);
    }
    for [a, b, c] in triangles {
        mesh.add_triangle(*a, *b, *c);
    }
    mesh
}

/// Storm motion, drawn from the centroid toward where the storm is GOING.
///
/// The bulletin reports the direction the storm comes FROM, so the vector is
/// that bearing plus 180 degrees.
fn draw_hazard_motion(
    painter: &egui::Painter,
    points: &[egui::Pos2],
    hazard: &PlacedHazard,
    width: f32,
) {
    let Some((from_degrees, knots)) = hazard.motion else {
        return;
    };
    let count = points.len() as f32;
    let centroid = egui::pos2(
        points.iter().map(|point| point.x).sum::<f32>() / count,
        points.iter().map(|point| point.y).sum::<f32>() / count,
    );
    let heading = (f32::from(from_degrees) + 180.0).to_radians();
    // Length scales with speed but is capped: a 60 kt arrow that reaches
    // across the county says nothing extra.
    let length = (f32::from(knots) * 0.9).clamp(14.0, 54.0);
    let tip = egui::pos2(
        centroid.x + heading.sin() * length,
        centroid.y - heading.cos() * length,
    );
    let stroke = egui::Stroke::new(width, hazard.color);
    painter.line_segment([centroid, tip], stroke);
    for side in [-1.0_f32, 1.0] {
        let barb = heading + side * 150.0_f32.to_radians();
        painter.line_segment(
            [
                tip,
                egui::pos2(tip.x + barb.sin() * 7.0, tip.y - barb.cos() * 7.0),
            ],
            stroke,
        );
    }
}

/// The short tag, at the NORTHERNMOST vertex -- the top of the polygon on
/// screen, which is where there is reliably room outside the shape.
///
/// `halo` is the pane's own ground, so the outline around the tag is the
/// colour the tag is most often sitting on. On a light basemap a dark halo
/// would read as a smudge, which is the same mistake in the opposite
/// direction.
fn draw_hazard_tag(
    painter: &egui::Painter,
    points: &[egui::Pos2],
    hazard: &PlacedHazard,
    halo: egui::Color32,
) {
    if hazard.tag.is_empty() {
        return;
    }
    let top = points
        .iter()
        .copied()
        .min_by(|a, b| a.y.total_cmp(&b.y))
        .unwrap_or(points[0]);
    let at = egui::pos2(top.x, top.y - 7.0);
    // A one-pixel dark halo, because a warning tag lands on radar as often as
    // on the basemap and has to read on both.
    for (dx, dy) in [(-1.0, 0.0), (1.0, 0.0), (0.0, -1.0), (0.0, 1.0)] {
        painter.text(
            egui::pos2(at.x + dx, at.y + dy),
            egui::Align2::CENTER_BOTTOM,
            &hazard.tag,
            egui::FontId::monospace(10.0),
            halo,
        );
    }
    painter.text(
        at,
        egui::Align2::CENTER_BOTTOM,
        &hazard.tag,
        egui::FontId::monospace(10.0),
        hazard.color,
    );
}

/// Draw radar site markers and report one if it was clicked.
///
/// Positions arrive already projected, so this only applies the camera
/// transform. Markers are drawn as small boxes, which is what makes them an
/// obvious click target rather than a decoration.
#[allow(clippy::too_many_arguments)]
fn draw_radar_sites(
    ui: &egui::Ui,
    painter: &egui::Painter,
    rect: egui::Rect,
    camera: Camera2D,
    viewport: ViewportMetrics,
    map: &PaneMap,
    clicked: bool,
) -> Option<String> {
    if map.sites.is_empty() {
        return None;
    }
    // Same blend the vertex shader uses, from the same camera AND the same
    // pane metrics, so a marker and the coastline under it cannot disagree.
    let globe_blend =
        map_scene::projection::globe::blend_for_pane(camera.sanitized().km_per_point, viewport);
    let pointer = ui.input(|input| input.pointer.hover_pos());
    let mut hit: Option<(String, f32)> = None;
    let mut drawn = 0_usize;

    for site in map.sites.iter() {
        if drawn >= MAX_SITE_MARKERS {
            break;
        }
        // A site on the far side of the globe is not drawn at all. It is not
        // moved to the limb: a marker at a position that is not the site's is
        // worse than no marker.
        let Some(world) = map_scene::projection::globe::warp_world(site.world, globe_blend) else {
            continue;
        };
        let screen = camera.world_to_screen(world, viewport);
        let position = egui::pos2(rect.left() + screen.x, rect.top() + screen.y);
        if !rect.contains(position) {
            continue;
        }
        drawn += 1;

        let active = map.active_site.as_deref() == Some(site.id.as_str());
        let hovered = pointer.is_some_and(|pointer| {
            (pointer - position).length() <= SITE_MARKER_HALF + SITE_CLICK_SLACK
        });
        if hovered && let Some(pointer) = pointer {
            let distance = (pointer - position).length();
            if hit.as_ref().is_none_or(|(_, best)| distance < *best) {
                hit = Some((site.id.clone(), distance));
            }
        }

        let box_rect = egui::Rect::from_center_size(
            position,
            egui::vec2(SITE_MARKER_HALF * 2.0, SITE_MARKER_HALF * 2.0),
        );
        // Outline and identifier come from the chosen look; the fill does
        // not. A marker's fill is a translucent dark scrim that reads as a
        // filled box on a dark pane and as a shadow on a light one, so it
        // needs no per-look value - whereas the outline and the name are ink
        // straight onto the ground, and on the Daylight pane the shipped
        // hover amber separated from it by 0.006 of luminance.
        let (stroke_color, fill) = if active {
            (
                chrome_color(map.chrome.site_active_ink),
                egui::Color32::from_rgba_unmultiplied(40, 120, 170, 140),
            )
        } else if hovered {
            (
                chrome_color(map.chrome.site_hover_ink),
                egui::Color32::from_rgba_unmultiplied(120, 100, 40, 150),
            )
        } else {
            (
                chrome_color(map.chrome.site_ink),
                egui::Color32::from_rgba_unmultiplied(30, 45, 60, 120),
            )
        };
        painter.rect_filled(box_rect, 1.0, fill);
        // A halo ring under the ink ring, the same pairing the labels use: a
        // lone 1 px slate outline disappears on top of bright reflectivity,
        // which is exactly where an analyst most needs to find the site.
        painter.rect_stroke(
            box_rect,
            1.0,
            egui::Stroke::new(3.0, chrome_color(map.chrome.label_halo)),
            egui::StrokeKind::Middle,
        );
        painter.rect_stroke(
            box_rect,
            1.0,
            egui::Stroke::new(if active || hovered { 1.6_f32 } else { 1.2 }, stroke_color),
            egui::StrokeKind::Middle,
        );

        // Only label what the analyst can act on, so a continental view is not
        // buried under two hundred identifiers - unless the settings say
        // always or never, which both override the clutter rule.
        if match map.site_labels {
            SiteLabelMode::Always => true,
            SiteLabelMode::Never => false,
            SiteLabelMode::Auto => active || hovered || map.sites.len() <= 40,
        } {
            painter.text(
                position + egui::vec2(0.0, -SITE_MARKER_HALF - 2.0),
                egui::Align2::CENTER_BOTTOM,
                &site.id,
                egui::FontId::monospace(11.0),
                stroke_color,
            );
        }
    }

    let hovered_site = hit.map(|(id, _)| id);
    if hovered_site.is_some() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    if clicked { hovered_site } else { None }
}

/// Queue the retained map for this pane.
///
/// The callback carries only a geometry handle and the camera; the vertex and
/// index buffers behind it are already on the GPU and are not touched here.
fn paint_map(
    painter: &egui::Painter,
    rect: egui::Rect,
    pane: PaneId,
    camera: Camera2D,
    viewport: ViewportMetrics,
    map: &PaneMap,
) {
    let Some(geometry) = map.geometry.clone() else {
        return;
    };
    if geometry.is_empty() {
        return;
    }
    let pixels_per_point = viewport.sanitized().pixels_per_point;
    let callback = MapPaintCallback {
        pane_index: pane.index(),
        geometry,
        camera,
        viewport,
        rect_px: [
            rect.left() * pixels_per_point,
            rect.top() * pixels_per_point,
            rect.right() * pixels_per_point,
            rect.bottom() * pixels_per_point,
        ],
    };
    painter.add(eframe::egui_wgpu::Callback::new_paint_callback(
        rect, callback,
    ));
}
/// The raster tile underlay, drawn through its own wgpu callback.
///
/// Queued before `paint_map` so the imagery is genuinely the ground: the
/// vector boundaries and everything else in this file paint on top of it.
fn paint_tiles(
    painter: &egui::Painter,
    rect: egui::Rect,
    pane: PaneId,
    camera: Camera2D,
    viewport: ViewportMetrics,
    map: &PaneMap,
) {
    let Some(frame) = map.tiles.clone() else {
        return;
    };
    if frame.draws.is_empty() && frame.uploads.is_empty() {
        return;
    }
    let pixels_per_point = viewport.sanitized().pixels_per_point;
    let callback = TilePaintCallback {
        pane_index: pane.index(),
        frame,
        camera,
        viewport,
        rect_px: [
            rect.left() * pixels_per_point,
            rect.top() * pixels_per_point,
            rect.right() * pixels_per_point,
            rect.bottom() * pixels_per_point,
        ],
    };
    painter.add(eframe::egui_wgpu::Callback::new_paint_callback(
        rect, callback,
    ));
}

/// The provider's required attribution, bottom right.
///
/// Drawn unconditionally whenever imagery is on, and there is deliberately no
/// switch for it: displaying it is a condition of using every provider this
/// app ships, not a preference.
fn draw_tile_attribution(painter: &egui::Painter, rect: egui::Rect, map: &PaneMap) {
    let Some(frame) = map.tiles.as_ref() else {
        return;
    };
    let galley = painter.layout_no_wrap(
        frame.attribution.to_owned(),
        egui::FontId::proportional(9.0),
        egui::Color32::from_gray(232),
    );
    let padding = egui::vec2(4.0, 2.0);
    let plate = egui::Rect::from_min_size(
        egui::pos2(
            rect.right() - galley.size().x - padding.x * 2.0 - 4.0,
            rect.bottom() - galley.size().y - padding.y * 2.0 - 4.0,
        ),
        galley.size() + padding * 2.0,
    );
    painter.rect_filled(plate, 2.0, egui::Color32::from_black_alpha(150));
    painter.galley(plate.min + padding, galley, egui::Color32::from_gray(232));
}

fn paint_transformed_texture(
    painter: &egui::Painter,
    rect: egui::Rect,
    current_camera: Camera2D,
    current_viewport: ViewportMetrics,
    texture: PaneTexture<'_>,
) {
    let rendered_viewport = texture.viewport.sanitized();
    let rendered_corners = [
        ScreenPoint::new(0.0, 0.0),
        ScreenPoint::new(rendered_viewport.width_points, 0.0),
        ScreenPoint::new(
            rendered_viewport.width_points,
            rendered_viewport.height_points,
        ),
        ScreenPoint::new(0.0, rendered_viewport.height_points),
    ];
    let uv = [
        egui::pos2(0.0, 0.0),
        egui::pos2(1.0, 0.0),
        egui::pos2(1.0, 1.0),
        egui::pos2(0.0, 1.0),
    ];
    let mut mesh = egui::Mesh::with_texture(texture.handle.id());
    for (corner, uv) in rendered_corners.into_iter().zip(uv) {
        let world = texture.camera.screen_to_world(corner, rendered_viewport);
        let current = current_camera.world_to_screen(world, current_viewport);
        mesh.vertices.push(egui::epaint::Vertex {
            pos: egui::pos2(rect.left() + current.x, rect.top() + current.y),
            uv,
            color: egui::Color32::WHITE,
        });
    }
    mesh.indices.extend_from_slice(&[0, 1, 2, 0, 2, 3]);
    painter.add(egui::Shape::mesh(mesh));
}

/// Draw the labels that survived bounded placement.
///
/// Text is egui's, drawn after the retained geometry. The expensive part —
/// projecting every candidate — already happened in the build; this only
/// transforms the survivors and rejects overlaps.
fn draw_map_labels(
    painter: &egui::Painter,
    rect: egui::Rect,
    camera: Camera2D,
    viewport: ViewportMetrics,
    map: &PaneMap,
) {
    let Some(geometry) = map.geometry.as_ref() else {
        return;
    };
    let ink = chrome_color(map.chrome.label_ink);
    let halo = chrome_color(map.chrome.label_halo);
    // The provider comes off the tile frame this pane is already holding, so
    // nothing new has to be plumbed through `PaneMap` or `app.rs`. With USGS
    // Topo or Imagery Topo the raster prints its own place names and this
    // returns an empty list, which is what stops "Oklahoma City" appearing
    // twice.
    let (placed, _metrics) = map_scene::labels::place_labels_for_pane(
        geometry,
        camera,
        viewport,
        map_scene::MAX_LABELS_PLACED,
        map_scene::labels::LabelContext::for_pane(
            camera,
            viewport,
            map.tiles.as_ref().map(|frame| frame.key.provider),
        ),
    );
    for label in placed {
        let position = egui::pos2(
            rect.left() + label.position.x,
            rect.top() + label.position.y,
        );
        // A halo that contrasts with the ink keeps the name readable where it
        // lands on bright reflectivity rather than on the basemap. Which way
        // round that runs is the preset's decision, not this function's: on a
        // light pane the halo is the light one.
        for offset in [
            egui::vec2(-1.0, 0.0),
            egui::vec2(1.0, 0.0),
            egui::vec2(0.0, -1.0),
            egui::vec2(0.0, 1.0),
        ] {
            painter.text(
                position + offset,
                egui::Align2::CENTER_CENTER,
                label.name,
                egui::FontId::proportional(10.0),
                halo,
            );
        }
        painter.text(
            position,
            egui::Align2::CENTER_CENTER,
            label.name,
            egui::FontId::proportional(10.0),
            ink,
        );
    }
}

/// A style colour as the egui painter wants it.
///
/// `to_rgba8` is the same quantisation `build_geometry`'s `pack_color` applies
/// before a vertex leaves the CPU, so the ground under a line and the line
/// itself round the same way and cannot end up a step apart. Straight alpha
/// out (`from_rgba_unmultiplied`), because egui premultiplies internally.
fn chrome_color(color: map_scene::LayerColor) -> egui::Color32 {
    let [red, green, blue, alpha] = color.to_rgba8();
    egui::Color32::from_rgba_unmultiplied(red, green, blue, alpha)
}

fn draw_range_rings(
    painter: &egui::Painter,
    rect: egui::Rect,
    camera: Camera2D,
    viewport: ViewportMetrics,
    map: &PaneMap,
) {
    let radar = camera.world_to_screen(WorldPoint::ORIGIN, viewport);
    let center = egui::pos2(rect.left() + radar.x, rect.top() + radar.y);
    let stroke = egui::Stroke::new(0.8_f32, chrome_color(map.chrome.range_ring));
    for range_km in RANGE_RINGS_KM {
        let radius = (*range_km as f32 / camera.sanitized().km_per_point).abs();
        if radius > 4.0 && radius < rect.width().max(rect.height()) * 2.0 {
            painter.circle_stroke(center, radius, stroke);
        }
    }
    painter.circle_filled(center, 3.2, chrome_color(map.chrome.origin_dot));
}

/// The pointer's radar-local position, when it is over this pane.
fn hovered_world_km(
    ui: &egui::Ui,
    rect: egui::Rect,
    camera: Camera2D,
    viewport: ViewportMetrics,
    hovered: bool,
) -> Option<(f64, f64)> {
    if !hovered {
        return None;
    }
    let pointer = ui.input(|input| input.pointer.hover_pos())?;
    if !rect.contains(pointer) {
        return None;
    }
    let local = ScreenPoint::new(pointer.x - rect.left(), pointer.y - rect.top());
    // The probe samples the volume in radar-local kilometres, so the globe has
    // to be undone first or it reads the wrong gate.
    let world = map_scene::projection::globe::unwarp_world(
        camera.screen_to_world(local, viewport),
        map_scene::projection::globe::blend_for_pane(camera.sanitized().km_per_point, viewport),
    )?;
    Some((world.east_km, world.north_km))
}

/// The value under the cursor, drawn above the geographic readout.
fn draw_probe_readout(painter: &egui::Painter, rect: egui::Rect, text: &str, ink: egui::Color32) {
    let anchor = egui::pos2(rect.left() + 8.0, rect.bottom() - 26.0);
    painter.text(
        anchor,
        egui::Align2::LEFT_BOTTOM,
        text,
        egui::FontId::monospace(12.0),
        ink,
    );
}

#[allow(clippy::too_many_arguments)]
fn draw_cursor_readout(
    ui: &egui::Ui,
    painter: &egui::Painter,
    rect: egui::Rect,
    camera: Camera2D,
    viewport: ViewportMetrics,
    hovered: bool,
    projection: Option<&RadarProjection>,
    ink: egui::Color32,
) {
    if !hovered {
        return;
    }
    let Some(pointer) = ui.input(|input| input.pointer.hover_pos()) else {
        return;
    };
    if !rect.contains(pointer) {
        return;
    }
    let local = ScreenPoint::new(pointer.x - rect.left(), pointer.y - rect.top());
    // Undo the globe morph BEFORE reading anything off the position. Range and
    // azimuth are radar-local quantities and must not be measured on the bent
    // frame; off the globe entirely there is nothing under the cursor.
    let Some(world) = map_scene::projection::globe::unwarp_world(
        camera.screen_to_world(local, viewport),
        map_scene::projection::globe::blend_for_pane(camera.sanitized().km_per_point, viewport),
    ) else {
        return;
    };
    let range_km = world.east_km.hypot(world.north_km);
    let azimuth_deg = world
        .east_km
        .atan2(world.north_km)
        .to_degrees()
        .rem_euclid(360.0);
    // Same inverse transform the map was built with, so the readout and the
    // basemap can never disagree.
    let text = match projection.map(|projection| projection.world_to_lon_lat(world)) {
        Some((lon_deg, lat_deg)) => format!(
            "{range_km:.1} km  {azimuth_deg:05.1}°   {:.4}°{}  {:.4}°{}",
            lat_deg.abs(),
            if lat_deg >= 0.0 { "N" } else { "S" },
            lon_deg.abs(),
            if lon_deg >= 0.0 { "E" } else { "W" },
        ),
        None => format!("{range_km:.1} km  {azimuth_deg:05.1}°"),
    };
    painter.text(
        egui::pos2(rect.left() + 8.0, rect.bottom() - 8.0),
        egui::Align2::LEFT_BOTTOM,
        text,
        egui::FontId::monospace(11.0),
        ink,
    );
}

fn draw_header(painter: &egui::Painter, rect: egui::Rect, title: &str, status: &str) {
    let header = egui::Rect::from_min_max(
        rect.min,
        egui::pos2(
            rect.right(),
            (rect.top() + HEADER_HEIGHT).min(rect.bottom()),
        ),
    );
    painter.rect_filled(
        header,
        0.0,
        egui::Color32::from_rgba_unmultiplied(4, 7, 10, 218),
    );
    painter.text(
        egui::pos2(header.left() + 8.0, header.center().y),
        egui::Align2::LEFT_CENTER,
        title,
        egui::FontId::proportional(12.0),
        egui::Color32::from_rgb(239, 243, 246),
    );
    painter.text(
        egui::pos2(header.right() - 8.0, header.center().y),
        egui::Align2::RIGHT_CENTER,
        status,
        egui::FontId::monospace(10.0),
        egui::Color32::from_rgb(166, 184, 196),
    );
}

fn draw_border(painter: &egui::Painter, rect: egui::Rect, active: bool) {
    let color = if active {
        egui::Color32::from_rgb(78, 180, 244)
    } else {
        egui::Color32::from_rgb(45, 57, 67)
    };
    let width = if active { 2.0_f32 } else { 1.0_f32 };
    let stroke = egui::Stroke::new(width, color);
    painter.line_segment([rect.left_top(), rect.right_top()], stroke);
    painter.line_segment([rect.right_top(), rect.right_bottom()], stroke);
    painter.line_segment([rect.right_bottom(), rect.left_bottom()], stroke);
    painter.line_segment([rect.left_bottom(), rect.left_top()], stroke);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn four_pane_layout_covers_each_quadrant() {
        let canvas = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1000.0, 800.0));
        let panes = pane_rects(canvas, PaneLayout::Four);
        assert_eq!(panes.len(), 4);
        assert!(panes[0].1.center().x < canvas.center().x);
        assert!(panes[1].1.center().x > canvas.center().x);
        assert!(panes[2].1.center().y > canvas.center().y);
        assert!(panes[3].1.center().y > canvas.center().y);
    }

    use analyst_runtime::{
        DEFAULT_KM_PER_POINT, MAX_KM_PER_POINT, MIN_KM_PER_POINT, TRACKPAD_POINTS_PER_NOTCH,
        ZOOM_PER_NOTCH,
    };

    fn pane_zero() -> PaneId {
        PaneId::new(0).expect("pane 0 exists")
    }

    /// Run one egui pass and hand the pane's input readers the events a device
    /// would have sent.
    ///
    /// This drives the real `InputState`, so the test sees exactly the wheel
    /// units, smoothing and modifier routing egui applies in the application --
    /// which is the part of this that was wrong, and the part a hand-rolled
    /// fake would have hidden.
    fn run_pass<R>(
        context: &egui::Context,
        time: f64,
        events: Vec<egui::Event>,
        body: impl FnOnce(&mut egui::Ui) -> R,
    ) -> R {
        // A real integration updates the modifier state before it dispatches
        // the key or wheel event that carries it, so the pass mirrors that:
        // `InputState::modifiers` is what the readers consult, and a test that
        // set only the event's own modifiers would be testing a state no
        // window system ever produces.
        let modifiers = events
            .iter()
            .rev()
            .find_map(|event| match event {
                egui::Event::Key { modifiers, .. } | egui::Event::MouseWheel { modifiers, .. } => {
                    Some(*modifiers)
                }
                _ => None,
            })
            .unwrap_or(egui::Modifiers::NONE);
        let input = egui::RawInput {
            time: Some(time),
            modifiers,
            events,
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1000.0, 800.0),
            )),
            ..Default::default()
        };
        // `run_ui` hands the body a `Ui` for the whole viewport, which is the
        // shape `draw_pane` works in. It runs the closure once, so the body is
        // taken out of an Option rather than being required to be `FnMut`.
        let mut body = Some(body);
        let mut result = None;
        let _ = context.run_ui(input, |ui| {
            if let Some(body) = body.take() {
                result = Some(body(ui));
            }
        });
        result.expect("the pass body runs exactly once")
    }

    fn wheel(unit: egui::MouseWheelUnit, delta_y: f32, modifiers: egui::Modifiers) -> egui::Event {
        egui::Event::MouseWheel {
            unit,
            delta: egui::vec2(0.0, delta_y),
            phase: egui::TouchPhase::Move,
            modifiers,
        }
    }

    fn key(key: egui::Key, pressed: bool) -> egui::Event {
        egui::Event::Key {
            key,
            physical_key: None,
            pressed,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        }
    }

    /// One detent is one notch, whatever `line_scroll_speed` happens to be.
    ///
    /// The rate this replaced read `smooth_scroll_delta`, which is that setting
    /// multiplied in and then spread over frames, so the same detent produced a
    /// different zoom on a different platform and about 6.7% of one on this one.
    #[test]
    fn one_wheel_detent_is_one_notch() {
        let context = egui::Context::default();
        let factor = run_pass(
            &context,
            1.0,
            vec![wheel(
                egui::MouseWheelUnit::Line,
                1.0,
                egui::Modifiers::NONE,
            )],
            |ui| wheel_zoom_factor(ui, pane_zero()),
        );
        assert!((factor - ZOOM_PER_NOTCH).abs() < 1.0e-5, "{factor}");

        // And a detent the other way is its exact inverse.
        let factor = run_pass(
            &context,
            9.0,
            vec![wheel(
                egui::MouseWheelUnit::Line,
                -1.0,
                egui::Modifiers::NONE,
            )],
            |ui| wheel_zoom_factor(ui, pane_zero()),
        );
        assert!((factor - 1.0 / ZOOM_PER_NOTCH).abs() < 1.0e-5, "{factor}");
    }

    /// The regression that motivated reading raw events: with the zoom modifier
    /// down, egui moves the wheel into `zoom_delta` and leaves
    /// `smooth_scroll_delta` at zero, so Ctrl+scroll used to be inert here.
    #[test]
    fn ctrl_scroll_zooms_instead_of_doing_nothing() {
        let context = egui::Context::default();
        let events = vec![wheel(
            egui::MouseWheelUnit::Line,
            1.0,
            egui::Modifiers::COMMAND,
        )];
        let (factor, smooth) = run_pass(&context, 1.0, events, |ui| {
            (
                wheel_zoom_factor(ui, pane_zero()),
                ui.input(|input| input.smooth_scroll_delta.y),
            )
        });
        assert_eq!(smooth, 0.0, "egui still reports ctrl+scroll as a zoom");
        assert!((factor - ZOOM_PER_NOTCH).abs() < 1.0e-5, "{factor}");
    }

    /// A trackpad reports distance, not detents, so it is measured in points
    /// and gets no burst acceleration -- see `ZoomResponder::factor`.
    #[test]
    fn trackpad_points_convert_to_notches_without_acceleration() {
        let context = egui::Context::default();
        let mut zoomed = 1.0_f32;
        // Eight back-to-back frames, each a full notch of swipe. A detented
        // wheel at this rate would be well into the gain cap.
        for step in 0..8 {
            zoomed *= run_pass(
                &context,
                f64::from(step) * 0.016,
                vec![wheel(
                    egui::MouseWheelUnit::Point,
                    TRACKPAD_POINTS_PER_NOTCH,
                    egui::Modifiers::NONE,
                )],
                |ui| wheel_zoom_factor(ui, pane_zero()),
            );
        }
        let unaccelerated = ZOOM_PER_NOTCH.powi(8);
        assert!(
            (zoomed / unaccelerated - 1.0).abs() < 1.0e-3,
            "{zoomed} != {unaccelerated}"
        );
    }

    #[test]
    fn a_frame_with_no_wheel_input_leaves_the_camera_alone() {
        let context = egui::Context::default();
        let factor = run_pass(&context, 1.0, Vec::new(), |ui| {
            wheel_zoom_factor(ui, pane_zero())
        });
        assert_eq!(factor, 1.0);
    }

    /// Each pane keeps its own burst window, so spinning in one pane cannot
    /// make the next notch in another pane jump.
    #[test]
    fn burst_acceleration_does_not_leak_between_panes() {
        let context = egui::Context::default();
        let spun = egui::MouseWheelUnit::Line;
        for step in 0..8 {
            run_pass(
                &context,
                f64::from(step) * 0.02,
                vec![wheel(spun, 1.0, egui::Modifiers::NONE)],
                |ui| wheel_zoom_factor(ui, pane_zero()),
            );
        }
        let other = PaneId::new(1).expect("pane 1 exists");
        let factor = run_pass(
            &context,
            0.18,
            vec![wheel(spun, 1.0, egui::Modifiers::NONE)],
            |ui| wheel_zoom_factor(ui, other),
        );
        assert!((factor - ZOOM_PER_NOTCH).abs() < 1.0e-5, "{factor}");
    }

    #[test]
    fn arrows_and_wasd_both_pan_and_agree_on_direction() {
        let context = egui::Context::default();
        for (pressed, expected) in [
            (egui::Key::ArrowRight, (1.0, 0.0)),
            (egui::Key::D, (1.0, 0.0)),
            (egui::Key::ArrowLeft, (-1.0, 0.0)),
            (egui::Key::A, (-1.0, 0.0)),
            (egui::Key::ArrowUp, (0.0, 1.0)),
            (egui::Key::W, (0.0, 1.0)),
            (egui::Key::ArrowDown, (0.0, -1.0)),
            (egui::Key::S, (0.0, -1.0)),
        ] {
            let context = egui::Context::default();
            let nav = run_pass(&context, 1.0, vec![key(pressed, true)], |ui| {
                keyboard_nav(ui, true)
            });
            assert_eq!((nav.pan_right, nav.pan_up), expected, "{pressed:?}");
        }
        // Opposite keys held together cancel rather than picking a winner.
        let nav = run_pass(
            &context,
            1.0,
            vec![
                key(egui::Key::ArrowLeft, true),
                key(egui::Key::ArrowRight, true),
            ],
            |ui| keyboard_nav(ui, true),
        );
        assert_eq!(nav.pan_right, 0.0);
    }

    #[test]
    fn zoom_keys_report_a_step_on_press_and_a_hold_while_down() {
        let context = egui::Context::default();
        let nav = run_pass(&context, 1.0, vec![key(egui::Key::Equals, true)], |ui| {
            keyboard_nav(ui, true)
        });
        assert_eq!(nav.zoom_steps, 1.0);
        assert_eq!(nav.zoom_hold, 1.0);
        // Still held on the next frame, with no new press: the flight
        // continues, but the one-notch step does not repeat.
        let nav = run_pass(&context, 1.02, Vec::new(), |ui| keyboard_nav(ui, true));
        assert_eq!(nav.zoom_steps, 0.0);
        assert_eq!(nav.zoom_hold, 1.0);

        let context = egui::Context::default();
        let nav = run_pass(&context, 1.0, vec![key(egui::Key::Minus, true)], |ui| {
            keyboard_nav(ui, true)
        });
        assert_eq!(nav.zoom_steps, -1.0);
        assert_eq!(nav.zoom_hold, -1.0);
    }

    #[test]
    fn home_resets_and_is_the_only_reset_key_bound() {
        let context = egui::Context::default();
        let nav = run_pass(&context, 1.0, vec![key(egui::Key::Home, true)], |ui| {
            keyboard_nav(ui, true)
        });
        assert!(nav.reset);
        // End is the neighbouring key and must not do this by accident.
        let context = egui::Context::default();
        let nav = run_pass(&context, 1.0, vec![key(egui::Key::End, true)], |ui| {
            keyboard_nav(ui, true)
        });
        assert!(!nav.reset);
        assert!(nav.is_idle());
    }

    /// The two ways a key press can belong to something other than the camera.
    #[test]
    fn keyboard_navigation_yields_to_the_rest_of_the_application() {
        // An inactive pane never flies: only the pane with the highlighted
        // border responds, so four panes do not pan at once.
        let context = egui::Context::default();
        let nav = run_pass(
            &context,
            1.0,
            vec![key(egui::Key::ArrowRight, true)],
            |ui| keyboard_nav(ui, false),
        );
        assert!(nav.is_idle());

        // A modifier means the press was aimed at a shortcut, not at the map.
        let context = egui::Context::default();
        let nav = run_pass(
            &context,
            1.0,
            vec![egui::Event::Key {
                key: egui::Key::W,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::COMMAND,
            }],
            |ui| keyboard_nav(ui, true),
        );
        assert!(nav.is_idle());
    }

    /// Typing a site identifier into the toolbar must not fly the camera:
    /// `wants_keyboard_input` is the gate, and this pins that it is wired up.
    #[test]
    fn a_focused_text_field_swallows_the_navigation_keys() {
        let context = egui::Context::default();
        let mut text = String::from("KEAX");
        let mut field = None;
        // Claim focus on the first pass.
        run_pass(&context, 1.0, Vec::new(), |ui| {
            let response = ui.add(egui::TextEdit::singleline(&mut text));
            response.request_focus();
            field = Some(response.id);
        });
        let nav = run_pass(&context, 1.02, vec![key(egui::Key::S, true)], |ui| {
            assert!(
                ui.ctx().egui_wants_keyboard_input(),
                "the text field should hold focus"
            );
            let nav = keyboard_nav(ui, true);
            ui.add(egui::TextEdit::singleline(&mut text));
            nav
        });
        assert!(nav.is_idle(), "{nav:?}");
    }

    /// Drive one camera through a whole frame of pane input, the way
    /// `draw_pane` does: read the wheel, then apply it.
    ///
    /// Returns the camera so a test can assert on where it ended up rather
    /// than on the factor, which is the thing the analyst actually sees.
    fn zoom_one_frame(
        context: &egui::Context,
        camera: &mut Camera2D,
        time: f64,
        events: Vec<egui::Event>,
    ) -> f32 {
        let viewport = ViewportMetrics {
            width_points: 1000.0,
            height_points: 800.0,
            pixels_per_point: 1.0,
        };
        let factor = run_pass(context, time, events, |ui| {
            wheel_zoom_factor(ui, pane_zero())
        });
        camera.zoom_about(factor, ScreenPoint::new(731.0, 96.0), viewport);
        factor
    }

    /// A trackpad delivers a swipe as a stream of small deltas; a wheel
    /// delivers the same distance as a few large ones. Both are a distance, so
    /// both must land on the same scale.
    ///
    /// If they did not, the same gesture would zoom differently depending on
    /// how the driver happened to chop it up, which is the sort of thing an
    /// analyst experiences as "the scroll wheel is unpredictable" without ever
    /// being able to say why.
    #[test]
    fn a_swipe_zooms_the_same_however_the_driver_chops_it_up() {
        let dribbled = {
            let context = egui::Context::default();
            let mut camera = Camera2D::default();
            // Twenty events of a fifth of a notch, all in one frame.
            let events = (0..20)
                .map(|_| {
                    wheel(
                        egui::MouseWheelUnit::Point,
                        TRACKPAD_POINTS_PER_NOTCH * 0.2,
                        egui::Modifiers::NONE,
                    )
                })
                .collect();
            zoom_one_frame(&context, &mut camera, 1.0, events);
            camera.km_per_point
        };
        let in_one_go = {
            let context = egui::Context::default();
            let mut camera = Camera2D::default();
            let events = vec![wheel(
                egui::MouseWheelUnit::Point,
                TRACKPAD_POINTS_PER_NOTCH * 4.0,
                egui::Modifiers::NONE,
            )];
            zoom_one_frame(&context, &mut camera, 1.0, events);
            camera.km_per_point
        };
        assert!(
            (dribbled / in_one_go - 1.0).abs() < 1.0e-4,
            "twenty small deltas gave {dribbled}, one big one gave {in_one_go}"
        );
        // And four notches of swipe is four notches of scale, not four
        // notches accelerated: a trackpad reports its own magnitude.
        let expected = DEFAULT_KM_PER_POINT / ZOOM_PER_NOTCH.powi(4);
        assert!((in_one_go / expected - 1.0).abs() < 1.0e-4, "{in_one_go}");
    }

    /// Several detents inside one frame are one gesture, not several.
    ///
    /// A frame that carries three detents is a fast spin, so it accelerates --
    /// but only once, through the same burst window a spin spread over three
    /// frames would use. Summing them and calling the responder once is what
    /// makes the response independent of the frame rate.
    #[test]
    fn several_detents_in_one_frame_are_one_accelerated_gesture() {
        let context = egui::Context::default();
        let mut camera = Camera2D::default();
        let events = (0..3)
            .map(|_| wheel(egui::MouseWheelUnit::Line, 1.0, egui::Modifiers::NONE))
            .collect();
        zoom_one_frame(&context, &mut camera, 1.0, events);
        // Three notches at a gain of three: the frame's own three notches are
        // what fill the window, so the exponent is 3 * 3 = 9.
        let expected = DEFAULT_KM_PER_POINT / ZOOM_PER_NOTCH.powf(9.0);
        assert!(
            (camera.km_per_point / expected - 1.0).abs() < 1.0e-4,
            "{} wanted {expected}",
            camera.km_per_point
        );
        // Still inside the limits, and still a camera.
        assert_eq!(camera.sanitized(), camera);
    }

    /// The backlog a stall hands to the next frame is not a flick.
    ///
    /// A volume landing or a shader compiling stops the frame loop for a few
    /// hundred milliseconds; the window system keeps queueing the detents the
    /// analyst spun during it and delivers all of them at once. Read as one
    /// gesture, that backlog earns the full burst gain AND multiplies by its
    /// own count, so ten queued detents used to move the scale further than the
    /// entire legal range and the camera arrived at MIN_KM_PER_POINT whatever
    /// the analyst meant. Driven through the real event stream here because the
    /// batching is a property of the frame, not of the responder.
    #[test]
    fn a_stall_that_queues_ten_detents_does_not_teleport_the_camera() {
        for direction in [1.0_f32, -1.0] {
            let context = egui::Context::default();
            let start = (MIN_KM_PER_POINT * MAX_KM_PER_POINT).sqrt();
            let mut camera = Camera2D {
                km_per_point: start,
                ..Camera2D::default()
            };
            let events = (0..10)
                .map(|_| wheel(egui::MouseWheelUnit::Line, direction, egui::Modifiers::NONE))
                .collect();
            zoom_one_frame(&context, &mut camera, 1.0, events);
            let moved = (camera.km_per_point / start).max(start / camera.km_per_point);
            assert!(
                moved <= 10.0001,
                "ten queued detents moved the scale {moved}x"
            );
            // It did move, though: the frame is honoured, just not believed
            // to be a gesture ten times harder than it was.
            assert!(moved > 2.0, "the backlog was ignored entirely: {moved}x");
        }
    }

    /// Every shape of wheel event a driver can emit, including the ones that
    /// are not numbers.
    ///
    /// None of these may panic, produce a factor that is not a positive finite
    /// number, or leave the camera outside its scale limits. A zero delta and
    /// a NaN delta must both be exactly nothing: a frame the pane cannot read
    /// is a frame the map does not move.
    #[test]
    fn no_wheel_event_can_break_the_camera() {
        let deltas = [
            0.0_f32,
            -0.0,
            f32::NAN,
            f32::INFINITY,
            f32::NEG_INFINITY,
            f32::MAX,
            f32::MIN,
            1.0e30,
            -1.0e30,
            1.0e-30,
            120.0,
            -120.0,
            1.0,
            -1.0,
        ];
        for unit in [
            egui::MouseWheelUnit::Line,
            egui::MouseWheelUnit::Point,
            egui::MouseWheelUnit::Page,
        ] {
            for delta in deltas {
                for modifiers in [egui::Modifiers::NONE, egui::Modifiers::COMMAND] {
                    let context = egui::Context::default();
                    let mut camera = Camera2D {
                        km_per_point: 7.5,
                        center_east_km: 40.0,
                        center_north_km: -12.0,
                        rotation_rad: 1.1,
                    };
                    // Two hundred frames of it, so a value that merely leaks
                    // rather than jumping is caught as well.
                    for step in 0..200 {
                        let factor = zoom_one_frame(
                            &context,
                            &mut camera,
                            f64::from(step) * 0.016,
                            vec![wheel(unit, delta, modifiers)],
                        );
                        assert!(
                            factor.is_finite() && factor > 0.0,
                            "{unit:?} {delta} gave factor {factor}"
                        );
                        assert!(
                            camera.km_per_point >= MIN_KM_PER_POINT
                                && camera.km_per_point <= MAX_KM_PER_POINT,
                            "{unit:?} {delta} reached {}",
                            camera.km_per_point
                        );
                        assert_eq!(camera.sanitized(), camera, "{unit:?} {delta}");
                    }
                    // The frames that mean nothing must MEAN nothing: zero and
                    // NaN both leave the camera untouched.
                    if delta == 0.0 || delta.is_nan() {
                        assert_eq!(camera.km_per_point, 7.5, "{unit:?} {delta} moved the scale");
                    }
                }
            }
        }
    }

    /// A pinch arrives as `Event::Zoom` and must be applied once, not twice.
    ///
    /// `zoom_delta()` would double-count it, because egui folds its own
    /// Ctrl+scroll synthesis into the same number as a real pinch and the wheel
    /// events that synthesis came from are already counted here.
    #[test]
    fn a_pinch_zooms_exactly_once() {
        let context = egui::Context::default();
        let factor = run_pass(&context, 1.0, vec![egui::Event::Zoom(1.5)], |ui| {
            wheel_zoom_factor(ui, pane_zero())
        });
        assert!((factor - 1.5).abs() < 1.0e-5, "{factor}");

        // A pinch and a wheel notch in the same frame compose once each.
        let context = egui::Context::default();
        let factor = run_pass(
            &context,
            1.0,
            vec![
                egui::Event::Zoom(1.5),
                wheel(egui::MouseWheelUnit::Line, 1.0, egui::Modifiers::NONE),
            ],
            |ui| wheel_zoom_factor(ui, pane_zero()),
        );
        assert!((factor - 1.5 * ZOOM_PER_NOTCH).abs() < 1.0e-4, "{factor}");

        // A pinch that is not a number is dropped rather than propagated.
        let context = egui::Context::default();
        let factor = run_pass(&context, 1.0, vec![egui::Event::Zoom(f32::NAN)], |ui| {
            wheel_zoom_factor(ui, pane_zero())
        });
        assert_eq!(factor, 1.0);
    }

    /// The keys this pane binds, in one place, so a future binding lands next
    /// to the list it has to avoid.
    ///
    /// Every other keyboard reader in this application, found by grepping the
    /// workspace for `key_down`, `key_pressed` and `consume_key`:
    ///
    ///   * `product_picker::read_keys` -- `ArrowUp`, `ArrowDown`, `Enter`,
    ///     `Escape`.
    ///   * `vol3d::camera` -- `W`/`A`/`S`/`D`, the four arrows, `E`, `Q`,
    ///     `PageUp`, `PageDown`.
    ///   * `popup` -- `Escape`.
    ///
    /// The arrows and W/A/S/D genuinely overlap. Consumption does NOT resolve
    /// that: `count_and_consume_key` removes the key EVENT and leaves
    /// `keys_down` alone, and the pan axes here are read from `keys_down`
    /// because a held key produces no further events. Keyboard FOCUS is what
    /// resolves it, and both of the other readers claim it -- the picker
    /// through its filter field, the 3D canvas through `request_focus` while
    /// flying. The two tests below drive the real widgets to prove it, rather
    /// than trusting this comment.
    const KEYS_THIS_PANE_BINDS: &[egui::Key] = &[
        egui::Key::ArrowLeft,
        egui::Key::ArrowRight,
        egui::Key::ArrowUp,
        egui::Key::ArrowDown,
        egui::Key::A,
        egui::Key::D,
        egui::Key::W,
        egui::Key::S,
        egui::Key::Plus,
        egui::Key::Equals,
        egui::Key::Minus,
        egui::Key::Home,
    ];

    #[test]
    fn the_pane_binds_the_listed_keys_and_no_others() {
        // Every key in the list does something.
        for bound in KEYS_THIS_PANE_BINDS {
            let context = egui::Context::default();
            let nav = run_pass(&context, 1.0, vec![key(*bound, true)], |ui| {
                keyboard_nav(ui, true)
            });
            assert!(!nav.is_idle(), "{bound:?} is listed but does nothing");
        }
        // And the keys the rest of the application owns do not.
        for reserved in [
            egui::Key::Enter,
            egui::Key::Escape,
            egui::Key::Tab,
            egui::Key::Space,
            egui::Key::E,
            egui::Key::Q,
            egui::Key::PageUp,
            egui::Key::PageDown,
            egui::Key::End,
            egui::Key::Delete,
            egui::Key::Backspace,
        ] {
            assert!(
                !KEYS_THIS_PANE_BINDS.contains(&reserved),
                "{reserved:?} is spoken for elsewhere"
            );
            let context = egui::Context::default();
            let nav = run_pass(&context, 1.0, vec![key(reserved, true)], |ui| {
                keyboard_nav(ui, true)
            });
            assert!(nav.is_idle(), "{reserved:?} moved the camera");
        }
    }

    /// The collision that matters: the picker walks its list with the arrows
    /// while the pane pans with them.
    ///
    /// Driven through the REAL picker, because the thing being checked is a
    /// property of how the two interact and not of either one alone. The
    /// picker is drawn first, exactly as `WorkstationApp::ui` draws the toolbar
    /// before the canvas.
    ///
    /// Consumption alone would NOT be enough -- the picker takes the arrow
    /// events but `keys_down` still reports them, and the pan axes are read
    /// from `keys_down` -- so what this really pins is that the picker's filter
    /// field holds the keyboard the whole time it is open.
    #[test]
    fn the_product_picker_keeps_the_arrows_while_it_is_open() {
        use crate::product::DisplayProduct;
        use crate::product_availability::ProductAvailabilityIndex;
        use crate::product_picker::{ProductPickerInput, ProductPickerState, draw_product_picker};
        use color_tables::ColorTableSet;

        let context = egui::Context::default();
        let mut state = ProductPickerState::default();
        state.opened(DisplayProduct::Reflectivity);
        let availability = ProductAvailabilityIndex::unrestricted();
        let tables = ColorTableSet::default();

        // Several frames: the one the picker opens on, and the steady state
        // after. The first is the interesting one, because the filter field
        // claims focus during that very pass.
        for frame in 0..4 {
            let nav = run_pass(
                &context,
                1.0 + f64::from(frame) * 0.016,
                vec![
                    key(egui::Key::ArrowDown, true),
                    key(egui::Key::ArrowRight, true),
                    key(egui::Key::S, true),
                ],
                |ui| {
                    let _ = draw_product_picker(
                        ui,
                        ProductPickerInput {
                            state: &mut state,
                            current: DisplayProduct::Reflectivity,
                            availability: &availability,
                            tables: &tables,
                            show_experimental: false,
                        },
                    );
                    keyboard_nav(ui, true)
                },
            );
            assert!(
                nav.is_idle(),
                "frame {frame}: the picker was open and the camera moved anyway: {nav:?}"
            );
        }
    }

    /// The same gate, from the other direction: any focused widget silences
    /// the pane, not only a text field.
    ///
    /// This is precisely what `vol3d::camera::drive_camera` relies on. It takes
    /// focus on a plain canvas -- not a `TextEdit` -- while the operator is
    /// flying the 3D box with W/A/S/D, and it is drawn before the panes. If
    /// this gate were narrowed to text fields, one held W would fly the 3D
    /// camera and pan the map behind it at the same time.
    #[test]
    fn a_focused_canvas_stands_the_pane_down_too() {
        let context = egui::Context::default();
        let canvas = egui::Id::new("a-canvas-that-is-not-a-text-field");
        let nav = run_pass(&context, 1.0, vec![key(egui::Key::W, true)], |ui| {
            ui.ctx().memory_mut(|memory| memory.request_focus(canvas));
            assert!(!ui.ctx().text_edit_focused(), "this must not be a TextEdit");
            keyboard_nav(ui, true)
        });
        assert!(nav.is_idle(), "{nav:?}");
    }

    /// Everything above drives the two input readers directly. This drives
    /// `draw_pane`, which is what the application calls.
    ///
    /// It is the only test here that proves the wiring: that the wheel is read
    /// at all (it is behind `response.hovered()`), that the pointer rather than
    /// the pane centre is the anchor, that the pane-local coordinate is the one
    /// handed to the camera, and that the moved camera comes back out through
    /// `PaneInteraction` so the workspace can link it to the other panes.
    ///
    /// Two passes because egui resolves hover against the widget rects
    /// registered on the PREVIOUS pass, so the first pass is what puts this
    /// pane under the pointer at all.
    fn pane_frames(
        context: &egui::Context,
        rect: egui::Rect,
        camera: Camera2D,
        active: bool,
        frames: Vec<(f64, Vec<egui::Event>)>,
    ) -> PaneInteraction {
        let map = PaneMap::default();
        let overlay = PaneOverlay {
            legend: None,
            table: None,
            product_name: "REF",
            badges: &[],
            probe: None,
        };
        let mut camera = camera;
        let mut last = None;
        for (time, events) in frames {
            let interaction = run_pass(context, time, events, |ui| {
                draw_pane(
                    ui,
                    pane_zero(),
                    rect,
                    active,
                    camera,
                    NavTuning::default(),
                    None,
                    &map,
                    "KEAX 0.5 REF",
                    "",
                    &overlay,
                )
            });
            camera = interaction.camera;
            last = Some(interaction);
        }
        last.expect("at least one frame")
    }

    #[test]
    fn a_wheel_notch_over_the_pane_zooms_about_the_pointer() {
        let context = egui::Context::default();
        // Offset from the window origin, so a test that confused window
        // coordinates with pane-local ones would anchor in the wrong place.
        let rect = egui::Rect::from_min_size(egui::pos2(120.0, 60.0), egui::vec2(800.0, 700.0));
        let cursor = egui::pos2(831.0, 156.0);
        let start = Camera2D::default();
        let viewport = ViewportMetrics {
            width_points: rect.width(),
            height_points: rect.height(),
            pixels_per_point: 1.0,
        };
        let local = ScreenPoint::new(cursor.x - rect.left(), cursor.y - rect.top());
        let under_cursor = start.screen_to_world(local, viewport);

        let interaction = pane_frames(
            &context,
            rect,
            start,
            true,
            vec![
                (1.0, vec![egui::Event::PointerMoved(cursor)]),
                (
                    1.016,
                    vec![
                        egui::Event::PointerMoved(cursor),
                        wheel(egui::MouseWheelUnit::Line, 1.0, egui::Modifiers::NONE),
                    ],
                ),
            ],
        );

        assert!(interaction.camera_changed, "the pane reported no change");
        assert_eq!(interaction.viewport, viewport);
        let expected = DEFAULT_KM_PER_POINT / ZOOM_PER_NOTCH;
        assert!(
            (interaction.camera.km_per_point / expected - 1.0).abs() < 1.0e-4,
            "one notch gave {}, wanted {expected}",
            interaction.camera.km_per_point
        );
        // The anchor: whatever was under the pointer is still under it. Checked
        // through the forward transform, so it is the picture that is being
        // measured and not the correction that produced it.
        let back = interaction.camera.world_to_screen(under_cursor, viewport);
        let slip = (back.x - local.x).hypot(back.y - local.y);
        assert!(slip < 0.01, "the map slid {slip} points under the pointer");
        // Anchoring on the pointer is the whole point: the pane centre would
        // have given a different camera for the same notch.
        let mut centred = start;
        centred.zoom_about(ZOOM_PER_NOTCH, viewport.center(), viewport);
        assert_ne!(
            centred.center_east_km, interaction.camera.center_east_km,
            "the zoom was anchored on the pane centre, not the pointer"
        );
    }

    /// The keyboard reaches the camera through the same return path, and only
    /// for the pane the analyst is working in.
    #[test]
    fn a_held_pan_key_flies_the_active_pane_and_only_that_pane() {
        let rect = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(1000.0, 800.0));
        let held = vec![
            (1.0_f64, vec![key(egui::Key::ArrowRight, true)]),
            // Still down, no new event: a held key produces none, which is why
            // the pan axes are read from `keys_down` rather than from events.
            (1.016, Vec::new()),
        ];

        let context = egui::Context::default();
        let flown = pane_frames(&context, rect, Camera2D::default(), true, held.clone());
        assert!(flown.camera_changed);
        assert!(
            flown.camera.center_east_km > 0.0,
            "the active pane did not fly east: {:?}",
            flown.camera
        );

        let context = egui::Context::default();
        let still = pane_frames(&context, rect, Camera2D::default(), false, held);
        assert_eq!(
            still.camera,
            Camera2D::default(),
            "an inactive pane flew: four panes would pan at once"
        );
    }
}

/// What the pane actually paints, read off a real egui frame. A sibling file,
/// because this module keeps growing and `pane_canvas.rs` is close to the
/// 2 000-line cap `tests/architecture.rs` enforces.
#[cfg(test)]
mod chrome_tests;
