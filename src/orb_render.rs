// The platform-neutral orb renderer.
//
// The orb is one look on every platform: a 48px translucent glass sphere with a
// zero-gravity gas-fluid nebula inside, packed as 32-bit premultiplied ARGB.
// What differs per platform is only where the pixels go - a Win32 layered
// window on Windows (`orb.rs`), a Wayland layer-shell surface on Linux
// (`platform/linux_orb.rs`). The simulation itself is arithmetic over one
// buffer, so it lives here once and every platform blits the same output.
//
// The buffer layout is premultiplied ARGB packed `(a << 24) | (r << 16) |
// (g << 8) | b`, which is both what Wayland's `ARGB8888` shm format and Win32's
// `UpdateLayeredWindow` consume (Windows reads it as BGRA in memory, the low
// byte first, and this packing puts blue in the low byte).

use crate::config;

/// The orb's square edge in pixels, shared by every platform's window.
pub const ORB_WIDTH: u32 = 48;
pub const ORB_HEIGHT: u32 = 48;

pub const ORB_CX: f32 = ORB_WIDTH as f32 / 2.0;
pub const ORB_CY: f32 = ORB_HEIGHT as f32 / 2.0;
/// The sphere's radius as three quarters of the half-extent, so resizing the
/// window scales the sphere instead of leaving it floating at a stale pixel
/// radius (they used to be independent literals: `24.0`/`24.0`/`18.0`).
pub const ORB_RADIUS: f32 = ORB_CX * 0.75;

/// The fluid level the rendering is tuned around, and the denominator that
/// makes the filament brightness scale with the user's setting.
///
/// This is `config::DEFAULT_ORB_FLUID_LEVEL` spelled by reference rather than
/// re-typed: the render used to divide by a literal `0.75` while the config
/// module owned the value of that default, so changing `ORB_FLUID_LEVEL`'s
/// default would silently re-brighten every orb - a coupling no test caught,
/// because both sides were correct on their own. Naming it here makes the
/// dependency visible and one edit away from holding.
pub const REFERENCE_FLUID_LEVEL: f32 = config::DEFAULT_ORB_FLUID_LEVEL;

/// The two ends of the filament brightness curve, declared together because
/// they describe one look (see their use). Module-level so a change to one is
/// visibly next to the other, rather than buried in a per-pixel loop body.
const FILAMENT_GAIN: f32 = 1.45;
const FILAMENT_FLOOR: f32 = 0.28;

/// Below this a light contributes nothing, in either of the two places the
/// renderer asks that question: `add_light`'s early-out on the requested
/// intensity, and `pack_premul`'s early-out on the accumulated alpha. The rule
/// is one, so the number is one constant - bumping it in only one of the two
/// sites would silently discard contributions after accumulation instead of
/// before it, with no compile signal either way.
const LIGHT_EPSILON: f32 = 0.002;

/// Which visual state the orb is showing. Recording breathes slowly; the
/// post-stop "thinking" state breathes faster, which is the only visual
/// difference between the two. An enum rather than a flattened bool so neither
/// end of the call has to remember which way `true` pointed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum OrbState {
    Recording,
    Transcribing,
}

/// Linear-light accumulator for one pixel: lights are summed here and packed
/// once at the end, so `add_light` adds to a struct instead of four separate
/// `&mut f32` slots.
#[derive(Clone, Copy, Default)]
struct Rgba {
    r: f32,
    g: f32,
    b: f32,
    a: f32,
}

#[inline]
fn add_light(acc: &mut Rgba, r: f32, g: f32, b: f32, intensity: f32) {
    if intensity <= LIGHT_EPSILON {
        return;
    }
    let int_c = intensity.clamp(0.0, 1.0);
    acc.r += r * int_c;
    acc.g += g * int_c;
    acc.b += b * int_c;
    acc.a += int_c;
}

#[inline]
fn pack_premul(c: Rgba) -> u32 {
    let a_c = c.a.clamp(0.0, 1.0);
    if a_c <= LIGHT_EPSILON {
        return 0;
    }
    let a_byte = (a_c * 255.0 + 0.5) as u32;
    let r_byte = (c.r.clamp(0.0, 1.0) * a_c * 255.0 + 0.5) as u32;
    let g_byte = (c.g.clamp(0.0, 1.0) * a_c * 255.0 + 0.5) as u32;
    let b_byte = (c.b.clamp(0.0, 1.0) * a_c * 255.0 + 0.5) as u32;
    (a_byte << 24) | (r_byte << 16) | (g_byte << 8) | b_byte
}

/// Which way the nebula drifts. Recording breathes slowly; the post-stop
/// "thinking" state breathes faster, which is the only visual difference
/// between the two states. Passed as the state rather than a flattened bool so
/// neither end of the call has to remember which way `true` pointed.
fn fluid_speed(state: OrbState) -> f32 {
    match state {
        OrbState::Recording => 0.038,
        OrbState::Transcribing => 0.065,
    }
}

/// Render the 48px translucent glass sphere with its floating zero-gravity
/// gas-fluid nebula into `buf`, one premultiplied ARGB pixel per entry.
///
/// `buf` must be exactly `ORB_WIDTH * ORB_HEIGHT` long - the render indexes it
/// as a square, and a shorter slice would panic mid-frame rather than clip.
pub fn render_gas_fluid(
    frame: u32,
    buf: &mut [u32],
    state: OrbState,
    color: (f32, f32, f32),
    fluid_level: f32,
) {
    assert!(
        buf.len() == (ORB_WIDTH * ORB_HEIGHT) as usize,
        "orb buffer must be exactly ORB_WIDTH * ORB_HEIGHT pixels"
    );
    let (base_r, base_g, base_b) = color;
    let fluid_mult = fluid_level.clamp(0.05, 1.0);
    let speed = fluid_speed(state);
    let t = frame as f32 * speed;
    let cx = ORB_CX;
    let cy = ORB_CY;
    let r_sphere = ORB_RADIUS;

    buf.fill(0);

    for y in 0..ORB_HEIGHT {
        let dy = y as f32 - cy + 0.5;
        let row_offset = (y * ORB_WIDTH) as usize;
        for x in 0..ORB_WIDTH {
            let dx = x as f32 - cx + 0.5;
            let d_sq = dx * dx + dy * dy;

            if d_sq > (r_sphere + 5.0) * (r_sphere + 5.0) {
                continue;
            }

            let d = d_sq.sqrt();
            let mut px = Rgba::default();

            // 1. Ambient soft aura
            let aura = (-d_sq / 260.0).exp() * 0.26 * fluid_mult.max(0.4);
            add_light(&mut px, base_r, base_g, base_b, aura);

            if d <= r_sphere + 1.2 {
                let sphere_edge = ((r_sphere + 1.2 - d) / 1.5).clamp(0.0, 1.0);
                let z = (0.0f32).max(1.0 - (d / r_sphere).powi(2)).sqrt();
                let fresnel = (1.0 - z).powi(2);

                // --- ZERO-GRAVITY GAS-FLUID NEBULA FLOATING ALL AROUND THE SPHERE ---
                let nx = dx / r_sphere;
                let ny = dy / r_sphere;
                let r_norm = d / r_sphere;
                let angle = ny.atan2(nx);

                // Swirling convective gas vortices
                let rot1 = angle + t * 1.8 + (r_norm * 3.5);
                let g1 =
                    (rot1 * 2.0).sin() * (nx * 2.5 + t * 1.2).cos() + (ny * 2.5 - t * 1.5).sin();

                let rot2 = angle - t * 2.2 - (r_norm * 4.0);
                let g2 =
                    (rot2 * 3.0).cos() * (nx * 3.2 - t * 2.0).sin() + (ny * 3.0 + t * 1.8).cos();

                // Wispy turbulence / domain warping
                let warp_x = nx + 0.30 * (ny * 4.0 + t * 2.5).sin();
                let warp_y = ny + 0.30 * (nx * 4.0 - t * 2.0).cos();
                let g3 = ((warp_x * 4.5 + t * 3.0).sin() + (warp_y * 4.5 - t * 2.8).cos()) * 0.5;

                // Balanced, airy fluid density with breathing room
                let density =
                    ((0.46 + 0.26 * g1 + 0.20 * g2 + 0.16 * g3) * fluid_mult).clamp(0.0, 1.0);
                let core_falloff = (1.0 - (r_norm * 0.78).powi(2)).clamp(0.0, 1.0);
                let gas_volume = density * core_falloff;

                // Subtle ambient vapor fill
                let ambient_fluid = 0.12 * sphere_edge * fluid_mult;
                add_light(&mut px, base_r, base_g, base_b, ambient_fluid);

                // Real vibrant fluid body
                let body_int = gas_volume * 0.88 * sphere_edge;
                let cr = base_r;
                let cg = base_g * (0.6 + 0.4 * density);
                let cb = base_b * (0.6 + 0.4 * density);
                add_light(&mut px, cr, cg, cb, body_int);

                // Luminous filaments & tendrils
                // GAIN puts a default-level orb near the top of the range; FLOOR
                // is the level below which no filaments show. They are one look,
                // not two numbers: change the gain without the floor and
                // filaments appear at levels the user would call empty.
                let filament = (gas_volume * FILAMENT_GAIN * (fluid_mult / REFERENCE_FLUID_LEVEL)
                    - FILAMENT_FLOOR)
                    .clamp(0.0, 1.0);
                let fil_r = (base_r + 0.3).min(1.0);
                let fil_g = (base_g + 0.3).min(1.0);
                let fil_b = (base_b + 0.3).min(1.0);
                add_light(&mut px, fil_r, fil_g, fil_b, filament * 0.72 * sphere_edge);

                // Floating ion sparks drifting in zero-g gas
                let sp1_x = (t * 1.3).sin() * 6.5;
                let sp1_y = (t * 1.7).cos() * 6.5;
                let sp1 = (-((dx - sp1_x).powi(2) + (dy - sp1_y).powi(2)) / 3.8).exp() * 0.85;
                let sp_r = (base_r + 0.4).min(1.0);
                let sp_g = (base_g + 0.4).min(1.0);
                let sp_b = (base_b + 0.4).min(1.0);
                add_light(&mut px, sp_r, sp_g, sp_b, sp1 * sphere_edge);

                let sp2_x = (t * 2.1 + 2.0).cos() * 8.0;
                let sp2_y = (t * 1.5 + 1.0).sin() * 8.0;
                let sp2 = (-((dx - sp2_x).powi(2) + (dy - sp2_y).powi(2)) / 3.2).exp() * 0.75;
                add_light(&mut px, sp_r, sp_g, sp_b, sp2 * sphere_edge);

                // --- TRANSLUCENT GLASS SHELL ---
                // Crystal Fresnel rim tinted with base color
                let rim = fresnel * (0.46 + 0.20 * (t * 2.5).sin()) * sphere_edge;
                add_light(
                    &mut px,
                    (base_r + 0.2).min(1.0),
                    (base_g + 0.2).min(1.0),
                    (base_b + 0.2).min(1.0),
                    rim,
                );

                // Primary specular highlight (crisp white reflection on outer glass)
                let hl_dx = dx + 5.2;
                let hl_dy = dy + 5.8;
                let hl_d_sq = hl_dx * hl_dx + hl_dy * hl_dy;
                let hl = (-hl_d_sq / 8.5).exp() * 0.95 * sphere_edge;
                add_light(&mut px, 1.0, 0.95, 0.98, hl);

                // Secondary bounce highlight
                let hl2_dx = dx - 4.8;
                let hl2_dy = dy - 5.2;
                let hl2_d_sq = hl2_dx * hl2_dx + hl2_dy * hl2_dy;
                let hl2 = (-hl2_d_sq / 13.0).exp() * 0.36 * sphere_edge;
                add_light(
                    &mut px,
                    (base_r + 0.2).min(1.0),
                    (base_g + 0.2).min(1.0),
                    (base_b + 0.2).min(1.0),
                    hl2,
                );
            }

            buf[row_offset + x as usize] = pack_premul(px);
        }
    }
}

/// Pixels a frame paints with enough alpha to read as the orb's body, as
/// opposed to its faint aura. The one owner of "reads as the orb's body": both
/// the platform tests and the look tests below count through this, so tuning
/// the threshold moves every test together instead of leaving one behind.
#[cfg_attr(not(test), allow(dead_code))]
pub fn body_pixels(buf: &[u32]) -> usize {
    buf.iter().filter(|p| ((**p >> 24) & 0xFF) > 40).count()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An all-transparent buffer arithmetically "passes" a non-zero check, so every
    /// test must assert opaque pixels too. A regressed alpha channel is exactly how
    /// an orb ships that is present but invisible.
    #[test]
    fn a_rendered_frame_is_opaque_and_coloured() {
        let mut buf = vec![0u32; (ORB_WIDTH * ORB_HEIGHT) as usize];
        render_gas_fluid(
            10,
            &mut buf,
            OrbState::Recording,
            config::DEFAULT_ORB_COLOR,
            config::DEFAULT_ORB_FLUID_LEVEL,
        );

        let opaque = body_pixels(&buf);
        assert!(
            opaque > 150,
            "orb must have visible opaque pixels, got {opaque}"
        );

        let centre = (ORB_HEIGHT / 2 * ORB_WIDTH + ORB_WIDTH / 2) as usize;
        let p = buf[centre];
        assert!(
            ((p >> 24) & 0xFF) > 100,
            "centre pixel must be visible, alpha=0x{:02X}",
            (p >> 24) & 0xFF
        );
        // Packing order: the low byte is blue, and the green byte must come
        // from the configured 0.18 component. On Wayland the same word is
        // ARGB8888, so the byte order is the one contract both platforms read.
        let green = (p >> 8) & 0xFF;
        assert!((20..60).contains(&green), "green byte looks wrong: {green}");
    }

    #[test]
    fn a_recording_frame_renders_visible_pixels() {
        let mut buf = vec![0u32; (ORB_WIDTH * ORB_HEIGHT) as usize];
        render_gas_fluid(
            0,
            &mut buf,
            OrbState::Recording,
            config::DEFAULT_ORB_COLOR,
            1.0,
        );
        assert!(body_pixels(&buf) > 100, "recording orb must be visible");
    }

    #[test]
    fn a_transcribing_frame_renders_visible_pixels() {
        let mut buf = vec![0u32; (ORB_WIDTH * ORB_HEIGHT) as usize];
        render_gas_fluid(
            0,
            &mut buf,
            OrbState::Transcribing,
            config::DEFAULT_ORB_COLOR,
            1.0,
        );
        assert!(body_pixels(&buf) > 100, "transcribing orb must be visible");
    }

    #[test]
    fn more_fluid_paints_a_bigger_body() {
        let mut low = vec![0u32; (ORB_WIDTH * ORB_HEIGHT) as usize];
        render_gas_fluid(
            5,
            &mut low,
            OrbState::Recording,
            config::DEFAULT_ORB_COLOR,
            0.05,
        );
        let mut full = vec![0u32; (ORB_WIDTH * ORB_HEIGHT) as usize];
        render_gas_fluid(
            5,
            &mut full,
            OrbState::Recording,
            config::DEFAULT_ORB_COLOR,
            1.0,
        );
        assert!(
            body_pixels(&full) > body_pixels(&low),
            "a fuller orb must paint a bigger body: {} vs {}",
            body_pixels(&full),
            body_pixels(&low)
        );
    }

    #[test]
    fn a_short_buffer_is_rejected_instead_of_panicking_mid_frame() {
        let mut buf = vec![0u32; 10];
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            render_gas_fluid(
                0,
                &mut buf,
                OrbState::Recording,
                config::DEFAULT_ORB_COLOR,
                1.0,
            );
        }));
        assert!(result.is_err(), "a wrongly sized buffer must fail loudly");
    }
}
