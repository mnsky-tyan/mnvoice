// Ethereal floating gas-fluid orb (36px) for mnvoice.
// Simulates a supercritical fluid / zero-gravity luminescent gas nebula inside a translucent glass sphere.
// Rendered via Win32 layered window (UpdateLayeredWindow) with 32-bit premultiplied ARGB.
// Sits unobtrusively right at the bottom edge of the screen, 2px above the taskbar.
// Click-through, non-activating, zero interference with active apps.

use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{w, PCWSTR};

pub const ORB_WIDTH: i32 = 48;
pub const ORB_HEIGHT: i32 = 48;
const ORB_CLASS_NAME: PCWSTR = w!("mnvoiceOrbClass");

const ORB_CX: f32 = 24.0;
const ORB_CY: f32 = 24.0;
const ORB_RADIUS: f32 = 18.0;

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
    pub state: OrbState,
    frame: u32,
    visible: bool,
}

unsafe extern "system" fn orb_wndproc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

impl Orb {
    pub fn new(instance: HINSTANCE) -> Result<Self, String> {
        unsafe {
            let wc = WNDCLASSW {
                lpfnWndProc: Some(orb_wndproc),
                hInstance: instance.into(),
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

            let bmi = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: ORB_WIDTH,
                    biHeight: -ORB_HEIGHT, // top-down DIB
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB.0,
                    ..Default::default()
                },
                ..Default::default()
            };

            let mut bits_ptr: *mut std::ffi::c_void = std::ptr::null_mut();
            let bitmap = match CreateDIBSection(
                dc_mem,
                &bmi,
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

            let _ = SelectObject(dc_mem, bitmap);

            Ok(Self {
                hwnd,
                dc_mem,
                bitmap,
                bits: bits_ptr as *mut u32,
                state: OrbState::Recording,
                frame: 0,
                visible: false,
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

    fn reposition(&self) {
        let (pos_x, pos_y) = calc_position();
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

    fn render_frame(&self) {
        if self.bits.is_null() {
            return;
        }
        let total_pixels = (ORB_WIDTH * ORB_HEIGHT) as usize;
        let buf = unsafe { std::slice::from_raw_parts_mut(self.bits, total_pixels) };

        match self.state {
            OrbState::Recording => render_gas_fluid(self.frame, buf, false),
            OrbState::Transcribing => render_gas_fluid(self.frame, buf, true),
        }

        unsafe {
            let (pos_x, pos_y) = calc_position();
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
    unsafe {
        let _ = SystemParametersInfoW(
            SPI_GETWORKAREA,
            0,
            Some(&mut work_area as *mut _ as *mut _),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        );
    }
    let screen_w = work_area.right - work_area.left;
    let x = work_area.left + (screen_w - ORB_WIDTH) / 2;
    // Anchored right at the bottom edge, 2px above taskbar
    let y = (work_area.bottom - ORB_HEIGHT - 2).max(work_area.top);
    (x, y)
}

#[inline]
fn add_light(r_acc: &mut f32, g_acc: &mut f32, b_acc: &mut f32, a_acc: &mut f32, r: f32, g: f32, b: f32, intensity: f32) {
    if intensity <= 0.002 {
        return;
    }
    let int_c = intensity.clamp(0.0, 1.0);
    *r_acc += r * int_c;
    *g_acc += g * int_c;
    *b_acc += b * int_c;
    *a_acc += int_c;
}

#[inline]
fn pack_premul(r: f32, g: f32, b: f32, a: f32) -> u32 {
    let a_c = a.clamp(0.0, 1.0);
    if a_c <= 0.002 {
        return 0;
    }
    let a_byte = (a_c * 255.0 + 0.5) as u32;
    let r_byte = (r.clamp(0.0, 1.0) * a_c * 255.0 + 0.5) as u32;
    let g_byte = (g.clamp(0.0, 1.0) * a_c * 255.0 + 0.5) as u32;
    let b_byte = (b.clamp(0.0, 1.0) * a_c * 255.0 + 0.5) as u32;
    (a_byte << 24) | (r_byte << 16) | (g_byte << 8) | b_byte
}

/// Render 36px translucent glass sphere with floating zero-gravity gas-fluid nebula inside.
fn render_gas_fluid(frame: u32, buf: &mut [u32], is_loading: bool) {
    let speed = if is_loading { 0.065 } else { 0.038 };
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
            let mut r = 0.0f32;
            let mut g = 0.0f32;
            let mut b = 0.0f32;
            let mut a = 0.0f32;

            // 1. Ambient outer aura
            let aura = (-d_sq / 260.0).exp() * 0.28;
            add_light(&mut r, &mut g, &mut b, &mut a, 0.12, 0.45, 1.0, aura);

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

                let density = (0.42 + 0.28 * g1 + 0.22 * g2 + 0.18 * g3).clamp(0.0, 1.0);
                let core_falloff = (1.0 - (r_norm * 0.85).powi(2)).clamp(0.0, 1.0);
                let gas_volume = (density * core_falloff).powf(1.15);

                // Gas-fluid body (between liquid and gas)
                let body_int = gas_volume * 0.88 * sphere_edge;
                let cr = 0.08 + 0.20 * density;
                let cg = 0.38 + 0.38 * density;
                let cb = 0.98;
                add_light(&mut r, &mut g, &mut b, &mut a, cr, cg, cb, body_int);

                // Luminous gas filaments & tendrils
                let filament = (gas_volume * 1.55 - 0.32).clamp(0.0, 1.0);
                add_light(&mut r, &mut g, &mut b, &mut a, 0.45, 0.92, 1.0, filament * 0.70 * sphere_edge);

                // Floating ion micro-sparks drifting in zero-g gas
                let sp1_x = (t * 1.3).sin() * 6.5;
                let sp1_y = (t * 1.7).cos() * 6.5;
                let sp1 = (-((dx - sp1_x).powi(2) + (dy - sp1_y).powi(2)) / 3.8).exp() * 0.85;
                add_light(&mut r, &mut g, &mut b, &mut a, 0.65, 0.95, 1.0, sp1 * sphere_edge);

                let sp2_x = (t * 2.1 + 2.0).cos() * 8.0;
                let sp2_y = (t * 1.5 + 1.0).sin() * 8.0;
                let sp2 = (-((dx - sp2_x).powi(2) + (dy - sp2_y).powi(2)) / 3.2).exp() * 0.75;
                add_light(&mut r, &mut g, &mut b, &mut a, 0.55, 0.90, 1.0, sp2 * sphere_edge);

                // --- TRANSLUCENT GLASS SHELL ---
                // Fresnel rim
                let rim = fresnel * (0.48 + 0.20 * (t * 2.5).sin()) * sphere_edge;
                add_light(&mut r, &mut g, &mut b, &mut a, 0.48, 0.85, 1.0, rim);

                // Primary specular highlight (upper-left curve)
                let hl_dx = dx + 5.2;
                let hl_dy = dy + 5.8;
                let hl_d_sq = hl_dx * hl_dx + hl_dy * hl_dy;
                let hl = (-hl_d_sq / 8.5).exp() * 0.95 * sphere_edge;
                add_light(&mut r, &mut g, &mut b, &mut a, 1.0, 1.0, 1.0, hl);

                // Secondary bounce highlight (lower-right)
                let hl2_dx = dx - 4.8;
                let hl2_dy = dy - 5.2;
                let hl2_d_sq = hl2_dx * hl2_dx + hl2_dy * hl2_dy;
                let hl2 = (-hl2_d_sq / 13.0).exp() * 0.36 * sphere_edge;
                add_light(&mut r, &mut g, &mut b, &mut a, 0.35, 0.80, 1.0, hl2);
            }

            buf[row_offset + x as usize] = pack_premul(r, g, b, a);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_render_recording_non_empty() {
        let mut buf = vec![0u32; (ORB_WIDTH * ORB_HEIGHT) as usize];
        render_gas_fluid(10, &mut buf, false);
        let non_zero = buf.iter().filter(|&&p| p != 0).count();
        assert!(non_zero > 300, "Gas fluid orb should render visible pixels");
    }

    #[test]
    fn test_render_loading_non_empty() {
        let mut buf = vec![0u32; (ORB_WIDTH * ORB_HEIGHT) as usize];
        render_gas_fluid(10, &mut buf, true);
        let non_zero = buf.iter().filter(|&&p| p != 0).count();
        assert!(non_zero > 300, "Gas fluid orb should render visible pixels");
    }
}
