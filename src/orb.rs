// Fancy animated procedural glowing orb overlay for mnvoice.
// Rendered via Win32 layered window (UpdateLayeredWindow) with 32-bit premultiplied ARGB.
// Centered at bottom-middle of the screen above the taskbar.
// Click-through, non-activating, zero interference with active apps.

use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{w, PCWSTR};

pub const ORB_WIDTH: i32 = 120;
pub const ORB_HEIGHT: i32 = 120;
const ORB_CLASS_NAME: PCWSTR = w!("mnvoiceOrbClass");

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
            OrbState::Recording => render_recording(self.frame, buf),
            OrbState::Transcribing => render_transcribing(self.frame, buf),
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
    // Float 32px above the taskbar
    let y = (work_area.bottom - ORB_HEIGHT - 32).max(work_area.top);
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

/// Recording: rhythmic breathing cyan/violet iris with expanding acoustic ripple wave.
fn render_recording(frame: u32, buf: &mut [u32]) {
    let t = frame as f32 * 0.033;
    let cx = 60.0f32;
    let cy = 60.0f32;

    let pulse = 0.5 + 0.5 * (t * 3.5).sin();
    let r_core = 18.0 + 3.0 * pulse;

    let wave = (t * 1.25).fract();
    let r_wave = r_core + wave * 28.0;
    let wave_alpha = (1.0 - wave) * (1.0 - wave) * 0.55;

    for y in 0..ORB_HEIGHT {
        let dy = y as f32 - cy + 0.5;
        let row_offset = (y * ORB_WIDTH) as usize;
        for x in 0..ORB_WIDTH {
            let dx = x as f32 - cx + 0.5;
            let d_sq = dx * dx + dy * dy;

            if d_sq > 58.0 * 58.0 {
                buf[row_offset + x as usize] = 0;
                continue;
            }

            let d = d_sq.sqrt();
            let mut r = 0.0f32;
            let mut g = 0.0f32;
            let mut b = 0.0f32;
            let mut a = 0.0f32;

            // 1. Ambient deep-violet aura
            let aura = (-d_sq / 750.0).exp() * 0.38;
            add_light(&mut r, &mut g, &mut b, &mut a, 0.45, 0.20, 0.95, aura);

            // 2. Core sphere with smooth antialiased edge & radial gradient
            if d < r_core + 2.5 {
                let edge = ((r_core + 2.5 - d) / 2.5).clamp(0.0, 1.0);
                let f = (d / r_core).clamp(0.0, 1.0);
                let cr = 0.90 * (1.0 - f) + 0.15 * f;
                let cg = 0.96 * (1.0 - f) + 0.60 * f;
                let cb = 1.0;
                let core_int = edge * (0.85 + 0.15 * pulse);
                add_light(&mut r, &mut g, &mut b, &mut a, cr, cg, cb, core_int);
            }

            // 3. Specular highlight (upper-left sheen)
            let hl_dx = dx + 5.0;
            let hl_dy = dy + 5.0;
            let hl_d_sq = hl_dx * hl_dx + hl_dy * hl_dy;
            let hl = (-hl_d_sq / 32.0).exp() * 0.55;
            add_light(&mut r, &mut g, &mut b, &mut a, 1.0, 1.0, 1.0, hl);

            // 4. Acoustic soundwave ripple
            let ring_dist = (d - r_wave).abs();
            let ring_int = (-ring_dist * ring_dist / 6.0).exp() * wave_alpha;
            add_light(&mut r, &mut g, &mut b, &mut a, 0.2, 0.9, 1.0, ring_int);

            buf[row_offset + x as usize] = pack_premul(r, g, b, a);
        }
    }
}

/// Transcribing: rapid orbiting multi-color constellation around a warm shimmering core.
fn render_transcribing(frame: u32, buf: &mut [u32]) {
    let t = frame as f32 * 0.033;
    let cx = 60.0f32;
    let cy = 60.0f32;

    let core_pulse = 0.5 + 0.5 * (t * 7.0).sin();
    let r_core = 13.0 + 2.5 * core_pulse;

    let rot = t * 5.5;
    let r_orbit = 25.0;

    // 3 orbital nodes (angles: rot, rot + 120 deg, rot + 240 deg)
    let nodes = [
        (rot, (1.0f32, 0.82f32, 0.20f32)),        // Amber-gold
        (rot + 2.094395, (0.15f32, 0.90f32, 1.0f32)), // Electric cyan
        (rot + 4.18879, (1.0f32, 0.30f32, 0.85f32)),  // Magenta
    ];

    let node_coords: Vec<(f32, f32, (f32, f32, f32))> = nodes
        .iter()
        .map(|&(ang, color)| {
            let nx = cx + r_orbit * ang.cos();
            let ny = cy + r_orbit * ang.sin();
            (nx, ny, color)
        })
        .collect();

    for y in 0..ORB_HEIGHT {
        let dy = y as f32 - cy + 0.5;
        let row_offset = (y * ORB_WIDTH) as usize;
        for x in 0..ORB_WIDTH {
            let dx = x as f32 - cx + 0.5;
            let d_sq = dx * dx + dy * dy;

            if d_sq > 58.0 * 58.0 {
                buf[row_offset + x as usize] = 0;
                continue;
            }

            let d = d_sq.sqrt();
            let mut r = 0.0f32;
            let mut g = 0.0f32;
            let mut b = 0.0f32;
            let mut a = 0.0f32;

            // 1. Warm central core (processing glow)
            if d < r_core + 2.0 {
                let edge = ((r_core + 2.0 - d) / 2.0).clamp(0.0, 1.0);
                let f = (d / r_core).clamp(0.0, 1.0);
                let cr = 1.0 * (1.0 - f) + 0.85 * f;
                let cg = 0.95 * (1.0 - f) + 0.50 * f;
                let cb = 0.80 * (1.0 - f) + 0.15 * f;
                let core_int = edge * (0.80 + 0.20 * core_pulse);
                add_light(&mut r, &mut g, &mut b, &mut a, cr, cg, cb, core_int);
            }

            // 2. Faint connecting energy orbital ring
            let ring_dist = (d - r_orbit).abs();
            let ring_int = (-ring_dist * ring_dist / 10.0).exp() * 0.22;
            add_light(&mut r, &mut g, &mut b, &mut a, 0.5, 0.6, 1.0, ring_int);

            // 3. Orbiting particles with Gaussian falloff
            let px = x as f32 + 0.5;
            let py = y as f32 + 0.5;
            for &(nx, ny, (nr, ng, nb)) in &node_coords {
                let nd_sq = (px - nx) * (px - nx) + (py - ny) * (py - ny);
                if nd_sq < 14.0 * 14.0 {
                    let p_int = (-nd_sq / 22.0).exp() * 0.85;
                    add_light(&mut r, &mut g, &mut b, &mut a, nr, ng, nb, p_int);
                }
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
        render_recording(10, &mut buf);
        let non_zero = buf.iter().filter(|&&p| p != 0).count();
        assert!(non_zero > 500, "Recording orb should render visible pixels");
    }

    #[test]
    fn test_render_transcribing_non_empty() {
        let mut buf = vec![0u32; (ORB_WIDTH * ORB_HEIGHT) as usize];
        render_transcribing(10, &mut buf);
        let non_zero = buf.iter().filter(|&&p| p != 0).count();
        assert!(non_zero > 500, "Transcribing orb should render visible pixels");
    }

    #[test]
    fn test_pack_premul_alpha() {
        let val = pack_premul(1.0, 0.5, 0.0, 1.0);
        let a = (val >> 24) & 0xFF;
        let r = (val >> 16) & 0xFF;
        let g = (val >> 8) & 0xFF;
        let b = val & 0xFF;
        assert_eq!(a, 255);
        assert_eq!(r, 255);
        assert_eq!(g, 128);
        assert_eq!(b, 0);
    }
}
