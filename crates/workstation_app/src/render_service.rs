use std::sync::Arc;
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::Instant;

use analyst_runtime::{
    Camera2D, LatestLaneSender, PaneId, RenderStamp, StormMotionIntent, ViewportMetrics,
    latest_lane_channel,
};
use color_tables::ColorTableSet;
use eframe::egui;
use radar_core::RadarVolume;
use render2d::derived::compute::{INTERACTIVE_SPACING_KM, compute_volume_field};
use render2d::derived::field::{FieldRasterOptions, field_rgba_buffer_len, render_field_rgba_into};
use render2d::quality::render_moment_viewport_quality_rgba_into;
use render2d::sweep_blend::{
    DealiasedSweepBlend, SweepBlend, SweepBlendCensor,
    render_storm_relative_sweep_blend_rgba_into_censored, render_sweep_blend_rgba_into_censored,
};
use render2d::{
    DisplayQuality, GateFilter, GateFilterReport, StormMotion, ViewportMomentCache,
    ViewportRasterOptions, evaluate_gate_filter, viewport_rgba_buffer_len,
};

use crate::product::DisplayProduct;

const RESULT_QUEUE_CAPACITY: usize = 8;

pub struct RenderRequest {
    pub pane: PaneId,
    pub stamp: RenderStamp,
    pub volume: Arc<RadarVolume>,
    /// Measured once per frame by the app; volume products select their tilts
    /// from it rather than measuring the volume again per render.
    pub capabilities: Arc<product_engine::VolumeCapabilities>,
    /// Thermal levels for the hail products. Always present - the fallback is
    /// itself an environment, and it says so on the pane.
    pub environment: product_engine::HailEnvironment,
    pub cut_index: usize,
    pub product: DisplayProduct,
    pub camera: Camera2D,
    pub viewport: ViewportMetrics,
    pub storm_motion: StormMotionIntent,
    pub color_tables: Arc<ColorTableSet>,
    /// How hard to work on this frame: grid softening, polar upsampling and
    /// supersampling. Carried per request rather than read from a global so a
    /// prewarm or an export can ask for a quality the live panes are not using.
    pub quality: DisplayQuality,
    /// Which gates this pane is allowed to draw - the criteria the analyst set,
    /// carried per request for the same reason `quality` is, so an export or a
    /// prewarm can ask for a picture the live panes are not showing.
    ///
    /// [`GateFilter::OFF`] is the shipped value and costs nothing: the filter
    /// returns before it reads a gate, so an unfiltered pane renders down the
    /// same path it always did, byte for byte. When it is anything else the
    /// pane MUST say so on screen - see [`RenderedPane::gate_filter`], which
    /// carries back exactly what was hidden.
    pub gate_filter: GateFilter,
    /// Set while the tilt on screen is still arriving. Present means "paint the
    /// part of the sweep that has landed over the last complete picture of the
    /// same tilt"; absent means the ordinary single-sweep raster.
    pub sweep: Option<SweepBlendRequest>,
}

/// Why a volume-derived product does not obey a per-gate filter, in the words
/// the analyst is shown.
///
/// One constant rather than two literals because two indicators quote it: the
/// engine's [`GateFilterReport::badge`], via
/// [`GateFilterReport::not_applicable`] in [`render_derived`], and the pane's
/// own band, via `crate::gate_filter_ui::pane_banner_text_for`. A pane that
/// described its own state differently from the report it was handed would be
/// the exact failure the gate filter's safety rule is written against, and a
/// shared constant makes the two unable to drift.
pub const DERIVED_PRODUCT_NOT_FILTERED: &str =
    "this product is integrated from the whole volume, not rastered from one sweep";

/// What a partially-arrived sweep needs in order to be drawn over the last
/// complete one.
///
/// The previous sweep is carried as a whole volume plus a cut index rather than
/// as borrowed grids because the render runs on a worker thread: an `Arc` keeps
/// the frame alive for exactly as long as the render that is reading it, even
/// if history evicts it in the meantime.
pub struct SweepBlendRequest {
    pub previous_volume: Arc<RadarVolume>,
    pub previous_cut_index: usize,
    /// Azimuth the arriving sweep started from, in degrees.
    pub start_deg: f32,
    /// Degrees clockwise from `start_deg` the reveal has reached. Never beyond
    /// the newest radial that has actually arrived - see `crate::sweep`.
    pub revealed_deg: f32,
}

pub struct RenderedPane {
    pub pane: PaneId,
    pub stamp: RenderStamp,
    pub camera: Camera2D,
    pub viewport: ViewportMetrics,
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
    pub elapsed_ms: f32,
    /// What the gate filter removed from this frame.
    ///
    /// [`GateFilterReport::INACTIVE`] whenever the request carried
    /// [`GateFilter::OFF`]. Anything else and the pane owes the analyst a badge
    /// naming what is hidden: a picture with gates removed must never be
    /// distinguishable from a picture with no echo only by the absence of echo.
    pub gate_filter: GateFilterReport,
}

struct RenderFailure {
    pane: PaneId,
    stamp: RenderStamp,
    message: String,
}

pub enum RenderUpdate {
    /// Boxed because a finished pane is now much larger than a failure: it
    /// carries the frame, the geometry it was drawn under, and the gate-filter
    /// report. One allocation on a path that has already allocated a megabyte
    /// of RGBA is not a cost worth paying an enum-sized channel slot for.
    Completed(Box<RenderedPane>),
    Failed {
        pane: PaneId,
        stamp: RenderStamp,
        message: String,
    },
}

pub struct RenderService {
    sender: LatestLaneSender<PaneId, RenderRequest>,
    receiver: Receiver<RenderUpdate>,
}

impl RenderService {
    pub fn new(context: egui::Context) -> Self {
        let (request_sender, request_receiver) = latest_lane_channel::<PaneId, RenderRequest>();
        let (result_sender, result_receiver) = mpsc::sync_channel(RESULT_QUEUE_CAPACITY);
        let _worker = thread::Builder::new()
            .name("radar-workstation-render".to_owned())
            .spawn(move || {
                while let Some((_pane, request)) = request_receiver.recv() {
                    let update = match render_request(request) {
                        Ok(rendered) => RenderUpdate::Completed(Box::new(rendered)),
                        Err(failure) => RenderUpdate::Failed {
                            pane: failure.pane,
                            stamp: failure.stamp,
                            message: failure.message,
                        },
                    };
                    if result_sender.send(update).is_err() {
                        break;
                    }
                    context.request_repaint();
                }
            })
            .expect("failed to start radar render worker");

        Self {
            sender: request_sender,
            receiver: result_receiver,
        }
    }

    /// Queue a render, newest-wins per pane.
    ///
    /// The rejected request comes back boxed: a `RenderRequest` now carries the
    /// volume capabilities and hail environment as well as the camera, and
    /// returning it by value would make every successful call pay for an
    /// error path that is only taken once, when the worker has gone.
    pub fn request(&self, request: RenderRequest) -> Result<(), Box<RenderRequest>> {
        self.sender
            .submit(request.pane, request)
            .map(|_| ())
            .map_err(|closed| Box::new(closed.0))
    }

    pub fn try_recv(&self) -> Option<RenderUpdate> {
        self.receiver.try_recv().ok()
    }

    pub fn queued_panes(&self) -> usize {
        self.sender.queued_lanes()
    }
}

/// Draw one pane.
///
/// # Where the gate filter joins
///
/// Gate censoring belongs to `render2d` - it is a decision about a gate, taken
/// where gates are read, and `render2d::gate_filter` documents the criteria and
/// their sources. This crate's job is the other three quarters of the feature:
/// carry the analyst's choice here, re-render when it moves, and say on the
/// pane that it is on.
///
/// `request.gate_filter` is not read in this function; it is read one call
/// deeper, by the filter-taking `ViewportMomentCache` constructors below and
/// by the censored `render_*_rgba_into_censored` calls in
/// [`render_blended_sweep`]. Every path out of here therefore either censors
/// with it or, in [`render_derived`], says in
/// [`RenderedPane::gate_filter`] that it could not - there is no path that
/// quietly ignores it.
///
/// What comes back out is [`RenderedPane::gate_filter`], and the pane is
/// required to put it on screen. An over-report - the badge on while nothing
/// is hidden - is a nuisance; the failure that matters is a censored picture
/// that says nothing, so nothing here is allowed to drop the report.
fn render_request(request: RenderRequest) -> Result<RenderedPane, RenderFailure> {
    if let Some(derived) = request.product.derived_volume() {
        return render_derived(request, derived);
    }
    let started = Instant::now();
    let raster_view = request.camera.radar_raster_view(request.viewport);
    let options = ViewportRasterOptions {
        width: raster_view.width_px,
        height: raster_view.height_px,
        radar_x_px: raster_view.radar_x_px,
        radar_y_px: raster_view.radar_y_px,
        km_per_px_x: raster_view.km_per_px,
        km_per_px_y: raster_view.km_per_px,
    };
    let mut rgba = vec![0_u8; viewport_rgba_buffer_len(options)];

    // A sweep still arriving is drawn as a blend over the last complete
    // picture of the same tilt. All four velocity products take this path, not
    // just the plain moment: the dealiased blend unfolds both layers, and the
    // storm-relative blend subtracts the same motion vector from both, so the
    // two halves of the picture stay in one reference frame.
    if let Some(blend) = request.sweep.as_ref() {
        return render_blended_sweep(&request, blend, options, rgba, started);
    }

    let cache = if request.product.uses_dealiased_velocity() {
        ViewportMomentCache::new_dealiased_velocity_display_quality_filtered(
            &request.volume,
            request.cut_index,
            &request.color_tables,
            request.quality,
            &request.gate_filter,
        )
    } else {
        ViewportMomentCache::new_display_quality_filtered(
            &request.volume,
            request.cut_index,
            request.product.source_moment(),
            &request.color_tables,
            request.quality,
            &request.gate_filter,
        )
    }
    .map_err(|error| RenderFailure {
        pane: request.pane,
        stamp: request.stamp,
        message: error.to_string(),
    })?;

    let dimensions = if request.product.is_storm_relative() {
        let direction_toward_deg =
            (request.storm_motion.direction_from_deg + 180.0).rem_euclid(360.0);
        cache.render_storm_relative_velocity_rgba_into(
            &request.volume,
            StormMotion {
                direction_deg: direction_toward_deg,
                speed_mps: request.storm_motion.speed_mps,
            },
            options,
            &mut rgba,
        )
    } else {
        // Supersampling is applied only here. The storm-relative path above
        // has no supersampled entry point yet, and a partly-supersampled
        // display would be worse than a consistent one: the two products would
        // disagree about how sharp the same storm is.
        render_moment_viewport_quality_rgba_into(
            &cache,
            &request.volume,
            options,
            request.quality.supersample,
            &mut rgba,
        )
    }
    .map_err(|error| RenderFailure {
        pane: request.pane,
        stamp: request.stamp,
        message: error.to_string(),
    })?;

    Ok(RenderedPane {
        pane: request.pane,
        stamp: request.stamp,
        camera: request.camera,
        viewport: request.viewport,
        width: dimensions.0,
        height: dimensions.1,
        rgba,
        elapsed_ms: started.elapsed().as_secs_f32() * 1_000.0,
        gate_filter: cache.gate_filter_report().clone(),
    })
}

/// Draw a volume-derived product.
///
/// The field is recomputed per request today. It is camera-independent, so a
/// pan recomputes something that did not change - the byte-bounded cache that
/// fixes that is the next piece of work, and this is honest about the cost
/// rather than pretending the field is cheap.
fn render_derived(
    request: RenderRequest,
    derived: product_engine::registry::DerivedVolumeId,
) -> Result<RenderedPane, RenderFailure> {
    let started = Instant::now();
    let raster_view = request.camera.radar_raster_view(request.viewport);
    let options = FieldRasterOptions {
        width: raster_view.width_px,
        height: raster_view.height_px,
        radar_x_px: raster_view.radar_x_px,
        radar_y_px: raster_view.radar_y_px,
        km_per_px_x: raster_view.km_per_px,
        km_per_px_y: raster_view.km_per_px,
    };

    let field = compute_volume_field(
        &request.volume,
        &request.capabilities,
        derived,
        &request.environment,
        INTERACTIVE_SPACING_KM,
    )
    .map_err(|error| RenderFailure {
        pane: request.pane,
        stamp: request.stamp,
        message: error.to_string(),
    })?;

    if field.plausibility.is_rejected() {
        // A field that failed its physical gate is never installed. Drawing it
        // would put a picture on screen that the application itself believes is
        // impossible.
        return Err(RenderFailure {
            pane: request.pane,
            stamp: request.stamp,
            message: format!("implausible field: {}", field.plausibility.summary()),
        });
    }

    let table = crate::palettes::table_for(request.product.descriptor(), &request.color_tables);
    let mut rgba = vec![0_u8; field_rgba_buffer_len(options)];
    let dimensions = render_field_rgba_into(&field, &table, options, &mut rgba);

    Ok(RenderedPane {
        pane: request.pane,
        stamp: request.stamp,
        camera: request.camera,
        viewport: request.viewport,
        width: dimensions.0,
        height: dimensions.1,
        rgba,
        elapsed_ms: started.elapsed().as_secs_f32() * 1_000.0,
        // A volume-derived field is integrated out of the whole volume by the
        // product engine, not rastered from one sweep, so a per-gate display
        // filter has nothing to attach to here. It is NOT reported as inactive:
        // an analyst with a filter switched on would otherwise watch the badge
        // vanish when they switched this pane to VIL and have no way to learn
        // that this one pane is not obeying the setting they can see enabled
        // everywhere else. The direction is safe - this pane shows more, never
        // less - but silence about it is not.
        gate_filter: GateFilterReport::not_applicable(
            request.gate_filter,
            DERIVED_PRODUCT_NOT_FILTERED,
        ),
    })
}

/// Draw a sweep that is still arriving over the last complete picture of the
/// same tilt.
///
/// The point is what a live pane looked like without it: a new tilt opens with
/// a few radials, so the pane went almost entirely blank and the storm appeared
/// to vanish until the cut closed. Here the arrived wedge is painted from the
/// new sweep and everything the antenna has not reached yet keeps showing the
/// previous sweep, so the picture is wiped rather than erased.
///
/// A missing previous sweep is not an error. The blend then paints the arrived
/// wedge and leaves the rest transparent, which is exactly the old behaviour -
/// correct for the first volume after a site change, when there is genuinely
/// nothing older to show.
///
/// The blend is deliberately NOT supersampled or upsampled. Both passes are
/// caches keyed to a finished sweep, and the arriving one changes several times
/// a second; rebuilding them per chunk would cost more than the frame is worth,
/// and the whole point of this path is that it keeps up with the antenna. The
/// closing frame of the tilt takes the ordinary path and arrives at full
/// quality.
fn render_blended_sweep(
    request: &RenderRequest,
    blend: &SweepBlendRequest,
    options: ViewportRasterOptions,
    mut rgba: Vec<u8>,
    started: Instant,
) -> Result<RenderedPane, RenderFailure> {
    let moment = request.product.source_moment();
    let fail = |message: String| RenderFailure {
        pane: request.pane,
        stamp: request.stamp,
        message,
    };

    let incoming = request
        .volume
        .cuts
        .get(request.cut_index)
        .ok_or_else(|| fail(format!("cut {} is gone", request.cut_index)))?;
    let incoming_grid = incoming.moments.get(&moment).ok_or_else(|| {
        fail(format!(
            "{} missing from the arriving sweep",
            request.product.id()
        ))
    })?;

    // The previous sweep is optional all the way down: anything wrong with it
    // costs the underpaint, never the frame. Failing here would blank a pane
    // over data that arrived perfectly well.
    let previous_cut = blend.previous_volume.cuts.get(blend.previous_cut_index);
    let previous = previous_cut.and_then(|cut| cut.moments.get(&moment).map(|grid| (cut, grid)));

    // Both halves of the blend are filtered, and they are filtered against
    // their OWN volumes, because the under-paint came from an earlier volume
    // whose companion sweeps are its own. Filtering only the arriving wedge
    // would leave the analyst looking at one picture with two different rules
    // in it, split along a line that moves with the antenna.
    //
    // The censor rides into the raster as a mask rather than as a blanked copy
    // of the sweep, which is what stops a removed gate being replaced by the
    // beam next to it. See `render2d::gate_filter`.
    let incoming_outcome = evaluate_gate_filter(
        &request.volume,
        request.cut_index,
        incoming_grid,
        &request.gate_filter,
    );
    let incoming_report = incoming_outcome.report;
    let previous_mask = previous.and_then(|(_, grid)| {
        evaluate_gate_filter(
            &blend.previous_volume,
            blend.previous_cut_index,
            grid,
            &request.gate_filter,
        )
        .mask
    });

    // The storm motion the analyst set, expressed the way the renderer wants
    // it: the direction the storm is moving TOWARD.
    let storm_motion = StormMotion {
        direction_deg: (request.storm_motion.direction_from_deg + 180.0).rem_euclid(360.0),
        speed_mps: request.storm_motion.speed_mps,
    };

    // The dealiased blend unfolds before it censors, for the reason
    // `ViewportMomentCache::new_dealiased_velocity_filtered` gives: a threshold
    // slider must not be able to rewrite the velocity of gates it does not
    // hide.
    let mut dealiased_report = GateFilterReport::INACTIVE;
    let dimensions = if request.product.uses_dealiased_velocity() {
        // Bound to a local: the SweepBlend borrows the unfolded grids out of it.
        let unfolded = render2d::sweep_blend::dealias_cut_velocity(incoming)
            .ok_or_else(|| fail("no velocity in the arriving sweep to unfold".to_owned()))?;
        let outcome = evaluate_gate_filter(
            &request.volume,
            request.cut_index,
            &unfolded,
            &request.gate_filter,
        );
        dealiased_report = outcome.report;
        let unfolded_previous = previous_cut.and_then(|cut| {
            let unfolded = render2d::sweep_blend::dealias_cut_velocity(cut)?;
            let mask = evaluate_gate_filter(
                &blend.previous_volume,
                blend.previous_cut_index,
                &unfolded,
                &request.gate_filter,
            )
            .mask;
            Some((cut, unfolded, mask))
        });
        let (previous_pair, previous_mask) = match unfolded_previous {
            Some((cut, grid, mask)) => (Some((cut, grid)), mask),
            None => (None, None),
        };
        let censor = SweepBlendCensor {
            incoming: outcome.mask.as_ref(),
            previous: previous_mask.as_ref(),
        };
        let dealiased = DealiasedSweepBlend::from_unfolded_grids(
            incoming,
            unfolded,
            previous_pair,
            blend.start_deg,
            blend.revealed_deg,
        );
        let blend = dealiased.blend();
        if request.product.is_storm_relative() {
            render_storm_relative_sweep_blend_rgba_into_censored(
                &blend,
                censor,
                storm_motion,
                options,
                &request.color_tables,
                &mut rgba,
            )
        } else {
            render_sweep_blend_rgba_into_censored(
                &blend,
                censor,
                options,
                &request.color_tables,
                &mut rgba,
            )
        }
    } else {
        let censor = SweepBlendCensor {
            incoming: incoming_outcome.mask.as_ref(),
            previous: previous_mask.as_ref(),
        };
        let blend = SweepBlend {
            incoming,
            incoming_grid,
            previous,
            start_deg: blend.start_deg,
            revealed_deg: blend.revealed_deg,
        };
        if request.product.is_storm_relative() {
            render_storm_relative_sweep_blend_rgba_into_censored(
                &blend,
                censor,
                storm_motion,
                options,
                &request.color_tables,
                &mut rgba,
            )
        } else {
            render_sweep_blend_rgba_into_censored(
                &blend,
                censor,
                options,
                &request.color_tables,
                &mut rgba,
            )
        }
    }
    .map_err(|error| fail(error.to_string()))?;

    Ok(RenderedPane {
        pane: request.pane,
        stamp: request.stamp,
        camera: request.camera,
        viewport: request.viewport,
        width: dimensions.0,
        height: dimensions.1,
        rgba,
        elapsed_ms: started.elapsed().as_secs_f32() * 1_000.0,
        // The report describes the ARRIVING sweep. The under-paint is filtered
        // by the same rule but is not counted, because the badge is about what
        // the analyst is being shown of this tilt, not about how many gates two
        // volumes lost between them.
        gate_filter: if request.product.uses_dealiased_velocity() {
            dealiased_report
        } else {
            incoming_report
        },
    })
}
