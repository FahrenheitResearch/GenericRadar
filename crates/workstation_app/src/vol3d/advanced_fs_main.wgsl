// Second-generation fragment entry point. Replaces the base shader's fs_main;
// `advanced::compose_shader` truncates the prelude at its `@fragment` marker
// and appends the helpers and this file in their place.
@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    let cy = cos(u.yaw);
    let sy = sin(u.yaw);
    let cp = cos(u.pitch);
    let sp = sin(u.pitch);
    let center = vec3<f32>(0.0, 0.0, u.zspan * clamp(u.focus_height, 0.0, 1.0));
    var eye = center + u.dist * vec3<f32>(cy * cp, sy * cp, sp);
    var fwd = normalize(center - eye);
    if (u.camera_mode > 0.5) {
        eye = vec3<f32>(u.fly_x, u.fly_y, u.fly_z);
        fwd = normalize(vec3<f32>(-cy * cp, -sy * cp, -sp));
    }
    let right = normalize(cross(fwd, vec3<f32>(0.0, 0.0, 1.0)));
    let up = cross(right, fwd);
    let ndc = (in.uv * 2.0 - 1.0) * vec2<f32>(u.aspect, 1.0);
    let rd = normalize(fwd + u.fov_scale * (ndc.x * right + ndc.y * up));

    // > 0.5 is the fixed-step, no-hierarchy reference path. It shares every
    // other uniform with the accelerated path so an A/B capture isolates the
    // traversal and nothing else.
    let reference = ua.reference_path > 0.5;

    let clip_low_z = clamp(u.clip_low, 0.0, 0.99) * u.zspan;
    let clip_high_z = clamp(u.clip_high, u.clip_low + 0.01, 1.0) * u.zspan;
    let crop_min = vec3<f32>(
        mix(-1.0, 1.0, clamp(ua.crop_x_min, 0.0, 0.99)),
        mix(-1.0, 1.0, clamp(ua.crop_y_min, 0.0, 0.99)),
        clip_low_z
    );
    let crop_max = vec3<f32>(
        mix(-1.0, 1.0, clamp(ua.crop_x_max, ua.crop_x_min + 0.01, 1.0)),
        mix(-1.0, 1.0, clamp(ua.crop_y_max, ua.crop_y_min + 0.01, 1.0)),
        clip_high_z
    );
    let hit = box_intersect(eye, rd, crop_min, crop_max);

    var color = vec3<f32>(0.0);
    var accumulated = 0.0;

    if (hit.y > max(hit.x, 0.0)) {
        let t0 = max(hit.x, 0.0);
        let step_count = i32(clamp(u.steps, 32.0, 256.0));
        let base_dt = (hit.y - t0) / f32(step_count);

        // Orthogonal slices are analytic plane intersections rather than a
        // sampled volume. Sort the three hit distances and composite front to
        // back.
        if (ua.render_mode > 3.5 && ua.render_mode < 4.5) {
            var slice_t = array<f32, 3>(HUGE_T, HUGE_T, HUGE_T);
            if (abs(rd.x) > 0.000001) {
                slice_t[0] = (mix(-1.0, 1.0, clamp(ua.slice_x, 0.0, 1.0)) - eye.x) / rd.x;
            }
            if (abs(rd.y) > 0.000001) {
                slice_t[1] = (mix(-1.0, 1.0, clamp(ua.slice_y, 0.0, 1.0)) - eye.y) / rd.y;
            }
            if (abs(rd.z) > 0.000001) {
                slice_t[2] = (clamp(ua.slice_z, 0.0, 1.0) * u.zspan - eye.z) / rd.z;
            }
            if (slice_t[1] < slice_t[0]) { let temp = slice_t[0]; slice_t[0] = slice_t[1]; slice_t[1] = temp; }
            if (slice_t[2] < slice_t[1]) { let temp = slice_t[1]; slice_t[1] = slice_t[2]; slice_t[2] = temp; }
            if (slice_t[1] < slice_t[0]) { let temp = slice_t[0]; slice_t[0] = slice_t[1]; slice_t[1] = temp; }
            for (var slice_index = 0; slice_index < 3; slice_index = slice_index + 1) {
                let sample_t = slice_t[slice_index];
                if (sample_t < t0 || sample_t > hit.y) { continue; }
                let point = eye + rd * sample_t;
                if (!crop_contains(point, clip_low_z, clip_high_z)) { continue; }
                let uvw = point_to_uvw(point);
                let structure = textureSampleLevel(t_volume, s_volume, uvw, 0.0).r;
                let support = support_value(uvw);
                if (support <= 0.0001) { continue; }
                var lut_coord = structure;
                var transfer = threshold_strength(structure, u.threshold, u.threshold_high, u.threshold_mode, 0.08);
                if (u.velocity_mode > 0.5) {
                    transfer = smoothstep(u.ref_gate, u.ref_gate + 0.08, structure);
                    lut_coord = textureSampleLevel(t_color, s_volume, uvw, 0.0).r;
                }
                if (transfer <= 0.0) { continue; }
                let palette = textureSampleLevel(t_lut, s_lut, vec2<f32>(lut_coord, 0.5), 0.0);
                let weight = support_weight(support);
                // A slice is a plane, not a slab, so there is no path length to
                // integrate - but the ramp still has to reach it through the
                // OPTICAL DEPTH and not through a multiplication on the
                // composited alpha. Multiplying was the first shape of this and
                // it is unsound: the gain is deliberately above 1, so
                // `palette.a * opacity * 0.72 * k` passes 1 at a strong core,
                // `1 - accumulated` goes NEGATIVE, and the next of the three
                // planes then SUBTRACTS its colour and opacity. Measured on
                // KUDX 2026-08-19T04:37Z: with a uniform ramp of 20 - strictly
                // more absorption than a uniform 1 at every voxel - 0.37% of the
                // frame came back LESS opaque than the flat render, by up to
                // 0.91 opacity points, which front-to-back compositing cannot
                // do. This form is bounded in 0..1 by construction, is the
                // identity at k = 1, and is k * a to first order in a, so the
                // slice and the march show one transfer function.
                //
                // `structure` is the STRUCTURE sample, so in velocity two-box
                // mode the ramp is still reading reflectivity while `lut_coord`
                // carries the signed velocity.
                let plane_alpha = palette.a * u.opacity * transfer * max(weight, 0.15) * 0.72;
                let alpha = 1.0 - pow(
                    max(1.0 - plane_alpha, 0.0001),
                    max(opacity_ramp(structure), 0.0)
                );
                var rgb = shaded_rgb(uvw, rd, palette.rgb);
                if (ua.support_mode > 1.5) { rgb = support_color(support); }
                color = color + (1.0 - accumulated) * alpha * rgb;
                accumulated = accumulated + (1.0 - accumulated) * alpha;
            }
        } else {
            var jitter = 0.0;
            if (!reference) {
                jitter = (hash12(in.pos.xy) - 0.5) * clamp(ua.jitter_strength, 0.0, 1.0);
            }
            var t = t0 + max(jitter * base_dt, 0.0);
            var previous_t = t;
            var previous_structure = textureSampleLevel(
                t_volume, s_volume, point_to_uvw(eye + rd * t), 0.0
            ).r;
            var have_previous = false;
            var maximum_value = -1.0;
            var maximum_lut = 0.0;
            var maximum_support = 0.0;
            var maximum_uvw = vec3<f32>(0.5);

            for (var iteration = 0; iteration < MAX_TRAVERSAL_STEPS; iteration = iteration + 1) {
                if (t > hit.y || accumulated > 0.992) { break; }
                let point = eye + rd * t;
                let uvw = point_to_uvw(point);
                let fine = fine_range(uvw);

                if (!reference) {
                    // Two-level empty-space skipping. Both bounds are
                    // conservative over the cell plus a one-voxel apron, so a
                    // cell rejected here cannot paint this pixel.
                    if (!range_can_contribute(coarse_range(uvw))) {
                        t = t + next_cell_exit(point, rd, COARSE_DIMS) + 0.00002;
                        have_previous = false;
                        continue;
                    }
                    if (!range_can_contribute(fine)) {
                        t = t + next_cell_exit(point, rd, FINE_DIMS) + 0.00002;
                        have_previous = false;
                        continue;
                    }
                }

                let structure = textureSampleLevel(t_volume, s_volume, uvw, 0.0).r;
                let support = support_value(uvw);
                // No data is transparent in EVERY mode. Without this the
                // stored 0 of an unobserved voxel reads as a legitimate low
                // value and Below/Outside thresholds paint the empty box.
                if (support <= 0.0001) {
                    t = t + base_dt;
                    have_previous = false;
                    continue;
                }

                var transfer = 0.0;
                var lut_coord = structure;
                var emphasis = 1.0;
                if (u.velocity_mode > 0.5) {
                    // Two-box: reflectivity gives the body, signed velocity
                    // gives the colour. Neither borrows the other's role.
                    transfer = smoothstep(u.ref_gate, u.ref_gate + 0.08, structure);
                    let velocity = textureSampleLevel(t_color, s_volume, uvw, 0.0).r;
                    let magnitude = clamp(abs(velocity - 0.5) * 2.0, 0.0, 1.0);
                    emphasis = mix(1.0, magnitude, clamp(u.couplet_emphasis, 0.0, 1.0));
                    lut_coord = velocity;
                } else {
                    transfer = threshold_strength(
                        structure, u.threshold, u.threshold_high, u.threshold_mode, 0.08
                    );
                }

                // Maximum projection keeps the strongest contributing structure
                // while preserving velocity colour at that same voxel.
                if (ua.render_mode > 2.5 && ua.render_mode < 3.5) {
                    let candidate = transfer * emphasis;
                    if (candidate > 0.0 && structure > maximum_value) {
                        maximum_value = structure;
                        maximum_lut = lut_coord;
                        maximum_support = support;
                        maximum_uvw = uvw;
                    }
                } else {
                    let crossed_iso = have_previous
                        && ((previous_structure < ua.iso_value && structure >= ua.iso_value)
                            || (previous_structure > ua.iso_value && structure <= ua.iso_value));
                    if (crossed_iso && (ua.render_mode > 0.5 && ua.render_mode < 2.5)) {
                        let surface_t = refined_iso_t(eye, rd, previous_t, t);
                        let surface_uvw = point_to_uvw(eye + rd * surface_t);
                        let surface_support = support_value(surface_uvw);
                        if (surface_support > 0.0001) {
                            var surface_lut = textureSampleLevel(t_volume, s_volume, surface_uvw, 0.0).r;
                            if (u.velocity_mode > 0.5) {
                                surface_lut = textureSampleLevel(t_color, s_volume, surface_uvw, 0.0).r;
                            }
                            let palette = textureSampleLevel(t_lut, s_lut, vec2<f32>(surface_lut, 0.5), 0.0);
                            let support_scale = support_weight(surface_support);
                            let shell_alpha = mix(0.78, 0.38, select(0.0, 1.0, ua.render_mode < 1.5))
                                * max(support_scale, 0.1);
                            var shell_rgb = shaded_rgb(surface_uvw, rd, palette.rgb);
                            if (ua.support_mode > 1.5 || ua.render_mode > 4.5) {
                                shell_rgb = support_color(surface_support);
                            }
                            color = color + (1.0 - accumulated) * shell_alpha * shell_rgb;
                            accumulated = accumulated + (1.0 - accumulated) * shell_alpha;
                            if (ua.render_mode > 1.5 && ua.render_mode < 2.5) {
                                break;
                            }
                        }
                    }

                    if (transfer > 0.0 && (ua.render_mode < 1.5 || ua.render_mode > 4.5)) {
                        let palette = textureSampleLevel(t_lut, s_lut, vec2<f32>(lut_coord, 0.5), 0.0);
                        let support_scale = support_weight(support);
                        var sample_rgb = palette.rgb;
                        var alpha = 0.0;
                        // Everything that scales how much this sample absorbs
                        // scales its OPTICAL DEPTH: the length of ray it stands
                        // for, the density multiplier, the value-driven
                        // extinction ramp, and the support display weight.
                        // Only that form composites correctly, because tau adds
                        // along a ray while alpha does not (Max 1995, eq. 1-4).
                        // The support weight used to be multiplied onto the
                        // finished alpha instead, which is why the same volume
                        // changed brightness whenever the adaptive sampler
                        // changed rate.
                        //
                        // `segment_dt` is the ACTUAL distance back to the
                        // previous sample, not `base_dt`. `adaptive_strength`
                        // lets the step reach 2.25x base, and it reaches it in
                        // flat interiors - which is precisely the inside of a
                        // core - so charging every sample one base step
                        // under-attenuated the storm exactly where it was
                        // meant to be solid. Where there is no previous sample
                        // (first hit, or the first sample after an empty-space
                        // skip) one base step is the honest estimate.
                        let segment_dt = select(base_dt, max(t - previous_t, 0.0), have_previous);
                        let optical_scale =
                            segment_dt * 28.0 * max(u.density, 0.05) * support_scale;
                        // Preintegration is disabled for the velocity two-box
                        // path: the table integrates one field, and there the
                        // structure and the colour are different fields.
                        let use_preintegration = ua.preintegration > 0.5
                            && u.velocity_mode < 0.5
                            && ua.render_mode < 1.5
                            && have_previous;
                        if (use_preintegration) {
                            // The table is row = segment START, column = segment
                            // END, and the ray runs previous -> current, so the
                            // horizontal axis takes the CURRENT value. Reversing
                            // the pair leaves the opacity untouched but weights
                            // the front-to-back colour toward the wrong end of
                            // the segment.
                            let segment = textureSampleLevel(
                                t_preintegrated,
                                s_lut,
                                vec2<f32>(structure, previous_structure),
                                0.0
                            );
                            sample_rgb = segment.rgb;
                            // Midpoint rule for the ramp across the segment.
                            // The table has already integrated the transfer
                            // function from `previous_structure` to
                            // `structure`; the ramp is the one factor left
                            // outside it, and the value halfway along a
                            // linearly interpolated segment is where a
                            // second-order quadrature of k(v) is evaluated.
                            let ramp = opacity_ramp(0.5 * (previous_structure + structure));
                            alpha = 1.0 - pow(
                                max(1.0 - segment.a, 0.0001),
                                max(optical_scale * ramp, 0.0)
                            );
                        } else {
                            let raw_alpha = palette.a * u.opacity * transfer * emphasis;
                            // One-sided, to match: `palette`, `transfer` and
                            // `emphasis` are all evaluated at this sample, so
                            // the ramp is too. `structure` is the STRUCTURE
                            // plane, which is reflectivity in velocity two-box
                            // mode - there the body comes from reflectivity and
                            // only the colour comes from m/s, so a fast, empty
                            // gate must not be allowed to turn solid.
                            let ramp = opacity_ramp(structure);
                            alpha = 1.0 - pow(
                                max(1.0 - raw_alpha, 0.0001),
                                max(optical_scale * ramp, 0.0)
                            );
                        }
                        sample_rgb = shaded_rgb(uvw, rd, sample_rgb);
                        if (ua.support_mode > 1.5 || ua.render_mode > 4.5) {
                            sample_rgb = support_color(support);
                        }
                        color = color + (1.0 - accumulated) * alpha * sample_rgb;
                        accumulated = accumulated + (1.0 - accumulated) * alpha;
                    }
                }

                previous_t = t;
                previous_structure = structure;
                have_previous = true;

                // Spend steps where the field is changing or where it is close
                // to the value the transfer function cares about, and coast
                // through flat interiors.
                var factor = 1.0;
                if (!reference) {
                    let interval_width = fine.g - fine.r;
                    let edge_value = select(u.threshold, ua.iso_value, ua.render_mode > 0.5 && ua.render_mode < 2.5);
                    let proximity = 1.0 - clamp(abs(structure - edge_value) / 0.12, 0.0, 1.0);
                    let detail = clamp(interval_width * 4.5, 0.0, 1.0);
                    let adaptive = max(detail, proximity);
                    let target_factor = mix(2.25, 0.55, adaptive);
                    factor = mix(1.0, target_factor, clamp(ua.adaptive_strength, 0.0, 1.0));
                }
                var step_dt = base_dt * factor;
                if (ua.render_mode > 2.5 && ua.render_mode < 3.5) {
                    step_dt = min(step_dt, base_dt * 0.85);
                }
                if (!reference) {
                    step_dt = min(step_dt, next_cell_exit(point, rd, FINE_DIMS) + 0.00002);
                }
                t = t + max(step_dt, base_dt * 0.35);
            }

            if (ua.render_mode > 2.5 && ua.render_mode < 3.5 && maximum_value >= 0.0) {
                let palette = textureSampleLevel(t_lut, s_lut, vec2<f32>(maximum_lut, 0.5), 0.0);
                let support_scale = support_weight(maximum_support);
                let alpha = clamp(u.opacity * max(support_scale, 0.1), 0.08, 1.0);
                var rgb = shaded_rgb(maximum_uvw, rd, palette.rgb);
                if (ua.support_mode > 1.5) { rgb = support_color(maximum_support); }
                color = rgb * alpha;
                accumulated = alpha;
            }
        }
    }

    // Ground underlay at z=0. The volume remains front-to-back composited, so
    // this intersection lies behind every volume sample.
    if (u.floor_mode > 0.5 && abs(rd.z) > 0.00001) {
        let floor_t = -eye.z / rd.z;
        if (floor_t > 0.0) {
            let point = eye + rd * floor_t;
            if (abs(point.x) <= 1.0 && abs(point.y) <= 1.0) {
                let floor_uv = vec2<f32>((point.x + 1.0) * 0.5, (point.y + 1.0) * 0.5);
                var value = textureSampleLevel(t_floor, s_floor, floor_uv, 0.0).r;
                if (u.floor_mode > 1.5 && u.velocity_mode < 0.5) {
                    value = column_max(floor_uv);
                }
                let floor_transfer = threshold_strength(
                    value, u.floor_threshold, u.floor_threshold_high,
                    u.floor_threshold_mode, 0.04
                );
                if (floor_transfer > 0.0) {
                    let palette = textureSampleLevel(t_lut, s_lut, vec2<f32>(value, 0.5), 0.0);
                    let alpha = palette.a * u.floor_opacity * floor_transfer;
                    color = color + (1.0 - accumulated) * alpha * palette.rgb;
                    accumulated = accumulated + (1.0 - accumulated) * alpha;
                }
            }
        }
    }

    if (accumulated <= 0.00001) {
        return vec4<f32>(0.0);
    }
    // The render target uses straight-alpha blending; unpremultiply once here
    // rather than letting the fixed-function blend multiply alpha twice.
    return vec4<f32>(color / accumulated, accumulated);
}
