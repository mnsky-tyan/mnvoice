// The orb's Linux surface: the same platform-neutral render (`orb_render`),
// blitted to a Wayland layer-shell surface.
//
// Why layer-shell: the orb is an overlay, not a window - it must sit above
// every app, take no keyboard focus, and swallow no clicks. The `zwlr_layer_shell_v1`
// protocol is what panels and OSDs use for exactly that, and KWin advertises it
// (version 5 here). An xdg_toplevel would be a window in the user's face with a
// taskbar entry; an override-redirect X11 window would be fighting the
// compositor's stacking.
//
// The surface lives on the Overlay layer, anchored to the bottom centre with a
// small margin, with an EMPTY input region: the orb is click-through like the
// Windows one (`WS_EX_TRANSPARENT` there, `wl_surface.set_input_region` here).
//
// The scope is `on-screen-display`, not an app name, and that is load-bearing.
// KWin derives a layer surface's window TYPE from its scope
// (layershellv1window.cpp: `scopeToType`), and an unrecognized scope falls back
// to `WindowType::Normal` - a normal window. The decoration effects then treat
// the orb as an app window: KDE-Rounded-Corners drew its 1px black outline and
// 8px corner radius around the 48px surface, which the user saw as four black
// edges boxing the orb in. `on-screen-display` is the protocol's own word for
// exactly this kind of indicator and maps to `WindowType::OnScreenDisplay`,
// which no decoration effect decorates.

use crate::config;
use crate::orb_render::{render_gas_fluid, OrbState, ORB_HEIGHT, ORB_WIDTH};
use std::sync::mpsc::Sender;
use std::time::Duration;
use wayland_client::protocol::{wl_compositor, wl_region, wl_shm, wl_shm_pool, wl_surface};
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle};
use wayland_protocols_wlr::layer_shell::v1::client::{zwlr_layer_shell_v1, zwlr_layer_surface_v1};

/// Frames per second while the orb is visible. The Windows orb animates via
/// its message loop; here a plain timer drives the same pace. 30 fps is what
/// the fluid drift needs to look alive, and the render is ~2k pixels, so the
/// CPU cost is negligible (measured: the whole render is a single pass over
/// 2304 u32s).
const FPS: u64 = 30;

/// How far above the bottom edge the orb floats. Windows sits 2px above the
/// taskbar; on Plasma the panel is at the bottom too, so a slightly larger
/// margin keeps the orb clear of it.
const BOTTOM_MARGIN: i32 = 16;

/// The messages the orb thread understands. The app sends `Show` when a
/// dictation starts and `Hide` when it ends, so the orb's lifetime is exactly
/// the recording's - the Windows orb is driven the same way from the tray loop.
pub enum OrbCommand {
    Show(OrbState),
    State(OrbState),
    Hide,
}

/// Spawns the orb thread and returns the handle that drives it. The thread
/// owns the Wayland connection for the life of the process; a failed
/// connection (no Wayland compositor, no layer shell) is logged and the app
/// keeps running without an orb rather than dying, because an indicator is
/// cosmetic next to dictation itself.
pub fn spawn(color: (f32, f32, f32), fluid_level: f32) -> Result<Sender<OrbCommand>, String> {
    let (tx, rx) = std::sync::mpsc::channel::<OrbCommand>();
    std::thread::Builder::new()
        .name("mnvoice-orb".into())
        .spawn(move || {
            if let Err(e) = run(rx, color, fluid_level) {
                eprintln!("mnvoice: orb disabled ({e})");
            }
        })
        .map_err(|e| format!("could not start the orb thread ({e})"))?;
    Ok(tx)
}

struct OrbState_ {
    /// The two shm buffers, swapped per frame so the compositor never reads
    /// pixels the render is writing (a single buffer would tear or, worse,
    /// block the render on a busy compositor).
    ///
    /// The pair lives exactly as long as the surface it is attached to:
    /// created lazily on the first paint of a dictation and destroyed with
    /// the surface when the dictation ends (`Hide` and the compositor's
    /// `Closed` event both take the slots), so every dictation paints into
    /// fresh memfds.
    buffers: [Option<Buffer>; 2],
    next: usize,
    frame: u32,
    visible: bool,
    /// The live layer surface, created per dictation and destroyed when the
    /// dictation ends. KWin resets a layer surface's ENTIRE state on unmap -
    /// anchor, size, margins and the configure handshake all go back to
    /// defaults (layershell_v1.cpp: `state = LayerSurfaceV1State()` on the
    /// unmap commit) - so a hide/show cycle on one surface cannot just
    /// re-attach a buffer: the second dictation died with protocol error 0,
    /// "a buffer has been attached to a layer surface prior to the first
    /// layer_surface.configure event", and KWin kills the connection over it.
    /// Destroying and recreating is what menus do; it needs no reset semantics.
    surface: Option<OrbSurface>,
    state: OrbState,
    color: (f32, f32, f32),
    fluid_level: f32,
    running: bool,
}

/// One live layer surface plus whether its configure has been acknowledged.
/// A fresh surface is unmapped and unconfigured; the handshake is commit ->
/// configure -> ack -> first paint.
struct OrbSurface {
    wl: wl_surface::WlSurface,
    layer: zwlr_layer_surface_v1::ZwlrLayerSurfaceV1,
    configured: bool,
}

/// The byte size of one frame's pixels, shared by the mapping and its
/// teardown so the two cannot disagree.
const BUFFER_BYTES: usize = (ORB_WIDTH * ORB_HEIGHT * 4) as usize;

struct Buffer {
    wl: wayland_client::protocol::wl_buffer::WlBuffer,
    /// The mmap'd pixels the render writes into, munmapped when the buffer
    /// drops. The file backs the shm pool and is unlinked immediately.
    map: *mut u8,
    _pool: wayland_client::protocol::wl_shm_pool::WlShmPool,
    /// Whether the compositor is still reading this buffer.
    ///
    /// The protocol hands the buffer to the compositor on commit and gives it
    /// back with `wl_buffer.release`. Until that release arrives the pixels
    /// are the compositor's to read, and writing them again is a data race on
    /// the buffer - the "tear" the double-buffer comment above promises to
    /// avoid. `release` is tracked per buffer, so the render can only ever
    /// write into a buffer the compositor has handed back. Buffers never
    /// outlive the surface, so the flag only tracks reuse within one
    /// dictation.
    in_use: bool,
}

// The mapped pixels are only ever reached through the render's `&mut [u32]`,
// one buffer at a time, on the one thread that owns the connection.
unsafe impl Send for Buffer {}

impl Drop for Buffer {
    fn drop(&mut self) {
        // The destroyed buffer's storage is never written again (a fresh
        // memfd is created per dictation), and the compositor maps the pool
        // through its own fd, so unmapping our mapping cannot pull storage
        // from under it.
        unsafe { rustix::mm::munmap(self.map as *mut core::ffi::c_void, BUFFER_BYTES) }
            .expect("munmap orb shm");
    }
}

impl OrbState_ {
    /// A dictation started. The surface is created by the caller when this is
    /// the first frame of a session.
    fn mark_visible(&mut self, state: OrbState) {
        self.state = state;
        self.visible = true;
    }

    /// A dictation ended. The caller destroys the surface; this flag only
    /// stops the render loop spending frames on nothing.
    fn mark_hidden(&mut self) {
        self.visible = false;
    }

    /// Whether a frame may be rendered and committed. Painting needs a live
    /// dictation and a surface whose configure has been acknowledged.
    fn can_paint(&self) -> bool {
        self.visible
            && self
                .surface
                .as_ref()
                .is_some_and(|surface| surface.configured)
    }

    /// Picks the buffer the next frame may be rendered into, or `None` when
    /// the compositor still holds both.
    ///
    /// `st.next` is the preferred one so the render alternates and the
    /// compositor is not reading the buffer being written; the other is used
    /// when the preferred one has not been released yet. A slot with no
    /// buffer yet is free by definition - the compositor has not seen it, and
    /// the render creates it on the spot.
    fn free_buffer(&self) -> Option<usize> {
        let free = |idx: usize| {
            self.buffers[idx]
                .as_ref()
                .is_none_or(|buffer| !buffer.in_use)
        };
        if free(self.next) {
            Some(self.next)
        } else if free((self.next + 1) % 2) {
            Some((self.next + 1) % 2)
        } else {
            None
        }
    }
}

/// Creates the orb's layer surface: overlay layer, bottom-centre anchored,
/// click-through, then an empty commit to request the configure. The caller
/// paints once the Dispatch has acknowledged the configure.
fn create_surface(
    compositor: &wl_compositor::WlCompositor,
    layer_shell: &zwlr_layer_shell_v1::ZwlrLayerShellV1,
    qh: &QueueHandle<OrbState_>,
    st: &mut OrbState_,
) {
    let surface = compositor.create_surface(qh, ());
    let layer = layer_shell.get_layer_surface(
        &surface,
        None,
        zwlr_layer_shell_v1::Layer::Overlay,
        // A window type, not a label: see the module comment. Renaming this to
        // something "more descriptive" gives the orb a titlebar-shaped outline.
        "on-screen-display".into(),
        qh,
        (),
    );
    // Never take the keyboard: the orb is an indicator, and a keystroke meant
    // for the user's editor must not land here. This is the protocol default,
    // stated on purpose - an Overlay-layer surface that accepts focus is
    // activated by KWin (layershellv1window.cpp: `handleAcceptsFocusChanged`
    // calls `activateWindow` for the top and overlay layers), which would pull
    // the caret out of whatever the user was typing in.
    layer.set_keyboard_interactivity(zwlr_layer_surface_v1::KeyboardInteractivity::None);
    layer.set_anchor(zwlr_layer_surface_v1::Anchor::Bottom);
    layer.set_size(ORB_WIDTH, ORB_HEIGHT);
    layer.set_margin(0, 0, BOTTOM_MARGIN, 0);
    layer.set_exclusive_zone(-1);
    // Click-through: an empty input region delivers pointer events to whatever
    // is underneath, like the Windows orb's WS_EX_TRANSPARENT.
    let region: wl_region::WlRegion = compositor.create_region(qh, ());
    region.add(0, 0, 0, 0);
    surface.set_input_region(Some(&region));
    region.destroy();
    surface.commit(); // the handshake: this asks for the configure
    st.surface = Some(OrbSurface {
        wl: surface,
        layer,
        configured: false,
    });
}

fn run(
    rx: std::sync::mpsc::Receiver<OrbCommand>,
    color: (f32, f32, f32),
    fluid_level: f32,
) -> Result<(), String> {
    let conn = Connection::connect_to_env().map_err(|e| format!("no Wayland display ({e})"))?;
    let (globals, mut queue) = wayland_client::globals::registry_queue_init(&conn)
        .map_err(|e| format!("Wayland registry failed ({e})"))?;
    let qh: QueueHandle<OrbState_> = queue.handle();

    let compositor: wl_compositor::WlCompositor = globals
        .bind(&qh, 1..=6, ())
        .map_err(|e| format!("wl_compositor unavailable ({e})"))?;
    let shm: wl_shm::WlShm = globals
        .bind(&qh, 1..=2, ())
        .map_err(|e| format!("wl_shm unavailable ({e})"))?;
    let layer_shell: zwlr_layer_shell_v1::ZwlrLayerShellV1 =
        globals.bind(&qh, 1..=5, ()).map_err(|e| {
            format!("layer-shell unavailable: your compositor does not support overlays ({e})")
        })?;

    let mut st = OrbState_ {
        buffers: [None, None],
        next: 0,
        frame: 0,
        visible: false,
        surface: None,
        state: OrbState::Recording,
        color,
        fluid_level,
        running: true,
    };

    let interval = Duration::from_millis(1000 / FPS);
    let mut last_render = std::time::Instant::now();

    while st.running {
        // One command per poll: the app sends at most a few per second, so
        // blocking here is the idle cost - zero CPU while the orb is hidden.
        match rx.recv_timeout(if st.visible {
            interval
        } else {
            Duration::from_secs(3600)
        }) {
            Ok(OrbCommand::Show(s)) => {
                st.mark_visible(s);
                // A fresh surface per session: KWin resets a layer surface's
                // whole state on unmap (see `OrbState_.surface`), so the
                // second dictation on a reused surface died with protocol
                // error 0. Creating and destroying is what menus do.
                if st.surface.is_none() {
                    create_surface(&compositor, &layer_shell, &qh, &mut st);
                }
            }
            Ok(OrbCommand::State(s)) => st.state = s,
            Ok(OrbCommand::Hide) => {
                st.mark_hidden();
                // Destroying the surface unmaps it and takes its state with
                // it; the next dictation gets a fresh handshake. Keeping the
                // surface and re-attaching is what protocol error 0 was.
                if let Some(surface) = st.surface.take() {
                    surface.layer.destroy();
                    surface.wl.destroy();
                }
                // The buffers die with the surface: their destroy requests
                // follow the surface destroy on the wire, and the destroyed
                // buffer's storage is never reused - the next dictation's
                // paint loop recreates the pair lazily, one fresh memfd+mmap
                // pair per dictation. wl_surface.attach permits destroying a
                // wl_buffer before wl_buffer.release as long as the
                // underlying buffer storage isn't re-used, so a release that
                // never arrives cannot leave a buffer marked in use for the
                // rest of the process; the surface-destroy-first ordering is
                // belt-and-braces, not the guarantee.
                for slot in st.buffers.iter_mut().flatten() {
                    slot.wl.destroy();
                    slot._pool.destroy();
                }
                st.buffers = [None, None];
            }
            Err(e) => {
                // A timeout is the frame tick, not a shutdown. The two used to
                // be the same arm: while hidden the wait is an hour, but as
                // soon as a dictation showed the orb the wait became one frame
                // (33ms), so the very first Show ended the thread 33ms later -
                // and `run` returned Ok, so nothing was logged. Every later
                // `send` hit a dead channel and was dropped by its `let _ =`,
                // which is the whole reason the orb looked unimplemented on a
                // machine where the hotkey, the orb state and the tray all
                // worked. Only a disconnected channel - the app dropped its
                // sender, so it is shutting down - ends the loop.
                if let std::sync::mpsc::RecvTimeoutError::Disconnected = e {
                    st.running = false;
                }
            }
        }

        if !st.running {
            break;
        }

        if st.can_paint() {
            let now = std::time::Instant::now();
            if now.duration_since(last_render) >= interval {
                last_render = now;
                // Only paint into a buffer the compositor has released. If
                // both are still out, this frame is skipped and the last
                // committed frame stays on screen - a still orb beats writing
                // into pixels the compositor is reading.
                let paint = st.free_buffer();
                if let Some(paint) = paint {
                    st.frame += 1;
                    let index = paint;
                    if st.buffers[index].is_none() {
                        st.buffers[index] = Some(make_buffer(&shm, &qh));
                    }
                    // The render writes into the buffer's map, and the frame
                    // counter advances once per paint, so both happen through
                    // one borrow instead of two.
                    let (paint, state, frame, color, fluid) =
                        (index, st.state, st.frame, st.color, st.fluid_level);
                    let surface = st
                        .surface
                        .as_ref()
                        .expect("can_paint guarantees a configured surface");
                    if let Some(buffer) = st.buffers[paint].as_mut() {
                        let pixels = unsafe {
                            std::slice::from_raw_parts_mut(
                                buffer.map as *mut u32,
                                (ORB_WIDTH * ORB_HEIGHT) as usize,
                            )
                        };
                        render_gas_fluid(frame, pixels, state, color, fluid);
                        surface.wl.attach(Some(&buffer.wl), 0, 0);
                        surface.wl.damage(0, 0, ORB_WIDTH as i32, ORB_HEIGHT as i32);
                        surface.wl.commit();
                        // The buffer now belongs to the compositor until it
                        // releases it.
                        buffer.in_use = true;
                        st.next = (st.next + 1) % 2;
                    }
                }
            }
        }

        // Dispatch whatever arrived (configure events, release events) without
        // blocking: the recv_timeout above is the pacing, this just drains.
        let _ = queue.dispatch_pending(&mut st);
        queue
            .roundtrip(&mut st)
            .map_err(|e| format!("Wayland roundtrip failed ({e})"))?;
    }
    // A thread that leaves this loop stops accepting commands, so the app's
    // later `Show` sends fail into a `let _` and no future dictation can bring
    // the orb back. Nothing about it is silent any more.
    eprintln!("mnvoice: orb thread exited (no further dictation will show it)");
    Ok(())
}

/// One double-sized shm buffer pair. The file is created unlinked (via
/// memfd on Linux), so nothing touches the disk - the orb keeps the
/// zero-audio-on-disk promise by not even putting its pixels there.
fn make_buffer(shm: &wl_shm::WlShm, qh: &QueueHandle<OrbState_>) -> Buffer {
    use std::os::fd::AsFd;
    let size = BUFFER_BYTES;
    let memfd = rustix::fs::memfd_create(
        rustix::cstr!("mnvoice-orb"),
        rustix::fs::MemfdFlags::CLOEXEC,
    )
    .expect("memfd_create");
    rustix::fs::ftruncate(&memfd, size as u64).expect("ftruncate");
    let map = unsafe {
        rustix::mm::mmap(
            std::ptr::null_mut(),
            size,
            rustix::mm::ProtFlags::READ | rustix::mm::ProtFlags::WRITE,
            rustix::mm::MapFlags::SHARED,
            memfd.as_fd(),
            0,
        )
        .expect("mmap orb shm")
    };
    let pool = shm.create_pool(memfd.as_fd(), size as i32, qh, ());
    let wl = pool.create_buffer(
        0,
        ORB_WIDTH as i32,
        ORB_HEIGHT as i32,
        (ORB_WIDTH * 4) as i32,
        wl_shm::Format::Argb8888,
        qh,
        (),
    );
    Buffer {
        wl,
        map: map as *mut u8,
        _pool: pool,
        // A buffer that has never been committed is not in use: the
        // compositor has not seen it yet.
        in_use: false,
    }
}

impl
    Dispatch<
        wayland_client::protocol::wl_registry::WlRegistry,
        wayland_client::globals::GlobalListContents,
    > for OrbState_
{
    fn event(
        _: &mut Self,
        _: &wayland_client::protocol::wl_registry::WlRegistry,
        _: wayland_client::protocol::wl_registry::Event,
        _: &wayland_client::globals::GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_compositor::WlCompositor, ()> for OrbState_ {
    fn event(
        _: &mut Self,
        _: &wl_compositor::WlCompositor,
        _: wl_compositor::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_shm::WlShm, ()> for OrbState_ {
    fn event(
        _: &mut Self,
        _: &wl_shm::WlShm,
        _: wl_shm::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_surface::WlSurface, ()> for OrbState_ {
    fn event(
        _: &mut Self,
        _: &wl_surface::WlSurface,
        _: wl_surface::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_shm_pool::WlShmPool, ()> for OrbState_ {
    fn event(
        _: &mut Self,
        _: &wl_shm_pool::WlShmPool,
        _: wl_shm_pool::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wayland_client::protocol::wl_buffer::WlBuffer, ()> for OrbState_ {
    fn event(
        state: &mut Self,
        buffer: &wayland_client::protocol::wl_buffer::WlBuffer,
        event: wayland_client::protocol::wl_buffer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        // `release` is the compositor handing the buffer back: until it
        // arrives the pixels are the compositor's to read, and the render must
        // not write them again. The event used to be discarded, which is how
        // the double-buffer swap went unvalidated.
        if let wayland_client::protocol::wl_buffer::Event::Release = event {
            let id = buffer.id();
            for slot in state.buffers.iter_mut().flatten() {
                if slot.wl.id() == id {
                    slot.in_use = false;
                    break;
                }
            }
        }
    }
}

impl Dispatch<wl_region::WlRegion, ()> for OrbState_ {
    fn event(
        _: &mut Self,
        _: &wl_region::WlRegion,
        _: wl_region::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<zwlr_layer_shell_v1::ZwlrLayerShellV1, ()> for OrbState_ {
    fn event(
        _: &mut Self,
        _: &zwlr_layer_shell_v1::ZwlrLayerShellV1,
        _: zwlr_layer_shell_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<zwlr_layer_surface_v1::ZwlrLayerSurfaceV1, ()> for OrbState_ {
    fn event(
        state: &mut Self,
        surface: &zwlr_layer_surface_v1::ZwlrLayerSurfaceV1,
        event: zwlr_layer_surface_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_layer_surface_v1::Event::Configure { serial, .. } => {
                // The protocol requires the configure to be acknowledged
                // before a buffer is committed against it.
                surface.ack_configure(serial);
                if let Some(live) = state.surface.as_mut() {
                    live.configured = true;
                }
            }
            zwlr_layer_surface_v1::Event::Closed => {
                // The compositor took the surface (an output went away, say).
                // The protocol asks the client to destroy the resource here,
                // and a proxy that is only dropped is never destroyed, so its
                // object ID would stay in the client's map for the life of
                // the process. The surface proxies go first and the buffers
                // with them, in the same order as the Hide arm.
                if let Some(surface) = state.surface.take() {
                    surface.layer.destroy();
                    surface.wl.destroy();
                }
                for slot in state.buffers.iter_mut().flatten() {
                    slot.wl.destroy();
                    slot._pool.destroy();
                }
                state.buffers = [None, None];
            }
            _ => {}
        }
    }
}

/// The orb's config surface is the same on both platforms: the color preset or
/// hex from `ORB_COLOR`, the level from `ORB_FLUID_LEVEL`. The parse lives in
/// config, so this module only carries what the caller passes.
pub fn orb_config(config: &Option<std::sync::Arc<config::Config>>) -> ((f32, f32, f32), f32) {
    let color = config
        .as_ref()
        .map(|c| c.orb_color)
        .unwrap_or(config::DEFAULT_ORB_COLOR);
    let fluid = config
        .as_ref()
        .map(|c| c.orb_fluid_level)
        .unwrap_or(config::DEFAULT_ORB_FLUID_LEVEL);
    (color, fluid)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> OrbState_ {
        OrbState_ {
            buffers: [None, None],
            next: 0,
            frame: 0,
            visible: false,
            surface: None,
            state: OrbState::Recording,
            color: (1.0, 0.5, 0.9),
            fluid_level: 0.75,
            running: true,
        }
    }

    /// A fresh surface has no configure yet: nothing paints until the Dispatch
    /// acknowledges one, and `can_paint` says so.
    #[test]
    fn no_surface_paints_nothing() {
        let mut st = state();
        st.mark_visible(OrbState::Recording);
        assert!(!st.can_paint(), "no surface, no paint");
    }

    // Whether a configured surface paints is exercised live: a unit test
    // cannot build the proxies (they are not constructible without a
    // Wayland connection), and zero-initializing them is invalid.

    /// Hide is what stops the frame loop spending frames on nothing.
    #[test]
    fn hidden_stops_painting() {
        let mut st = state();
        st.mark_visible(OrbState::Recording);
        st.mark_hidden();
        assert!(!st.can_paint());
    }

    /// The state a dictation moves through is carried by the flag, not
    /// inferred from visibility: the same surface shows both.
    #[test]
    fn a_shown_orb_carries_the_state_it_was_shown_with() {
        let mut st = state();
        st.mark_visible(OrbState::Transcribing);
        assert_eq!(st.state, OrbState::Transcribing);
    }
}
