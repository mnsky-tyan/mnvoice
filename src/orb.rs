// Ethereal floating gas-fluid orb (48px) for mnvoice.
// Simulates a supercritical fluid / zero-gravity luminescent gas nebula inside a translucent glass sphere.
// Rendered via Win32 layered window (UpdateLayeredWindow) with 32-bit premultiplied ARGB.
// Sits unobtrusively right at the bottom edge of the screen, 2px above the taskbar.
// Click-through, non-activating, zero interference with active apps.

use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{w, PCWSTR};

use crate::windows_app::log;

const ORB_WIDTH: i32 = 48;
const ORB_HEIGHT: i32 = 48;
const ORB_CLASS_NAME: PCWSTR = w!("mnvoiceOrbClass");

const ORB_CX: f32 = ORB_WIDTH as f32 / 2.0;
const ORB_CY: f32 = ORB_HEIGHT as f32 / 2.0;
/// The sphere's radius as three quarters of the half-extent, so resizing the
/// window scales the sphere instead of leaving it floating at a stale pixel
/// radius (they used to be independent literals: `24.0`/`24.0`/`18.0`).
const ORB_RADIUS: f32 = ORB_CX * 0.75;

/// The fluid level the rendering is tuned around, and the denominator that
/// makes the filament brightness scale with the user's setting.
///
/// This is `config::DEFAULT_ORB_FLUID_LEVEL` spelled by reference rather than
/// re-typed: the render used to divide by a literal `0.75` while the config
/// module owned the value of that default, so changing `ORB_FLUID_LEVEL`'s
/// default would silently re-brighten every orb - a coupling no test caught,
/// because both sides were correct on their own. Naming it here makes the
/// dependency visible and one edit away from holding.
const REFERENCE_FLUID_LEVEL: f32 = crate::config::DEFAULT_ORB_FLUID_LEVEL;

/// The two ends of the filament brightness curve, declared together because
/// they describe one look (see their use). Module-level so a change to one is
/// visibly next to the other, rather than buried in a per-pixel loop body.
const FILAMENT_GAIN: f32 = 1.45;
const FILAMENT_FLOOR: f32 = 0.28;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum OrbState {
    Recording,
    Transcribing,
}

pub struct Orb {
    hwnd: HWND,
    dc_mem: HDC,
    bitmap: HBITMAP,
    bits: *mut u32,
    state: OrbState,
    frame: u32,
    visible: bool,
    color: (f32, f32, f32),
    fluid_level: f32,
    /// Cached bottom-center position; refreshed by `reposition`, read per
    /// frame so painting does not call SystemParametersInfoW 30+ times a second.
    pos: (i32, i32),
}

/// The orb's window class needs no message handling of its own - every
/// message goes straight to `DefWindowProcW` - but the registration demands a
/// procedure, so this is a one-call forwarder.
///
/// Bare `extern "system" fn` with an `unsafe` block at the call, the same shape
/// `windows_app.rs`'s `wndproc` uses. Both can be that shape because `WNDPROC`
/// is `Option<unsafe extern "system" fn(..)>` and a safe `fn` coerces into it -
/// the windows crate does not require `unsafe` on the declaration. The two
/// procedures used to be declared differently (`unsafe extern` here, bare
/// there), which told a reader nothing about which form the crate demands, so
/// they match now.
extern "system" fn orb_wndproc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

impl Orb {
    pub fn new(
        instance: HINSTANCE,
        color: (f32, f32, f32),
        fluid_level: f32,
    ) -> Result<Self, String> {
        unsafe {
            let wc = WNDCLASSW {
                lpfnWndProc: Some(orb_wndproc),
                hInstance: instance,
                lpszClassName: ORB_CLASS_NAME,
                ..Default::default()
            };
            let _ = RegisterClassW(&wc);

            let ex_style = WS_EX_LAYERED
                | WS_EX_TOPMOST
                | WS_EX_TOOLWINDOW
                | WS_EX_TRANSPARENT
                | WS_EX_NOACTIVATE;

            let (pos_x, pos_y) = calc_position();

            let hwnd = match CreateWindowExW(
                ex_style,
                ORB_CLASS_NAME,
                w!("mnvoiceOrb"),
                WS_POPUP,
                pos_x,
                pos_y,
                ORB_WIDTH,
                ORB_HEIGHT,
                None,
                None,
                instance,
                None,
            ) {
                Ok(h) => h,
                Err(e) => return Err(format!("CreateWindowExW for orb failed: {e}")),
            };

            let screen_dc = GetDC(HWND::default());
            let dc_mem = CreateCompatibleDC(screen_dc);
            let _ = ReleaseDC(HWND::default(), screen_dc);
            if dc_mem.0.is_null() {
                let _ = DestroyWindow(hwnd);
                return Err("CreateCompatibleDC failed".into());
            }

            // A BI_BITFIELDS header with an explicit alpha mask is required here.
            // Plain BI_RGB at 32bpp gives a DGB with no alpha, and UpdateLayeredWindow
            // then composites a fully transparent surface - the orb is invisible.
            let bmi = BITMAPV5HEADER {
                bV5Size: std::mem::size_of::<BITMAPV5HEADER>() as u32,
                bV5Width: ORB_WIDTH,
                bV5Height: -ORB_HEIGHT, // top-down DIB
                bV5Planes: 1,
                bV5BitCount: 32,
                bV5Compression: BI_BITFIELDS,
                bV5RedMask: 0x00FF_0000,
                bV5GreenMask: 0x0000_FF00,
                bV5BlueMask: 0x0000_00FF,
                bV5AlphaMask: 0xFF00_0000,
                ..Default::default()
            };

            let mut bits_ptr: *mut std::ffi::c_void = std::ptr::null_mut();
            let bitmap = match CreateDIBSection(
                dc_mem,
                &bmi as *const BITMAPV5HEADER as *const BITMAPINFO,
                DIB_RGB_COLORS,
                &mut bits_ptr,
                HANDLE::default(),
                0,
            ) {
                Ok(bmp) => bmp,
                Err(e) => {
                    let _ = DeleteDC(dc_mem);
                    let _ = DestroyWindow(hwnd);
                    return Err(format!("CreateDIBSection failed: {e}"));
                }
            };

            // Checked like every other Win32 call in this function: this is
            // what makes the DIB section the memory DC's surface, and a
            // refused selection would leave `dc_mem` on its default 1x1
            // bitmap - so the orb would composite a blank surface for the life
            // of the process while render_frame drew into a bitmap nothing
            // displays, with no diagnostic anywhere.
            //
            // `SelectObject` returns `HGDIOBJ`, not a `Result`: the windows
            // crate does not convert this one. Failure is `HGDI_ERROR`, whose
            // value is all ones - a valid handle is never that, because GDI
            // handles are small non-negative integers.
            let previous = SelectObject(dc_mem, bitmap);
            if previous.0 as isize == -1 {
                let _ = DeleteObject(bitmap);
                let _ = DeleteDC(dc_mem);
                let _ = DestroyWindow(hwnd);
                return Err("SelectObject refused the DIB section".into());
            }

            Ok(Self {
                hwnd,
                dc_mem,
                bitmap,
                bits: bits_ptr as *mut u32,
                state: OrbState::Recording,
                frame: 0,
                visible: false,
                color,
                fluid_level,
                pos: (pos_x, pos_y),
            })
        }
    }

    pub fn show(&mut self, state: OrbState) {
        self.state = state;
        self.frame = 0;
        self.reposition();
        self.render_frame();
        unsafe {
            let _ = ShowWindow(self.hwnd, SW_SHOWNA);
        }
        self.visible = true;
    }

    pub fn set_state(&mut self, state: OrbState) {
        self.state = state;
        if self.visible {
            self.render_frame();
        }
    }

    pub fn hide(&mut self) {
        if self.visible {
            unsafe {
                let _ = ShowWindow(self.hwnd, SW_HIDE);
            }
            self.visible = false;
        }
    }

    pub fn tick(&mut self) {
        if !self.visible {
            return;
        }
        self.frame = self.frame.wrapping_add(1);
        self.render_frame();
    }

    fn reposition(&mut self) {
        // Re-read the work area here (each session start), not per frame:
        // render_frame paints in place, so its position is whatever this set.
        self.pos = calc_position();
        let (pos_x, pos_y) = self.pos;
        unsafe {
            let _ = SetWindowPos(
                self.hwnd,
                HWND_TOPMOST,
                pos_x,
                pos_y,
                ORB_WIDTH,
                ORB_HEIGHT,
                SWP_NOACTIVATE | SWP_NOSIZE,
            );
        }
    }

    fn render_frame(&mut self) {
        if self.bits.is_null() {
            return;
        }
        let total_pixels = (ORB_WIDTH * ORB_HEIGHT) as usize;
        let buf = unsafe { std::slice::from_raw_parts_mut(self.bits, total_pixels) };

        // Both states render; which one differs only in the drift speed, and
        // `render_gas_fluid` decides that from the state itself.
        render_gas_fluid(self.frame, buf, self.state, self.color, self.fluid_level);

        unsafe {
            let (pos_x, pos_y) = self.pos;
            let pt_dst = POINT { x: pos_x, y: pos_y };
            let size = SIZE {
                cx: ORB_WIDTH,
                cy: ORB_HEIGHT,
            };
            let pt_src = POINT { x: 0, y: 0 };
            let blend = BLENDFUNCTION {
                BlendOp: AC_SRC_OVER as u8,
                BlendFlags: 0,
                SourceConstantAlpha: 255,
                AlphaFormat: AC_SRC_ALPHA as u8,
            };
            let _ = UpdateLayeredWindow(
                self.hwnd,
                HDC::default(),
                Some(&pt_dst),
                Some(&size),
                self.dc_mem,
                Some(&pt_src),
                COLORREF(0),
                Some(&blend),
                ULW_ALPHA,
            );
        }
    }
}

impl Drop for Orb {
    fn drop(&mut self) {
        self.hide();
        unsafe {
            if !self.bitmap.0.is_null() {
                let _ = DeleteObject(self.bitmap);
            }
            if !self.dc_mem.0.is_null() {
                let _ = DeleteDC(self.dc_mem);
            }
            if !self.hwnd.0.is_null() {
                let _ = DestroyWindow(self.hwnd);
            }
        }
    }
}

fn calc_position() -> (i32, i32) {
    let mut work_area = RECT::default();
    let read = unsafe {
        SystemParametersInfoW(
            SPI_GETWORKAREA,
            0,
            Some(&mut work_area as *mut _ as *mut _),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        )
    };
    // A refused geometry read leaves `work_area` all zeros, and the arithmetic
    // below would then place the orb at (-24, 0) - off the top-left corner and
    // invisible, which reads as "the indicator is broken" rather than "the
    // screen size is unknown". Fall back to the primary monitor's metrics and
    // say which source was used, so a failure is diagnosable instead of
    // silent.
    if read.is_err() {
        let (w, h) = unsafe {
            (
                GetSystemMetrics(SM_CXSCREEN),
                GetSystemMetrics(SM_CYSCREEN),
            )
        };
        if w > 0 && h > 0 {
            log(&format!(
                "orb: SPI_GETWORKAREA refused, using primary screen metrics {w}x{h}"
            ));
            return (x_for(0, w), y_for(0, h));
        }
        log("orb: no screen geometry available, using the top-left corner");
        return (0, 0);
    }
    (
        x_for(work_area.left, work_area.right),
        y_for(work_area.top, work_area.bottom),
    )
}

/// Centres the orb horizontally in `[left, right)`.
fn x_for(left: i32, right: i32) -> i32 {
    left + ((right - left) - ORB_WIDTH) / 2
}

/// Anchors the orb at the bottom of `[top, bottom)`, 2px above the taskbar.
fn y_for(top: i32, bottom: i32) -> i32 {
    (bottom - ORB_HEIGHT - 2).max(top)
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

/// Below this a light contributes nothing, in either of the two places the
/// renderer asks that question: `add_light`'s early-out on the requested
/// intensity, and `pack_premul`'s early-out on the accumulated alpha. The rule
/// is one, so the number is one constant - bumping it in only one of the two
/// sites would silently discard contributions after accumulation instead of
/// before it, with no compile signal either way.
const LIGHT_EPSILON: f32 = 0.002;

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

/// Render the 48px translucent glass sphere with its floating zero-gravity gas-fluid nebula inside.
fn render_gas_fluid(
    frame: u32,
    buf: &mut [u32],
    state: OrbState,
    color: (f32, f32, f32),
    fluid_level: f32,
) {
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
                let g1 = (rot1 * 2.0).sin() * (nx * 2.5 + t * 1.2).cos() + (ny * 2.5 - t * 1.5).sin();

                let rot2 = angle - t * 2.2 - (r_norm * 4.0);
                let g2 = (rot2 * 3.0).cos() * (nx * 3.2 - t * 2.0).sin() + (ny * 3.0 + t * 1.8).cos();

                // Wispy turbulence / domain warping
                let warp_x = nx + 0.30 * (ny * 4.0 + t * 2.5).sin();
                let warp_y = ny + 0.30 * (nx * 4.0 - t * 2.0).cos();
                let g3 = ((warp_x * 4.5 + t * 3.0).sin() + (warp_y * 4.5 - t * 2.8).cos()) * 0.5;

                // Balanced, airy fluid density with breathing room
                let density = ((0.46 + 0.26 * g1 + 0.20 * g2 + 0.16 * g3) * fluid_mult).clamp(0.0, 1.0);
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
                add_light(&mut px, (base_r + 0.2).min(1.0), (base_g + 0.2).min(1.0), (base_b + 0.2).min(1.0), hl2);
            }

            buf[row_offset + x as usize] = pack_premul(px);
        }
    }
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
            crate::config::DEFAULT_ORB_COLOR,
            crate::config::DEFAULT_ORB_FLUID_LEVEL,
        );

        // The shared helper, not a second spelling of its threshold: pass 14
        // introduced `body_pixels` as the one owner of "reads as the orb's
        // body", and this line kept its own copy of the same predicate, so
        // tuning the helper would have left this test on the old number.
        let opaque = body_pixels(&buf);
        assert!(opaque > 150, "orb must have visible opaque pixels, got {opaque}");

        let centre = (ORB_HEIGHT / 2 * ORB_WIDTH + ORB_WIDTH / 2) as usize;
        let p = buf[centre];
        assert!(
            ((p >> 24) & 0xFF) > 100,
            "centre pixel must be visible, alpha=0x{:02X}",
            (p >> 24) & 0xFF
        );
        // Packing order must stay BGRA for UpdateLayeredWindow: the low byte is
        // blue, and the green byte must come from the configured 0.18 component.
        let green = (p >> 8) & 0xFF;
        assert!((20..60).contains(&green), "green byte looks wrong: {green}");
    }

    /// Pixels a frame paints with enough alpha to read as the orb's body, as
    /// opposed to the halo. Measured at the default fluid level: 223 while
    /// recording, 122 while transcribing, and 18 at the floor level - so the
    /// count tracks the fluid, which is what these tests are about.
    fn body_pixels(buf: &[u32]) -> usize {
        buf.iter().filter(|&&p| ((p >> 24) & 0xFF) > 200).count()
    }

    fn frame(state: OrbState, level: f32) -> Vec<u32> {
        let mut buf = vec![0u32; (ORB_WIDTH * ORB_HEIGHT) as usize];
        render_gas_fluid(10, &mut buf, state, crate::config::DEFAULT_ORB_COLOR, level);
        buf
    }

    /// Both live states must paint a body, not just a halo.
    ///
    /// This used to count `p != 0`, which is a fixed 1664 at every state,
    /// fluid level and tick because the aura covers those pixels whatever the
    /// fluid does - a renderer that drew no fluid at all still passed. Counting
    /// strongly-visible pixels instead makes the assertion able to fail, which
    /// is the point of a test that claims to check visibility.
    #[test]
    fn a_recording_frame_renders_visible_pixels() {
        let body = body_pixels(&frame(OrbState::Recording, crate::config::DEFAULT_ORB_FLUID_LEVEL));
        assert!(body > 100, "recording orb should paint a fluid body, got {body} pixels");
    }

    #[test]
    fn a_transcribing_frame_renders_visible_pixels() {
        let body = body_pixels(&frame(OrbState::Transcribing, crate::config::DEFAULT_ORB_FLUID_LEVEL));
        assert!(body > 100, "transcribing orb should paint a fluid body, got {body} pixels");
    }

    /// The body grows with `ORB_FLUID_LEVEL`, which is the whole reason the
    /// setting exists. The previous tests could not have caught it if the level
    /// were ignored entirely, because the pixels they counted did not depend on
    /// it.
    #[test]
    fn more_fluid_paints_a_bigger_body() {
        let floor = crate::config::parse_fluid_level("5");
        let low = body_pixels(&frame(OrbState::Recording, floor));
        let high = body_pixels(&frame(OrbState::Recording, 1.0));
        assert!(
            high > low * 2,
            "a full orb must paint far more body than one at the floor ({low} -> {high})"
        );
    }
}
