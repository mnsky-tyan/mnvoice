// Fancy animated glowing blue orb overlay with real-time streaming text for mnvoice.
// Rendered via Win32 layered window (UpdateLayeredWindow) with 32-bit premultiplied ARGB.
// Centered at bottom-middle of the screen, just above the taskbar.
// Click-through, non-activating, zero interference with active apps.

use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{w, PCWSTR};

pub const ORB_WIDTH: i32 = 640;
pub const ORB_HEIGHT: i32 = 160;
const ORB_CLASS_NAME: PCWSTR = w!("mnvoiceOrbClass");

const ORB_CX: f32 = 320.0;
const ORB_CY: f32 = 118.0;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum OrbState {
    Recording,
    Transcribing,
}

pub struct Orb {
    hwnd: HWND,
    dc_mem: HDC,
    bitmap: HBITMAP,
    font: HFONT,
    bits: *mut u32,
    pub state: OrbState,
    frame: u32,
    visible: bool,
    text: String,
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

            let font = CreateFontW(
                -16,
                0,
                0,
                0,
                FW_SEMIBOLD.0 as i32,
                0,
                0,
                0,
                DEFAULT_CHARSET.0 as u32,
                OUT_DEFAULT_PRECIS.0 as u32,
                CLIP_DEFAULT_PRECIS.0 as u32,
                CLEARTYPE_QUALITY.0 as u32,
                DEFAULT_PITCH.0 as u32,
                windows::core::w!("Segoe UI"),
            );
            if !font.0.is_null() {
                let _ = SelectObject(dc_mem, font);
            }

            Ok(Self {
                hwnd,
                dc_mem,
                bitmap,
                font,
                bits: bits_ptr as *mut u32,
                state: OrbState::Recording,
                frame: 0,
                visible: false,
                text: String::new(),
            })
        }
    }

    pub fn show(&mut self, state: OrbState) {
        self.state = state;
        self.frame = 0;
        self.text.clear();
        self.reposition();
        self.render_frame();
        unsafe {
            let _ = ShowWindow(self.hwnd, SW_SHOWNA);
        }
        self.visible = true;
    }

    pub fn set_text(&mut self, text: &str) {
        if self.text != text {
            self.text = text.to_string();
            if self.visible {
                self.render_frame();
            }
        }
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
            self.text.clear();
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

        // 1. Procedural rendering of the blue orb
        match self.state {
            OrbState::Recording => render_blue_recording(self.frame, buf),
            OrbState::Transcribing => render_blue_loading(self.frame, buf),
        }

        // 2. Render streaming text badge above the orb if present
        if !self.text.is_empty() {
            render_text_overlay(self.dc_mem, &self.text, buf);
        }

        // 3. Update layered window
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
            if !self.font.0.is_null() {
                let _ = DeleteObject(self.font);
            }
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
    // Lower position: sits just 8px above the taskbar / screen bottom
    let y = (work_area.bottom - ORB_HEIGHT - 8).max(work_area.top);
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

/// Recording: living blue/cyan orb with gentle breathing pulse and acoustic soundwave ripple.
fn render_blue_recording(frame: u32, buf: &mut [u32]) {
    let t = frame as f32 * 0.033;
    let cx = ORB_CX;
    let cy = ORB_CY;

    let pulse = 0.5 + 0.5 * (t * 3.5).sin();
    let r_core = 17.0 + 3.0 * pulse;

    let wave = (t * 1.25).fract();
    let r_wave = r_core + wave * 25.0;
    let wave_alpha = (1.0 - wave) * (1.0 - wave) * 0.55;

    // Clear entire buffer first
    buf.fill(0);

    for y in 70..ORB_HEIGHT {
        let dy = y as f32 - cy + 0.5;
        let row_offset = (y * ORB_WIDTH) as usize;
        for x in (ORB_WIDTH / 2 - 60)..(ORB_WIDTH / 2 + 60) {
            let dx = x as f32 - cx + 0.5;
            let d_sq = dx * dx + dy * dy;

            if d_sq > 54.0 * 54.0 {
                continue;
            }

            let d = d_sq.sqrt();
            let mut r = 0.0f32;
            let mut g = 0.0f32;
            let mut b = 0.0f32;
            let mut a = 0.0f32;

            // 1. Ambient soft deep-blue aura
            let aura = (-d_sq / 700.0).exp() * 0.35;
            add_light(&mut r, &mut g, &mut b, &mut a, 0.15, 0.45, 1.0, aura);

            // 2. Core sphere: luminous cyan & cobalt blue gradient
            if d < r_core + 2.5 {
                let edge = ((r_core + 2.5 - d) / 2.5).clamp(0.0, 1.0);
                let f = (d / r_core).clamp(0.0, 1.0);
                let cr = 0.85 * (1.0 - f) + 0.10 * f;
                let cg = 0.95 * (1.0 - f) + 0.60 * f;
                let cb = 1.0;
                let core_int = edge * (0.85 + 0.15 * pulse);
                add_light(&mut r, &mut g, &mut b, &mut a, cr, cg, cb, core_int);
            }

            // 3. Specular sheen (upper-left)
            let hl_dx = dx + 5.0;
            let hl_dy = dy + 5.0;
            let hl_d_sq = hl_dx * hl_dx + hl_dy * hl_dy;
            let hl = (-hl_d_sq / 30.0).exp() * 0.55;
            add_light(&mut r, &mut g, &mut b, &mut a, 1.0, 1.0, 1.0, hl);

            // 4. Acoustic soundwave ripple
            let ring_dist = (d - r_wave).abs();
            let ring_int = (-ring_dist * ring_dist / 6.0).exp() * wave_alpha;
            add_light(&mut r, &mut g, &mut b, &mut a, 0.25, 0.85, 1.0, ring_int);

            buf[row_offset + x as usize] = pack_premul(r, g, b, a);
        }
    }
}

/// Loading/Transcribing: the same blue orb, smoothly glowing with a steady blue breathing pulse.
/// (NO orange particles, NO fancy multi-colored constellation - pure blue theme).
fn render_blue_loading(frame: u32, buf: &mut [u32]) {
    let t = frame as f32 * 0.033;
    let cx = ORB_CX;
    let cy = ORB_CY;

    let pulse = 0.5 + 0.5 * (t * 5.0).sin();
    let r_core = 16.0 + 2.5 * pulse;

    buf.fill(0);

    for y in 70..ORB_HEIGHT {
        let dy = y as f32 - cy + 0.5;
        let row_offset = (y * ORB_WIDTH) as usize;
        for x in (ORB_WIDTH / 2 - 60)..(ORB_WIDTH / 2 + 60) {
            let dx = x as f32 - cx + 0.5;
            let d_sq = dx * dx + dy * dy;

            if d_sq > 54.0 * 54.0 {
                continue;
            }

            let d = d_sq.sqrt();
            let mut r = 0.0f32;
            let mut g = 0.0f32;
            let mut b = 0.0f32;
            let mut a = 0.0f32;

            // 1. Ambient deep-blue aura
            let aura = (-d_sq / 650.0).exp() * 0.40;
            add_light(&mut r, &mut g, &mut b, &mut a, 0.20, 0.55, 1.0, aura);

            // 2. Concentric radiant pulse ring
            let ring_dist = (d - (r_core + 8.0)).abs();
            let ring_int = (-ring_dist * ring_dist / 8.0).exp() * (0.30 + 0.15 * pulse);
            add_light(&mut r, &mut g, &mut b, &mut a, 0.30, 0.80, 1.0, ring_int);

            // 3. Core sphere: clean glowing electric blue
            if d < r_core + 2.0 {
                let edge = ((r_core + 2.0 - d) / 2.0).clamp(0.0, 1.0);
                let f = (d / r_core).clamp(0.0, 1.0);
                let cr = 0.90 * (1.0 - f) + 0.15 * f;
                let cg = 0.95 * (1.0 - f) + 0.65 * f;
                let cb = 1.0;
                let core_int = edge * (0.80 + 0.20 * pulse);
                add_light(&mut r, &mut g, &mut b, &mut a, cr, cg, cb, core_int);
            }

            // 4. Specular highlight
            let hl_dx = dx + 4.5;
            let hl_dy = dy + 4.5;
            let hl_d_sq = hl_dx * hl_dx + hl_dy * hl_dy;
            let hl = (-hl_d_sq / 28.0).exp() * 0.50;
            add_light(&mut r, &mut g, &mut b, &mut a, 1.0, 1.0, 1.0, hl);

            buf[row_offset + x as usize] = pack_premul(r, g, b, a);
        }
    }
}

/// Render a translucent dark rounded pill with Segoe UI text above the blue orb.
fn render_text_overlay(dc: HDC, text: &str, buf: &mut [u32]) {
    unsafe {
        let mut text_w: Vec<u16> = text.encode_utf16().collect();
        let mut size = SIZE::default();
        let _ = GetTextExtentPoint32W(dc, &text_w, &mut size);

        let max_text_w = 540;
        let text_w_clamped = size.cx.clamp(40, max_text_w);
        let pill_w = text_w_clamped + 36;
        let pill_h = 36;
        let pill_x = (ORB_WIDTH - pill_w) / 2;
        let pill_y = 30;

        // 1. Draw rounded translucent pill background directly into buffer
        let pill_r = 14.0f32;
        let px0 = pill_x as f32;
        let py0 = pill_y as f32;
        let px1 = (pill_x + pill_w) as f32;
        let py1 = (pill_y + pill_h) as f32;

        for y in pill_y..(pill_y + pill_h) {
            let y_f = y as f32 + 0.5;
            let row_offset = (y * ORB_WIDTH) as usize;
            for x in pill_x..(pill_x + pill_w) {
                let x_f = x as f32 + 0.5;

                // Rounded corner distance
                let dx = if x_f < px0 + pill_r {
                    px0 + pill_r - x_f
                } else if x_f > px1 - pill_r {
                    x_f - (px1 - pill_r)
                } else {
                    0.0
                };

                let dy = if y_f < py0 + pill_r {
                    py0 + pill_r - y_f
                } else if y_f > py1 - pill_r {
                    y_f - (py1 - pill_r)
                } else {
                    0.0
                };

                let d = (dx * dx + dy * dy).sqrt();
                if d > pill_r {
                    continue;
                }

                let edge = (pill_r - d).clamp(0.0, 1.0);
                // Translucent dark slate pill: rgb(15, 20, 30), opacity 85%
                let a = edge * 0.85;
                let r = 0.06;
                let g = 0.08;
                let b = 0.12;
                buf[row_offset + x as usize] = pack_premul(r, g, b, a);
            }
        }

        // 2. Draw text using GDI
        SetBkMode(dc, TRANSPARENT);
        SetTextColor(dc, COLORREF(0x00FFFFFF));

        let mut rect = RECT {
            left: pill_x + 10,
            top: pill_y,
            right: pill_x + pill_w - 10,
            bottom: pill_y + pill_h,
        };

        let _ = DrawTextW(
            dc,
            &mut text_w,
            &mut rect,
            DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS | DT_NOPREFIX,
        );

        // 3. Fix alpha for text pixels (GDI draws RGB but leaves A=0)
        for y in pill_y..(pill_y + pill_h) {
            let row_offset = (y * ORB_WIDTH) as usize;
            for x in pill_x..(pill_x + pill_w) {
                let pixel = buf[row_offset + x as usize];
                let b = (pixel & 0xFF) as u8;
                let g = ((pixel >> 8) & 0xFF) as u8;
                let r = ((pixel >> 16) & 0xFF) as u8;
                let mut a = ((pixel >> 24) & 0xFF) as u8;

                let text_val = r.max(g).max(b);
                if text_val > a {
                    a = text_val;
                    buf[row_offset + x as usize] = ((a as u32) << 24)
                        | ((r as u32) << 16)
                        | ((g as u32) << 8)
                        | (b as u32);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_render_recording_non_empty() {
        let mut buf = vec![0u32; (ORB_WIDTH * ORB_HEIGHT) as usize];
        render_blue_recording(10, &mut buf);
        let non_zero = buf.iter().filter(|&&p| p != 0).count();
        assert!(non_zero > 500, "Recording orb should render visible pixels");
    }

    #[test]
    fn test_render_loading_non_empty() {
        let mut buf = vec![0u32; (ORB_WIDTH * ORB_HEIGHT) as usize];
        render_blue_loading(10, &mut buf);
        let non_zero = buf.iter().filter(|&&p| p != 0).count();
        assert!(non_zero > 500, "Loading orb should render visible pixels");
    }
}
