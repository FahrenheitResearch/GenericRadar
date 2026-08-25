//! Retained, background-fed surface station models and real station history.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use analyst_runtime::{Camera2D, ScreenPoint, ViewportMetrics};
use chrono::{DateTime, Utc};
use data_source::observations::{self, Observation, ObservationPool, SkyCover};
use eframe::egui::{self, Align2, Color32, FontId, Pos2, Rect, Stroke, Vec2, pos2, vec2};
use map_scene::RadarProjection;

const METAR_COLOR: Color32 = Color32::from_rgb(235, 241, 248);
const MESONET_COLOR: Color32 = Color32::from_rgb(255, 193, 91);
const TEMPERATURE_COLOR: Color32 = Color32::from_rgb(255, 103, 104);
const DEWPOINT_COLOR: Color32 = Color32::from_rgb(96, 226, 134);
const WEATHER_COLOR: Color32 = Color32::from_rgb(220, 178, 255);
const PRESSURE_COLOR: Color32 = Color32::from_rgb(190, 215, 255);
const PLOT_BACKGROUND: Color32 = Color32::from_rgb(12, 16, 22);

/// Independent professional station-model controls. Temperature storage is
/// always SI; Fahrenheit affects display only and matches U.S. radar practice.
#[derive(Clone, Debug)]
pub(crate) struct ObservationPlotOptions {
    pub show_temperature: bool,
    pub show_dewpoint: bool,
    pub show_wind_barbs: bool,
    pub show_station_id: bool,
    pub show_sky_cover: bool,
    pub show_weather: bool,
    pub show_visibility: bool,
    pub show_pressure: bool,
    pub show_gusts: bool,
    pub declutter_px: f32,
    pub fahrenheit: bool,
}

impl Default for ObservationPlotOptions {
    fn default() -> Self {
        Self {
            show_temperature: true,
            show_dewpoint: true,
            show_wind_barbs: true,
            show_station_id: true,
            show_sky_cover: true,
            show_weather: true,
            show_visibility: false,
            show_pressure: false,
            show_gusts: true,
            declutter_px: 76.0,
            fahrenheit: true,
        }
    }
}

enum WorkerCommand {
    Enabled(bool),
    Refresh,
    MesonetEnabled(bool),
    RefreshInterval(Duration),
    History {
        station: String,
        hours: u8,
        frame_time: Option<DateTime<Utc>>,
    },
    Shutdown,
}

enum WorkerResult {
    Current(Result<Vec<Observation>, String>),
    Mesonet(Vec<Observation>),
    History {
        station: String,
        hours: u8,
        result: Result<Vec<Observation>, String>,
    },
}

pub(crate) struct SurfaceObservationService {
    commands: mpsc::Sender<WorkerCommand>,
    results: mpsc::Receiver<WorkerResult>,
    observations: ObservationPool,
    options: ObservationPlotOptions,
    enabled: bool,
    mesonet_enabled: bool,
    status: String,
    history_station: Option<String>,
    history: Vec<Observation>,
    history_hours: u8,
    history_open: bool,
    history_pending: bool,
    history_error: Option<String>,
}

impl SurfaceObservationService {
    pub(crate) fn new(context: egui::Context) -> Self {
        let (commands, command_receiver) = mpsc::channel();
        let (result_sender, results) = mpsc::channel();
        let _ = std::thread::Builder::new()
            .name("surface-observations".to_owned())
            .spawn(move || observation_worker(command_receiver, result_sender, context));
        Self {
            commands,
            results,
            observations: ObservationPool::new(),
            options: ObservationPlotOptions::default(),
            enabled: false,
            mesonet_enabled: false,
            status: "Surface observations disabled".to_owned(),
            history_station: None,
            history: Vec::new(),
            history_hours: 12,
            history_open: false,
            history_pending: false,
            history_error: None,
        }
    }

    pub(crate) fn set_enabled(&mut self, enabled: bool) {
        if self.enabled == enabled {
            return;
        }
        self.enabled = enabled;
        self.status = if enabled {
            "Loading worldwide surface observations…".to_owned()
        } else {
            "Surface observations disabled".to_owned()
        };
        let _ = self.commands.send(WorkerCommand::Enabled(enabled));
    }

    pub(crate) fn set_mesonet_enabled(&mut self, enabled: bool) {
        if self.mesonet_enabled == enabled {
            return;
        }
        self.mesonet_enabled = enabled;
        let _ = self.commands.send(WorkerCommand::MesonetEnabled(enabled));
    }

    pub(crate) fn refresh(&mut self) {
        if self.enabled {
            self.status = "Refreshing station observations…".to_owned();
            let _ = self.commands.send(WorkerCommand::Refresh);
        }
    }

    pub(crate) fn poll(&mut self) {
        while let Ok(result) = self.results.try_recv() {
            match result {
                WorkerResult::Current(Ok(observations)) => {
                    self.observations.merge(observations);
                    self.status = format!(
                        "{} reporting stations • updated {} UTC",
                        self.observations.station_count(),
                        Utc::now().format("%H:%M")
                    );
                }
                WorkerResult::Current(Err(error)) => {
                    self.status = if self.observations.is_empty() {
                        error
                    } else {
                        format!(
                            "{error} • retained {} stations",
                            self.observations.station_count()
                        )
                    };
                }
                WorkerResult::Mesonet(observations) => {
                    let count = observations.len();
                    if self.mesonet_enabled {
                        self.observations.merge(observations);
                        self.status = format!(
                            "{} reporting stations • {count} mesonet reports",
                            self.observations.station_count()
                        );
                    }
                }
                WorkerResult::History {
                    station,
                    hours,
                    result,
                } if self.history_station.as_deref() == Some(station.as_str())
                    && self.history_hours == hours =>
                {
                    self.history_pending = false;
                    match result {
                        Ok(observations) => {
                            self.history = observations;
                            self.history_error = None;
                        }
                        Err(error) => self.history_error = Some(error),
                    }
                }
                WorkerResult::History { .. } => {}
            }
        }
    }

    pub(crate) fn station_count(&self) -> usize {
        self.observations.station_count()
    }

    pub(crate) fn status(&self) -> &str {
        &self.status
    }

    pub(crate) fn set_plot_options(&mut self, options: ObservationPlotOptions) {
        self.options = options;
    }

    pub(crate) fn set_refresh_interval(&mut self, interval: Duration) {
        let interval = interval.max(Duration::from_secs(30));
        let _ = self.commands.send(WorkerCommand::RefreshInterval(interval));
    }

    pub(crate) fn request_station_history(&mut self, station_id: &str) {
        self.request_station_history_at(station_id, None);
    }

    pub(crate) fn request_station_history_at(
        &mut self,
        station_id: &str,
        frame_time: Option<DateTime<Utc>>,
    ) {
        let station_id = station_id.trim().to_ascii_uppercase();
        if station_id.is_empty() {
            return;
        }
        if self.history_station.as_deref() != Some(station_id.as_str()) {
            self.history.clear();
        }
        self.history_station = Some(station_id.clone());
        self.history_open = true;
        self.history_pending = true;
        self.history_error = None;
        let _ = self.commands.send(WorkerCommand::History {
            station: station_id,
            hours: self.history_hours,
            frame_time,
        });
    }

    pub(crate) fn selected_station(&self) -> Option<&str> {
        self.history_station.as_deref()
    }

    /// Draw retained vector station models and return a clicked station.
    /// Projection and clipping follow the radar pane's exact blended-globe
    /// pipeline; stations behind the globe are never wrapped onto its face.
    pub(crate) fn draw(
        &self,
        ui: &mut egui::Ui,
        rect: Rect,
        projection: Option<&RadarProjection>,
        camera: Camera2D,
        viewport: ViewportMetrics,
        frame_time: Option<DateTime<Utc>>,
    ) -> Option<String> {
        if !self.enabled || self.observations.is_empty() {
            return None;
        }
        let projection = projection?;
        let camera = camera.sanitized();
        let blend = map_scene::projection::globe::blend_for_pane(camera.km_per_point, viewport);
        let now = Utc::now();
        let radar_time = frame_time.unwrap_or(now);
        let archived_mismatch = frame_time
            .map(|time| (now - time).num_minutes().unsigned_abs() > 90)
            .unwrap_or(false);
        let plot_time = if archived_mismatch { now } else { radar_time };
        let painter = ui.painter_at(rect);
        let click = ui.ctx().input(|input| {
            (input.pointer.primary_clicked() && !input.modifiers.ctrl && !input.modifiers.command)
                .then(|| input.pointer.interact_pos())
                .flatten()
                .filter(|point| rect.contains(*point))
        });
        let center_world = camera.screen_to_world(
            ScreenPoint::new(viewport.width_points * 0.5, viewport.height_points * 0.5),
            viewport,
        );
        let center_geo = projection.globe_to_lon_lat(center_world, blend);
        let radius_km =
            viewport.width_points.hypot(viewport.height_points) * camera.km_per_point * 0.56;
        let expanded = rect.expand(28.0);

        // Geographic prefilter prevents expensive ellipsoid projections for
        // thousands of worldwide stations when a radar-sized map is visible.
        let mut candidates = self
            .observations
            .frame_observations(plot_time)
            .filter(|observation| self.mesonet_enabled || observation.network == "METAR")
            .filter(|observation| likely_visible(observation, center_geo, radius_km))
            .filter_map(|observation| {
                let world = projection.try_lon_lat_to_globe(
                    f64::from(observation.lon),
                    f64::from(observation.lat),
                    blend,
                )?;
                let point = camera.world_to_screen(world, viewport);
                if !point.x.is_finite() || !point.y.is_finite() {
                    return None;
                }
                let position = rect.min + vec2(point.x, point.y);
                expanded
                    .contains(position)
                    .then_some((observation, position))
            })
            .collect::<Vec<_>>();
        candidates.sort_unstable_by(|(left, _), (right, _)| {
            right
                .completeness
                .cmp(&left.completeness)
                .then_with(|| (right.network == "METAR").cmp(&(left.network == "METAR")))
                .then_with(|| right.time_utc.cmp(&left.time_utc))
                .then_with(|| left.station_id.cmp(&right.station_id))
        });

        let cell = self.options.declutter_px.max(1.0);
        let columns = (rect.width() / cell).ceil() as i32 + 2;
        let visible_cell_count = ((rect.width() / cell).ceil() as usize + 2)
            .saturating_mul((rect.height() / cell).ceil() as usize + 2);
        let mut occupied = HashMap::with_capacity(visible_cell_count);
        let mut clicked: Option<(&Observation, f32)> = None;

        for (observation, position) in candidates {
            let column = ((position.x - rect.left()) / cell).floor() as i32;
            let row = ((position.y - rect.top()) / cell).floor() as i32;
            let key = row * columns + column;
            if occupied.contains_key(&key) {
                if rect.contains(position) {
                    painter.circle_filled(
                        position,
                        1.2,
                        station_color(observation).gamma_multiply(0.54),
                    );
                }
                continue;
            }
            occupied.insert(key, ());
            draw_station_model(
                &painter,
                observation,
                position,
                &self.options,
                camera.rotation_rad,
            );
            if let Some(pointer) = click {
                let distance_sq = pointer.distance_sq(position);
                if distance_sq <= 14.0_f32.powi(2)
                    && clicked.is_none_or(|(_, best)| distance_sq < best)
                {
                    clicked = Some((observation, distance_sq));
                }
            }
        }

        if archived_mismatch {
            let label = format!(
                "LIVE OBS {} UTC • radar {} UTC",
                now.format("%H:%M"),
                radar_time.format("%Y-%m-%d %H:%M")
            );
            painter.text(
                rect.left_bottom() + vec2(8.0, -8.0),
                Align2::LEFT_BOTTOM,
                label,
                FontId::proportional(11.0),
                MESONET_COLOR,
            );
        }
        clicked.map(|(observation, _)| observation.station_id.clone())
    }

    pub(crate) fn history_window(&mut self, context: &egui::Context) {
        if !self.history_open {
            return;
        }
        let Some(station) = self.history_station.clone() else {
            return;
        };
        let mut open = self.history_open;
        let mut new_hours = None;
        let mut refresh = false;
        egui::Window::new(format!("{station} — station observation history"))
            .id(egui::Id::new("surface-station-history"))
            .open(&mut open)
            .default_width(860.0)
            .default_height(440.0)
            .resizable(true)
            .show(context, |ui| {
                ui.horizontal(|ui| {
                    ui.label("Lookback:");
                    for hours in [3, 6, 12, 24, 48, 72] {
                        if ui
                            .selectable_label(self.history_hours == hours, format!("{hours} h"))
                            .clicked()
                        {
                            new_hours = Some(hours);
                        }
                    }
                    ui.separator();
                    if ui.button("Refresh").clicked() {
                        refresh = true;
                    }
                    ui.weak("All report times UTC • observed values only");
                });
                if self.history_pending {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label(format!(
                            "Loading {} hours of actual {station} reports…",
                            self.history_hours
                        ));
                    });
                }
                if let Some(error) = &self.history_error {
                    ui.colored_label(TEMPERATURE_COLOR, error);
                }
                if self.history.is_empty() {
                    if !self.history_pending && self.history_error.is_none() {
                        ui.label("No station reports were returned for this time period.");
                    }
                    return;
                }
                draw_history_trend(ui, &self.history, self.options.fahrenheit);
                ui.separator();
                ui.label(format!(
                    "{} actual station reports, newest first",
                    self.history.len()
                ));
                egui::ScrollArea::both()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        egui::Grid::new("station-observation-history-grid")
                            .striped(true)
                            .spacing([14.0, 5.0])
                            .show(ui, |ui| {
                                for heading in [
                                    "Time UTC",
                                    "Temp",
                                    "Dewpoint",
                                    "Wind",
                                    "Gust",
                                    "Visibility",
                                    "Pressure",
                                    "Sky / ceiling",
                                    "Weather",
                                    "Raw METAR",
                                ] {
                                    ui.strong(heading);
                                }
                                ui.end_row();
                                for observation in self.history.iter().rev() {
                                    ui.monospace(
                                        observation.time_utc.format("%m-%d %H:%M").to_string(),
                                    );
                                    color_optional_temperature(
                                        ui,
                                        observation.temp_c,
                                        self.options.fahrenheit,
                                        TEMPERATURE_COLOR,
                                    );
                                    color_optional_temperature(
                                        ui,
                                        observation.dewpoint_c,
                                        self.options.fahrenheit,
                                        DEWPOINT_COLOR,
                                    );
                                    ui.monospace(format_history_wind(observation));
                                    ui.monospace(optional_with_unit(
                                        observation.wind_gust_kt,
                                        "kt",
                                        0,
                                    ));
                                    ui.monospace(optional_with_unit(
                                        observation.visibility_sm,
                                        "mi",
                                        1,
                                    ));
                                    ui.monospace(if let Some(pressure) = observation.mslp_hpa {
                                        format!("{pressure:.1} hPa")
                                    } else {
                                        optional_with_unit(observation.altim_in_hg, "inHg", 2)
                                    });
                                    ui.monospace(format_history_sky(observation));
                                    ui.label(observation.present_weather.as_deref().unwrap_or("—"));
                                    ui.monospace(observation.raw_metar.as_deref().unwrap_or("—"));
                                    ui.end_row();
                                }
                            });
                    });
            });
        self.history_open = open;
        if let Some(hours) = new_hours {
            self.history_hours = hours;
            self.request_station_history(&station);
        } else if refresh {
            self.request_station_history(&station);
        }
    }
}

impl Drop for SurfaceObservationService {
    fn drop(&mut self) {
        let _ = self.commands.send(WorkerCommand::Shutdown);
    }
}

fn observation_worker(
    commands: mpsc::Receiver<WorkerCommand>,
    results: mpsc::Sender<WorkerResult>,
    context: egui::Context,
) {
    let mut enabled = false;
    let mut mesonet_enabled = false;
    let mut interval = Duration::from_secs(300);
    let mut next_refresh = None;
    let mesonet_in_flight = Arc::new(AtomicBool::new(false));

    loop {
        let timeout = if enabled {
            next_refresh.map_or(Duration::ZERO, |next: Instant| {
                next.saturating_duration_since(Instant::now())
            })
        } else {
            Duration::from_secs(3600)
        };
        match commands.recv_timeout(timeout) {
            Ok(WorkerCommand::Enabled(value)) => {
                enabled = value;
                next_refresh = value.then(Instant::now);
            }
            Ok(WorkerCommand::Refresh) if enabled => next_refresh = Some(Instant::now()),
            Ok(WorkerCommand::Refresh) => {}
            Ok(WorkerCommand::MesonetEnabled(value)) => {
                mesonet_enabled = value;
                if value && enabled {
                    spawn_mesonet_fetch(&results, &context, &mesonet_in_flight);
                }
            }
            Ok(WorkerCommand::RefreshInterval(value)) => {
                interval = value.max(Duration::from_secs(30));
            }
            Ok(WorkerCommand::History {
                station,
                hours,
                frame_time,
            }) => {
                let result = match frame_time {
                    Some(time) if (Utc::now() - time).num_hours().unsigned_abs() > 2 => {
                        observations::fetch_station_history_at(&station, time, hours)
                    }
                    _ => observations::fetch_station_history(&station, hours),
                };
                let _ = results.send(WorkerResult::History {
                    station,
                    hours,
                    result,
                });
                context.request_repaint();
            }
            Ok(WorkerCommand::Shutdown) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Err(mpsc::RecvTimeoutError::Timeout) if enabled => {
                let result = observations::fetch_current_observations();
                let _ = results.send(WorkerResult::Current(result));
                context.request_repaint();
                next_refresh = Some(Instant::now() + interval);
                if mesonet_enabled {
                    spawn_mesonet_fetch(&results, &context, &mesonet_in_flight);
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
    }
}

fn spawn_mesonet_fetch(
    results: &mpsc::Sender<WorkerResult>,
    context: &egui::Context,
    in_flight: &Arc<AtomicBool>,
) {
    if in_flight.swap(true, Ordering::AcqRel) {
        return;
    }
    let results = results.clone();
    let context = context.clone();
    let in_flight = Arc::clone(in_flight);
    let result = std::thread::Builder::new()
        .name("surface-mesonet-networks".to_owned())
        .spawn(move || {
            let observations = observations::fetch_mesonet_observations();
            let _ = results.send(WorkerResult::Mesonet(observations));
            in_flight.store(false, Ordering::Release);
            context.request_repaint();
        });
    if result.is_err() {
        // Thread creation failed; don't permanently suppress future retries.
        // The captured Arc was dropped along with the failed closure.
    }
}

fn likely_visible(observation: &Observation, center: Option<(f64, f64)>, radius_km: f32) -> bool {
    let Some((center_lon, center_lat)) = center else {
        return true;
    };
    if !radius_km.is_finite() || radius_km >= 6000.0 {
        return true;
    }
    let lat_radius = f64::from(radius_km / 106.0 + 1.0);
    let lat_difference = (f64::from(observation.lat) - center_lat).abs();
    if lat_difference > lat_radius {
        return false;
    }
    let cosine = center_lat.to_radians().cos().abs().max(0.12);
    let lon_radius = (lat_radius / cosine).min(180.0);
    let mut lon_difference = (f64::from(observation.lon) - center_lon).abs();
    if lon_difference > 180.0 {
        lon_difference = 360.0 - lon_difference;
    }
    lon_difference <= lon_radius
}

fn station_color(observation: &Observation) -> Color32 {
    if observation.network == "METAR" {
        METAR_COLOR
    } else {
        MESONET_COLOR
    }
}

fn draw_station_model(
    painter: &egui::Painter,
    observation: &Observation,
    position: Pos2,
    options: &ObservationPlotOptions,
    map_rotation: f32,
) {
    let station = station_color(observation);
    if options.show_wind_barbs
        && let (Some(direction), Some(speed)) =
            (observation.wind_dir_deg, observation.wind_speed_kt)
    {
        draw_wind_barb(painter, position, direction, speed, map_rotation, station);
    }
    if options.show_sky_cover {
        if let Some(cover) = observation.sky_cover {
            draw_sky_cover(painter, position, cover, station);
        } else {
            painter.circle_filled(position, 2.4, station);
        }
    } else {
        painter.circle_filled(position, 2.1, station);
    }

    let value_font = FontId::proportional(13.0);
    let small_font = FontId::proportional(10.5);
    if options.show_temperature
        && let Some(value) = observation.temp_c
    {
        painter.text(
            position + vec2(-7.0, -12.0),
            Align2::RIGHT_CENTER,
            format_station_temperature(value, options.fahrenheit),
            value_font.clone(),
            TEMPERATURE_COLOR,
        );
    }
    if options.show_dewpoint
        && let Some(value) = observation.dewpoint_c
    {
        painter.text(
            position + vec2(-7.0, 12.0),
            Align2::RIGHT_CENTER,
            format_station_temperature(value, options.fahrenheit),
            value_font.clone(),
            DEWPOINT_COLOR,
        );
    }
    if options.show_weather
        && let Some(weather) = &observation.present_weather
    {
        painter.text(
            position + vec2(-8.0, 0.0),
            Align2::RIGHT_CENTER,
            weather,
            small_font.clone(),
            WEATHER_COLOR,
        );
    } else if options.show_visibility
        && let Some(visibility) = observation.visibility_sm
    {
        painter.text(
            position + vec2(-8.0, 0.0),
            Align2::RIGHT_CENTER,
            format!("{visibility:.0}"),
            small_font.clone(),
            METAR_COLOR,
        );
    }
    if options.show_pressure
        && let Some(pressure) = observation.mslp_hpa
    {
        // Standard station-model pressure: final three tenths-of-hPa digits.
        let code = (pressure * 10.0).round().rem_euclid(1000.0) as u16;
        painter.text(
            position + vec2(8.0, -12.0),
            Align2::LEFT_CENTER,
            format!("{code:03}"),
            small_font.clone(),
            PRESSURE_COLOR,
        );
    }
    if options.show_gusts
        && let Some(gust) = observation.wind_gust_kt
    {
        painter.text(
            position + vec2(8.0, 12.0),
            Align2::LEFT_CENTER,
            format!("G{gust:.0}"),
            small_font.clone(),
            MESONET_COLOR,
        );
    }
    if options.show_station_id {
        painter.text(
            position + vec2(0.0, 25.0),
            Align2::CENTER_CENTER,
            &observation.station_id,
            small_font,
            station.gamma_multiply(0.88),
        );
    }
}

fn draw_sky_cover(painter: &egui::Painter, position: Pos2, cover: SkyCover, color: Color32) {
    let radius = 4.5;
    let outline = Stroke::new(1.1, color);
    painter.circle_filled(position, radius, PLOT_BACKGROUND);
    let fraction = match cover {
        SkyCover::Clear => 0.0,
        SkyCover::Few => 0.25,
        SkyCover::Scattered => 0.5,
        SkyCover::Broken => 0.875,
        SkyCover::Overcast | SkyCover::Obscured => 1.0,
    };
    if fraction >= 1.0 {
        painter.circle_filled(position, radius, color);
    } else if fraction > 0.0 {
        let segments = 24;
        let mut vertices = Vec::with_capacity(segments + 2);
        vertices.push(position);
        for index in 0..=segments {
            let angle = -std::f32::consts::FRAC_PI_2
                + std::f32::consts::TAU * fraction * index as f32 / segments as f32;
            vertices.push(position + vec2(angle.cos(), angle.sin()) * radius);
        }
        painter.add(egui::Shape::convex_polygon(vertices, color, Stroke::NONE));
    }
    painter.circle_stroke(position, radius, outline);
    if cover == SkyCover::Obscured {
        let slash = Stroke::new(1.3, PLOT_BACKGROUND);
        painter.line_segment(
            [position + vec2(-2.8, -2.8), position + vec2(2.8, 2.8)],
            slash,
        );
        painter.line_segment(
            [position + vec2(-2.8, 2.8), position + vec2(2.8, -2.8)],
            slash,
        );
    }
}

fn draw_wind_barb(
    painter: &egui::Painter,
    station: Pos2,
    direction_deg: f32,
    speed_kt: f32,
    rotation: f32,
    color: Color32,
) {
    let stroke = Stroke::new(1.2, color);
    if speed_kt < 2.5 {
        painter.circle_stroke(station, 6.0, stroke);
        return;
    }
    let angle = direction_deg.to_radians();
    let east = vec2(rotation.cos(), rotation.sin());
    let north = vec2(rotation.sin(), -rotation.cos());
    let upwind = east * angle.sin() + north * angle.cos();
    let crosswind = vec2(-upwind.y, upwind.x);
    let shaft = 23.0;
    let spacing = 4.0;
    painter.line_segment([station + upwind * 4.4, station + upwind * shaft], stroke);
    let mut remaining = ((speed_kt + 2.5) / 5.0).floor() * 5.0;
    let mut offset = shaft;
    while remaining >= 50.0 {
        let base = station + upwind * offset;
        painter.add(egui::Shape::convex_polygon(
            vec![
                base,
                base + crosswind * 8.0 - upwind * 2.5,
                base - upwind * 5.0,
            ],
            color,
            stroke,
        ));
        offset -= 8.5;
        remaining -= 50.0;
    }
    while remaining >= 10.0 {
        let base = station + upwind * offset;
        painter.line_segment([base, base + crosswind * 8.0 + upwind * 2.5], stroke);
        offset -= spacing;
        remaining -= 10.0;
    }
    if remaining >= 5.0 {
        if speed_kt < 10.0 {
            offset -= 4.0;
        }
        let base = station + upwind * offset;
        painter.line_segment([base, base + crosswind * 4.2 + upwind * 1.2], stroke);
    }
}

fn draw_history_trend(ui: &mut egui::Ui, history: &[Observation], fahrenheit: bool) {
    let values: Vec<_> = history
        .iter()
        .flat_map(|observation| [observation.temp_c, observation.dewpoint_c])
        .flatten()
        .filter(|value| value.is_finite())
        .collect();
    if values.is_empty() {
        return;
    }
    let min = values.iter().copied().fold(f32::INFINITY, f32::min) - 1.0;
    let max = values.iter().copied().fold(f32::NEG_INFINITY, f32::max) + 1.0;
    let first = history.first().map(|observation| observation.time_utc);
    let last = history.last().map(|observation| observation.time_utc);
    let (Some(first), Some(last)) = (first, last) else {
        return;
    };
    let seconds = (last - first).num_seconds().max(1) as f32;
    let (allocated, _) =
        ui.allocate_exact_size(Vec2::new(ui.available_width(), 128.0), egui::Sense::hover());
    let plot = Rect::from_min_max(
        allocated.min + vec2(38.0, 8.0),
        allocated.max - vec2(8.0, 20.0),
    );
    let painter = ui.painter_at(allocated);
    painter.rect_filled(plot, 3.0, Color32::from_rgba_unmultiplied(22, 27, 34, 220));
    for index in 0..=4 {
        let fraction = index as f32 / 4.0;
        let y = egui::lerp(plot.bottom()..=plot.top(), fraction);
        painter.line_segment(
            [pos2(plot.left(), y), pos2(plot.right(), y)],
            Stroke::new(0.7, Color32::from_gray(63)),
        );
        painter.text(
            pos2(plot.left() - 5.0, y),
            Align2::RIGHT_CENTER,
            format_station_temperature(egui::lerp(min..=max, fraction), fahrenheit),
            FontId::proportional(10.0),
            Color32::LIGHT_GRAY,
        );
    }
    for (selector, color) in [(false, TEMPERATURE_COLOR), (true, DEWPOINT_COLOR)] {
        let points = history
            .iter()
            .filter_map(|observation| {
                let value = if selector {
                    observation.dewpoint_c
                } else {
                    observation.temp_c
                }?;
                let x_fraction = (observation.time_utc - first).num_seconds() as f32 / seconds;
                let y_fraction = ((value - min) / (max - min)).clamp(0.0, 1.0);
                Some(pos2(
                    egui::lerp(plot.left()..=plot.right(), x_fraction),
                    egui::lerp(plot.bottom()..=plot.top(), y_fraction),
                ))
            })
            .collect::<Vec<_>>();
        for pair in points.windows(2) {
            painter.line_segment([pair[0], pair[1]], Stroke::new(1.6, color));
        }
        for point in points {
            painter.circle_filled(point, 2.0, color);
        }
    }
    painter.text(
        plot.left_bottom() + vec2(0.0, 14.0),
        Align2::LEFT_CENTER,
        first.format("%m-%d %H:%M UTC").to_string(),
        FontId::proportional(10.0),
        Color32::GRAY,
    );
    painter.text(
        plot.right_bottom() + vec2(0.0, 14.0),
        Align2::RIGHT_CENTER,
        last.format("%H:%M UTC").to_string(),
        FontId::proportional(10.0),
        Color32::GRAY,
    );
}

fn format_station_temperature(celsius: f32, fahrenheit: bool) -> String {
    let value = if fahrenheit {
        celsius * 1.8 + 32.0
    } else {
        celsius
    };
    format!("{value:.0}")
}

fn color_optional_temperature(
    ui: &mut egui::Ui,
    value: Option<f32>,
    fahrenheit: bool,
    color: Color32,
) {
    if let Some(value) = value {
        let unit = if fahrenheit { "°F" } else { "°C" };
        ui.colored_label(
            color,
            format!("{} {unit}", format_station_temperature(value, fahrenheit)),
        );
    } else {
        ui.label("—");
    }
}

fn optional_with_unit(value: Option<f32>, unit: &str, decimals: usize) -> String {
    value.map_or_else(
        || "—".to_owned(),
        |value| format!("{value:.decimals$} {unit}"),
    )
}

fn format_history_wind(observation: &Observation) -> String {
    match (observation.wind_dir_deg, observation.wind_speed_kt) {
        (Some(direction), Some(speed)) => format!("{direction:03.0}° / {speed:.0} kt"),
        (None, Some(speed)) => format!("VRB / {speed:.0} kt"),
        _ => "—".to_owned(),
    }
}

fn format_history_sky(observation: &Observation) -> String {
    match (observation.sky_cover, observation.ceiling_ft_agl) {
        (Some(cover), Some(ceiling)) => format!("{} {ceiling:.0} ft", cover.label()),
        (Some(cover), None) => cover.label().to_owned(),
        (None, Some(ceiling)) => format!("{ceiling:.0} ft"),
        (None, None) => "—".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_station_models_use_fahrenheit_and_professional_fields() {
        let options = ObservationPlotOptions::default();
        assert!(options.fahrenheit);
        assert!(options.show_temperature && options.show_dewpoint && options.show_wind_barbs);
        assert_eq!(format_station_temperature(25.0, true), "77");
        assert_eq!(format_station_temperature(25.0, false), "25");
    }

    #[test]
    fn visibility_prefilter_handles_dateline_and_does_not_hide_full_globe() {
        let observation = Observation {
            station_id: "TEST".to_owned(),
            time_utc: Utc::now(),
            lat: 10.0,
            lon: -179.5,
            temp_c: None,
            dewpoint_c: None,
            wind_dir_deg: None,
            wind_speed_kt: None,
            wind_gust_kt: None,
            altim_in_hg: None,
            mslp_hpa: None,
            precip_1h_in: None,
            visibility_sm: None,
            ceiling_ft_agl: None,
            sky_cover: None,
            present_weather: None,
            raw_metar: None,
            network: "METAR".to_owned(),
            elevation_m: None,
            completeness: 0,
        };
        assert!(likely_visible(&observation, Some((179.5, 10.0)), 300.0));
        assert!(!likely_visible(&observation, Some((20.0, 10.0)), 300.0));
        assert!(likely_visible(&observation, Some((20.0, 10.0)), 9000.0));
    }
}
