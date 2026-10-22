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

/// One shm buffer holding one frame. The file is created unlinked (via
/// memfd on Linux), so nothing touches the disk - the orb keeps the
/// zero-audio-on-disk promise by not even putting its pixels there. Each
/// call makes one buffer; the caller holds at most two of them at a time.
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

    /// A scripted stand-in for the compositor, speaking just enough Wayland
    /// wire protocol for the real `run()` to connect, configure and paint:
    /// registry globals, the layer-shell configure handshake, and the shm
    /// buffer objects. It exists because the paint gate under test IS the
    /// `wl_buffer.release` protocol - the evidence has to come from a peer
    /// that really speaks it - and because a real compositor cannot drive the
    /// adversarial half: it hands a buffer back within a frame or two, so
    /// "both buffers held" is a state only a scripted peer can be pinned in.
    /// That is what the `release`/`closed` commands are for.
    ///
    /// Hand-rolled on std sockets rather than a new dev-dependency:
    /// wayland-backend's own test backend is private to that crate, and
    /// `wayland-server` would be a new crate for the dozen messages this
    /// speaks. Every opcode below is the declaration order in wayland.xml
    /// and wlr-layer-shell-unstable-v1.xml, the authoritative sources.
    ///
    /// The orb never reaches the user's session from here: the socket is a
    /// throwaway path in the temp dir and `WAYLAND_DISPLAY` points the run
    /// loop at it, so no real compositor is touched by these tests.
    mod fake_compositor {
        use super::*;
        use std::collections::HashMap;
        use std::io::{Read as _, Write as _};
        use std::os::unix::net::{UnixListener, UnixStream};
        use std::sync::mpsc::{Receiver, Sender};
        use std::sync::{Arc, Mutex, MutexGuard};

        // Request opcodes, in declaration order (wayland.xml).
        const DISPLAY_SYNC: u32 = 0;
        const DISPLAY_GET_REGISTRY: u32 = 1;
        const REGISTRY_BIND: u32 = 0;
        const COMPOSITOR_CREATE_SURFACE: u32 = 0;
        const COMPOSITOR_CREATE_REGION: u32 = 1;
        const SURFACE_DESTROY: u32 = 0;
        const SURFACE_ATTACH: u32 = 1;
        const SURFACE_COMMIT: u32 = 6;
        const REGION_DESTROY: u32 = 0;
        const SHM_CREATE_POOL: u32 = 0;
        const POOL_CREATE_BUFFER: u32 = 0;
        const POOL_DESTROY: u32 = 1;
        const BUFFER_DESTROY: u32 = 0;
        // Request opcodes (wlr-layer-shell-unstable-v1.xml).
        const LAYER_SHELL_GET_LAYER_SURFACE: u32 = 0;
        const LAYER_SURFACE_ACK_CONFIGURE: u32 = 6;
        const LAYER_SURFACE_DESTROY: u32 = 7;
        // Event opcodes the fake compositor sends.
        const REGISTRY_GLOBAL: u32 = 0;
        const CALLBACK_DONE: u32 = 0;
        const SHM_FORMAT: u32 = 0;
        const BUFFER_RELEASE: u32 = 0;
        const LAYER_SURFACE_CONFIGURE: u32 = 0;
        const LAYER_SURFACE_CLOSED: u32 = 1;

        /// One wire-level observation of what the orb's client did.
        #[derive(Clone, Debug, PartialEq)]
        pub enum Recorded {
            Bind { interface: String, id: u32 },
            CreateSurface { id: u32 },
            CreateRegion { id: u32 },
            GetLayerSurface { surface: u32, id: u32 },
            AckConfigure { id: u32 },
            CreatePool { id: u32, size: i32 },
            CreateBuffer { pool: u32, id: u32 },
            Attach { surface: u32, buffer: u32 },
            Commit { surface: u32 },
            DestroySurface { id: u32 },
            DestroyRegion { id: u32 },
            DestroyLayerSurface { id: u32 },
            DestroyBuffer { id: u32 },
            DestroyPool { id: u32 },
        }

        /// What the test wants the compositor to do to a live orb session.
        pub enum Cmd {
            /// Send `wl_buffer.release` for one buffer, handing it back.
            Release(u32),
            /// Send `zwlr_layer_surface_v1.closed` for one layer surface.
            Closed(u32),
        }

        /// The kind of every object id the client creates, so destroy requests
        /// can be recorded against the right interface.
        #[derive(Clone, Copy, PartialEq)]
        enum Kind {
            Registry,
            Compositor,
            Shm,
            Pool,
            Buffer,
            Surface,
            Region,
            LayerShell,
            LayerSurface,
        }

        fn put_u32(args: &mut Vec<u8>, value: u32) {
            args.extend_from_slice(&value.to_ne_bytes());
        }

        /// A wayland wire string: length including the NUL, bytes, NUL,
        /// padded to a multiple of four.
        fn put_str(args: &mut Vec<u8>, text: &str) {
            put_u32(args, text.len() as u32 + 1);
            args.extend_from_slice(text.as_bytes());
            args.push(0);
            while !args.len().is_multiple_of(4) {
                args.push(0);
            }
        }

        /// One wire message: object id, then size-including-header and opcode
        /// packed into one word, then the arguments.
        fn message(object: u32, opcode: u32, args: &[u8]) -> Vec<u8> {
            let mut out = Vec::with_capacity(8 + args.len());
            put_u32(&mut out, object);
            put_u32(
                &mut out,
                ((args.len() as u32 + 8) << 16) | (opcode & 0xffff),
            );
            out.extend_from_slice(args);
            out
        }

        /// Sequential argument cursor over one message's payload.
        struct Args<'a> {
            bytes: &'a [u8],
            at: usize,
        }

        impl<'a> Args<'a> {
            fn u32(&mut self) -> u32 {
                let value =
                    u32::from_ne_bytes(self.bytes[self.at..self.at + 4].try_into().unwrap());
                self.at += 4;
                value
            }

            fn i32(&mut self) -> i32 {
                self.u32() as i32
            }

            fn string(&mut self) -> String {
                let len = self.u32() as usize;
                let text =
                    String::from_utf8_lossy(&self.bytes[self.at..self.at + len - 1]).into_owned();
                self.at += len;
                while !self.at.is_multiple_of(4) {
                    self.at += 1;
                }
                text
            }
        }

        type Log = Arc<Mutex<Vec<Recorded>>>;

        fn record(log: &Log, entry: Recorded) {
            log.lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(entry);
        }

        /// The globals a dictation needs. The versions are inside the ranges
        /// `run()` binds: compositor 1..=6, shm 1..=2, layer shell 1..=5.
        const GLOBALS: [(&str, u32); 3] = [
            ("wl_compositor", 4),
            ("wl_shm", 1),
            ("zwlr_layer_shell_v1", 4),
        ];

        /// Serve one orb client until it disconnects or the test says stop.
        fn serve(mut stream: UnixStream, log: Log, cmds: Receiver<Cmd>, stop: Receiver<()>) {
            let mut kinds: HashMap<u32, Kind> = HashMap::new();
            let mut registry = 0u32;
            let mut globals_sent = false;
            let mut next_global_name = 1u32;
            let mut pending: Vec<u8> = Vec::with_capacity(4096);
            // A short timeout keeps the command channel responsive while
            // never blocking the orb's roundtrips for long.
            stream.set_read_timeout(Some(Duration::from_millis(5))).ok();
            loop {
                if stop.try_recv().is_ok() {
                    return;
                }
                while let Ok(cmd) = cmds.try_recv() {
                    let event = match cmd {
                        Cmd::Release(buffer) => message(buffer, BUFFER_RELEASE, &[]),
                        Cmd::Closed(layer_surface) => {
                            message(layer_surface, LAYER_SURFACE_CLOSED, &[])
                        }
                    };
                    if stream.write_all(&event).is_err() {
                        return;
                    }
                }
                let mut chunk = [0u8; 4096];
                match stream.read(&mut chunk) {
                    // The memfd in `wl_shm.create_pool` rides in the ancillary
                    // buffer; a plain read discards it (the kernel closes the
                    // fd), which is fine - the pixels are never inspected.
                    Ok(0) => return,
                    Ok(read) => pending.extend_from_slice(&chunk[..read]),
                    Err(ref e)
                        if e.kind() == std::io::ErrorKind::WouldBlock
                            || e.kind() == std::io::ErrorKind::TimedOut => {}
                    Err(_) => return,
                }
                while pending.len() >= 8 {
                    let object = u32::from_ne_bytes(pending[0..4].try_into().unwrap());
                    let word = u32::from_ne_bytes(pending[4..8].try_into().unwrap());
                    let size = (word >> 16) as usize;
                    let opcode = word & 0xffff;
                    if size < 8 || pending.len() < size {
                        break;
                    }
                    // The message is taken out of the buffer before parsing,
                    // so the payload never borrows `pending` across the drain.
                    let mut frame: Vec<u8> = pending.drain(..size).collect();
                    let payload = frame.split_off(8);
                    let mut args = Args {
                        bytes: &payload,
                        at: 0,
                    };

                    if object == 1 {
                        // wl_display: the sync/done pair is the roundtrip every
                        // loop iteration relies on, and the first one is
                        // `registry_queue_init`'s - the globals must land
                        // before that done or the client binds nothing.
                        if opcode == DISPLAY_SYNC {
                            let callback = args.u32();
                            if !globals_sent {
                                globals_sent = true;
                                for (interface, version) in GLOBALS {
                                    let mut ev = Vec::new();
                                    put_u32(&mut ev, next_global_name);
                                    put_str(&mut ev, interface);
                                    put_u32(&mut ev, version);
                                    let _ =
                                        stream.write_all(&message(registry, REGISTRY_GLOBAL, &ev));
                                    next_global_name += 1;
                                }
                            }
                            let _ = stream.write_all(&message(
                                callback,
                                CALLBACK_DONE,
                                &0u32.to_ne_bytes(),
                            ));
                        } else if opcode == DISPLAY_GET_REGISTRY {
                            registry = args.u32();
                            kinds.insert(registry, Kind::Registry);
                        }
                        continue;
                    }

                    match kinds.get(&object) {
                        Some(Kind::Registry) if opcode == REGISTRY_BIND => {
                            let _name = args.u32();
                            let interface = args.string();
                            let _version = args.u32();
                            let id = args.u32();
                            let kind = match interface.as_str() {
                                "wl_compositor" => Some(Kind::Compositor),
                                "wl_shm" => Some(Kind::Shm),
                                "zwlr_layer_shell_v1" => Some(Kind::LayerShell),
                                _ => None,
                            };
                            if let Some(kind) = kind {
                                kinds.insert(id, kind);
                            }
                            if interface == "wl_shm" {
                                // argb8888, the only format make_buffer asks for.
                                let _ =
                                    stream.write_all(&message(id, SHM_FORMAT, &0u32.to_ne_bytes()));
                            }
                            record(&log, Recorded::Bind { interface, id });
                        }
                        Some(Kind::Compositor) if opcode == COMPOSITOR_CREATE_SURFACE => {
                            let id = args.u32();
                            kinds.insert(id, Kind::Surface);
                            record(&log, Recorded::CreateSurface { id });
                        }
                        Some(Kind::Compositor) if opcode == COMPOSITOR_CREATE_REGION => {
                            let id = args.u32();
                            kinds.insert(id, Kind::Region);
                            record(&log, Recorded::CreateRegion { id });
                        }
                        Some(Kind::Surface) if opcode == SURFACE_ATTACH => {
                            let buffer = args.u32();
                            record(
                                &log,
                                Recorded::Attach {
                                    surface: object,
                                    buffer,
                                },
                            );
                        }
                        Some(Kind::Surface) if opcode == SURFACE_COMMIT => {
                            record(&log, Recorded::Commit { surface: object });
                        }
                        Some(Kind::Surface) if opcode == SURFACE_DESTROY => {
                            record(&log, Recorded::DestroySurface { id: object });
                        }
                        Some(Kind::Region) if opcode == REGION_DESTROY => {
                            record(&log, Recorded::DestroyRegion { id: object });
                        }
                        Some(Kind::Shm) if opcode == SHM_CREATE_POOL => {
                            let id = args.u32();
                            let size = args.i32();
                            kinds.insert(id, Kind::Pool);
                            record(&log, Recorded::CreatePool { id, size });
                        }
                        Some(Kind::Pool) if opcode == POOL_CREATE_BUFFER => {
                            let id = args.u32();
                            let _offset = args.i32();
                            let _width = args.i32();
                            let _height = args.i32();
                            let _stride = args.i32();
                            let _format = args.u32();
                            kinds.insert(id, Kind::Buffer);
                            record(&log, Recorded::CreateBuffer { pool: object, id });
                        }
                        Some(Kind::Pool) if opcode == POOL_DESTROY => {
                            record(&log, Recorded::DestroyPool { id: object });
                        }
                        Some(Kind::Buffer) if opcode == BUFFER_DESTROY => {
                            record(&log, Recorded::DestroyBuffer { id: object });
                        }
                        Some(Kind::LayerShell) if opcode == LAYER_SHELL_GET_LAYER_SURFACE => {
                            let id = args.u32();
                            let surface = args.u32();
                            let _output = args.u32();
                            let _layer = args.u32();
                            let _namespace = args.string();
                            kinds.insert(id, Kind::LayerSurface);
                            record(&log, Recorded::GetLayerSurface { surface, id });
                            // The configure the handshake needs, sent at once:
                            // the orb paints on the first tick after its ack.
                            let mut ev = Vec::new();
                            put_u32(&mut ev, 1); // serial
                            put_u32(&mut ev, ORB_WIDTH);
                            put_u32(&mut ev, ORB_HEIGHT);
                            let _ = stream.write_all(&message(id, LAYER_SURFACE_CONFIGURE, &ev));
                        }
                        Some(Kind::LayerSurface) if opcode == LAYER_SURFACE_ACK_CONFIGURE => {
                            let _serial = args.u32();
                            record(&log, Recorded::AckConfigure { id: object });
                        }
                        Some(Kind::LayerSurface) if opcode == LAYER_SURFACE_DESTROY => {
                            record(&log, Recorded::DestroyLayerSurface { id: object });
                        }
                        // damage, set_input_region, the layer surface's set_*
                        // requests: nothing the tests assert on.
                        _ => {}
                    }
                }
            }
        }

        /// The buffer ids the orb attached, in paint order - the wire-level
        /// view of "which pixels did it write".
        pub fn paints(log: &[Recorded]) -> Vec<u32> {
            log.iter()
                .filter_map(|r| match r {
                    Recorded::Attach { buffer, .. } => Some(*buffer),
                    _ => None,
                })
                .collect()
        }

        pub fn commits(log: &[Recorded]) -> usize {
            log.iter()
                .filter(|r| matches!(r, Recorded::Commit { .. }))
                .count()
        }

        pub fn buffers(log: &[Recorded]) -> Vec<u32> {
            log.iter()
                .filter_map(|r| match r {
                    Recorded::CreateBuffer { id, .. } => Some(*id),
                    _ => None,
                })
                .collect()
        }

        pub fn pools(log: &[Recorded]) -> Vec<u32> {
            log.iter()
                .filter_map(|r| match r {
                    Recorded::CreatePool { id, .. } => Some(*id),
                    _ => None,
                })
                .collect()
        }

        pub fn position_of(log: &[Recorded], entry: &Recorded) -> Option<usize> {
            log.iter().position(|r| r == entry)
        }

        /// Whether the orb bound the three globals a dictation needs. The
        /// binds sit in the client's buffer until the first command flush,
        /// so this is only meaningful after a Show woke the loop.
        pub fn bound_globals(log: &[Recorded]) -> bool {
            ["wl_compositor", "wl_shm", "zwlr_layer_shell_v1"]
                .iter()
                .all(|iface| {
                    log.iter().any(|r| {
                        matches!(
                            r,
                            Recorded::Bind { interface, .. } if interface == iface
                        )
                    })
                })
        }

        /// One orb session against the scripted compositor.
        pub struct Fake {
            socket: std::path::PathBuf,
            log: Log,
            cmds: Sender<Cmd>,
            stop: Sender<()>,
            server: Option<std::thread::JoinHandle<()>>,
            tx: Option<Sender<OrbCommand>>,
            orb: Option<std::thread::JoinHandle<Result<(), String>>>,
            /// How the run loop ended, if it ended on its own - printed by
            /// `wait_for` on timeout, because an orb that failed to connect
            /// otherwise looks identical to one that connected and went
            /// silent.
            outcome: Arc<Mutex<Option<String>>>,
            saved_display: Option<std::ffi::OsString>,
            // Both orb tests set WAYLAND_DISPLAY, a process-global variable:
            // this guard serializes them so they cannot race each other.
            _env_guard: MutexGuard<'static, ()>,
        }

        impl Fake {
            pub fn start() -> Self {
                static NEXT: std::sync::atomic::AtomicUsize =
                    std::sync::atomic::AtomicUsize::new(0);
                static ENV_GUARD: Mutex<()> = Mutex::new(());
                let _env_guard = ENV_GUARD.lock().unwrap_or_else(|p| p.into_inner());

                let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let socket = std::env::temp_dir()
                    .join(format!("mnvoice-orb-wire-{}-{n}.sock", std::process::id()));
                // A stale file from a crashed run would fail the bind.
                let _ = std::fs::remove_file(&socket);
                let listener =
                    UnixListener::bind(&socket).expect("bind the fake compositor socket");
                let log: Log = Arc::new(Mutex::new(Vec::new()));
                let (cmd_tx, cmd_rx) = std::sync::mpsc::channel::<Cmd>();
                let (stop_tx, stop_rx) = std::sync::mpsc::channel::<()>();
                let (accepted_tx, accepted_rx) = std::sync::mpsc::channel::<()>();
                let server_log = Arc::clone(&log);
                let server = std::thread::Builder::new()
                    .name("fake-compositor".into())
                    .spawn(move || {
                        let (stream, _) = listener.accept().expect("the orb connects");
                        // The accept proves the connect, and the connect proves
                        // the env var was read - only after that may the test
                        // restore it.
                        let _ = accepted_tx.send(());
                        serve(stream, server_log, cmd_rx, stop_rx);
                    })
                    .expect("spawn the fake compositor");

                let saved_display = std::env::var_os("WAYLAND_DISPLAY");
                std::env::set_var("WAYLAND_DISPLAY", &socket);
                let (tx, rx) = std::sync::mpsc::channel::<OrbCommand>();
                let outcome: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
                let orb_outcome = Arc::clone(&outcome);
                let orb = std::thread::Builder::new()
                    .name("orb-under-test".into())
                    .spawn(move || {
                        let result = run(rx, (0.2, 0.9, 0.6), 0.75);
                        if let Err(e) = &result {
                            *orb_outcome.lock().unwrap_or_else(|p| p.into_inner()) =
                                Some(e.clone());
                        }
                        result
                    })
                    .expect("spawn the orb run loop");

                let fake = Fake {
                    socket,
                    log,
                    cmds: cmd_tx,
                    stop: stop_tx,
                    server: Some(server),
                    tx: Some(tx),
                    orb: Some(orb),
                    outcome,
                    saved_display,
                    _env_guard,
                };

                // The connect happens before the first flush: `globals.bind`
                // only buffers the bind requests, and they reach the wire at
                // the first roundtrip - which only runs once a command wakes
                // the loop. So the harness waits for the connection, not the
                // binds; the binds are folded into the tests' first
                // post-Show wait instead.
                accepted_rx
                    .recv_timeout(Duration::from_secs(5))
                    .expect("the orb never connected to the fake compositor");
                match &fake.saved_display {
                    Some(value) => std::env::set_var("WAYLAND_DISPLAY", value),
                    None => std::env::remove_var("WAYLAND_DISPLAY"),
                }
                fake
            }

            pub fn show(&self, state: OrbState) {
                self.tx
                    .as_ref()
                    .expect("the orb channel is open")
                    .send(OrbCommand::Show(state))
                    .expect("the orb thread is running");
            }

            pub fn hide(&self) {
                self.tx
                    .as_ref()
                    .expect("the orb channel is open")
                    .send(OrbCommand::Hide)
                    .expect("the orb thread is running");
            }

            pub fn release(&self, buffer: u32) {
                self.cmds.send(Cmd::Release(buffer)).expect("compositor up");
            }

            pub fn closed(&self, layer_surface: u32) {
                self.cmds
                    .send(Cmd::Closed(layer_surface))
                    .expect("compositor up");
            }

            pub fn snapshot(&self) -> Vec<Recorded> {
                self.log.lock().unwrap_or_else(|p| p.into_inner()).clone()
            }

            /// Poll the wire log until `predicate` holds, and hand back the
            /// snapshot it held for. A timeout fails with the whole log, so a
            /// wire-level mismatch shows itself instead of a bare hang.
            pub fn wait_for(
                &self,
                timeout: Duration,
                predicate: impl Fn(&[Recorded]) -> bool,
            ) -> Vec<Recorded> {
                let started = std::time::Instant::now();
                loop {
                    let snapshot = self.snapshot();
                    if predicate(&snapshot) {
                        return snapshot;
                    }
                    assert!(
                        started.elapsed() < timeout,
                        "the fake compositor never saw the expected traffic within \
                         {timeout:?}; wire log so far:\n{:#?}\norb outcome: {:?}",
                        snapshot,
                        self.outcome.lock().unwrap_or_else(|p| p.into_inner())
                    );
                    std::thread::sleep(Duration::from_millis(5));
                }
            }

            /// End the dictation channel, let the run loop observe the
            /// disconnect and exit, then take the compositor down with it.
            pub fn shutdown(mut self) {
                drop(self.tx.take());
                let orb = self.orb.take().expect("the orb thread is joinable");
                std::thread::sleep(Duration::from_millis(150));
                let _ = self.stop.send(());
                if let Some(server) = self.server.take() {
                    let _ = server.join();
                }
                let outcome = orb.join().expect("the orb thread must not panic");
                assert!(
                    outcome.is_ok(),
                    "the orb run loop must exit cleanly, not with {outcome:?}"
                );
                let _ = std::fs::remove_file(&self.socket);
            }
        }
    }

    use fake_compositor::{commits, paints, Fake, Recorded};

    /// The paint gate: while the compositor holds both buffers the orb must
    /// skip frames rather than write into pixels that may still be read, and
    /// a release must resume painting into exactly the buffer it freed.
    ///
    /// A real compositor cannot be pinned in the adversarial state (it
    /// releases within a frame or two), which is why the peer here is
    /// scripted: the test decides when a `wl_buffer.release` exists at all.
    #[test]
    fn the_orb_paints_only_into_buffers_the_compositor_has_released() {
        let wire = Fake::start();
        wire.show(OrbState::Recording);

        // The first Show flushes the whole startup: the three binds, the
        // surface, the configure handshake, and then the paints. (Commits
        // are 3 at this point: the empty one that asks for the configure,
        // then one per paint.)
        let two = wire.wait_for(Duration::from_secs(3), |log| {
            fake_compositor::bound_globals(log) && paints(log).len() >= 2 && commits(log) >= 3
        });
        assert_eq!(paints(&two).len(), 2, "exactly two paints: {two:#?}");
        let (first, second) = (paints(&two)[0], paints(&two)[1]);
        assert_ne!(first, second, "the two slots are distinct buffers");

        // ~7 frame ticks pass here. With the gate, no paint can happen: both
        // buffers are held and neither was released. Pre-gate behaviour
        // attached one of them every tick.
        std::thread::sleep(Duration::from_millis(250));
        let gate = wire.snapshot();
        assert_eq!(
            paints(&gate).len(),
            2,
            "while the compositor holds both buffers the orb must skip frames, \
             not keep painting: {gate:#?}"
        );
        assert_eq!(commits(&gate), 3, "a skipped frame commits nothing");

        // Releasing the first buffer must resume painting into exactly that
        // buffer, exactly once: the freed slot is painted, marked in use
        // again, and the gate closes until the next release.
        wire.release(first);
        let resumed = wire.wait_for(Duration::from_secs(1), |log| paints(log).len() == 3);
        assert_eq!(
            paints(&resumed)[2],
            first,
            "the resumed frame must go to the released buffer: {resumed:#?}"
        );
        assert_eq!(
            commits(&resumed),
            4,
            "one release buys exactly one paint, not a flood"
        );
        assert_eq!(
            paints(&resumed).iter().filter(|b| **b == second).count(),
            1,
            "the still-held buffer must not be painted into: {resumed:#?}"
        );

        // Releasing the other resumes the alternation into it.
        wire.release(second);
        let resumed_again = wire.wait_for(Duration::from_secs(1), |log| paints(log).len() == 4);
        assert_eq!(
            paints(&resumed_again)[3],
            second,
            "the next paint must go to the newly released buffer: {resumed_again:#?}"
        );
        assert_eq!(commits(&resumed_again), 5);

        // Each buffer got its own shm pool (a fresh memfd per buffer, not a
        // shared one), so two pools exist for two paints.
        let created = wire.snapshot();
        let pools = fake_compositor::pools(&created);
        assert_eq!(pools.len(), 2, "one pool per buffer: {created:#?}");
        assert_ne!(pools[0], pools[1]);

        wire.shutdown();
    }

    /// Hide must take the surface and its buffers down together - the wire
    /// requests prove the teardown, not a dropped proxy - and the surface
    /// proxies go first, in the order the Hide arm documents. The next
    /// dictation then paints into a fresh memfd pair.
    #[test]
    fn hide_takes_the_surface_and_both_buffers_with_it() {
        let wire = Fake::start();
        wire.show(OrbState::Recording);

        let painted = wire.wait_for(Duration::from_secs(3), |log| {
            fake_compositor::bound_globals(log) && !paints(log).is_empty() && commits(log) >= 2
        });
        let surface = painted
            .iter()
            .find_map(|r| match r {
                Recorded::CreateSurface { id } => Some(*id),
                _ => None,
            })
            .expect("a surface was created");
        let layer = painted
            .iter()
            .find_map(|r| match r {
                Recorded::GetLayerSurface { id, .. } => Some(*id),
                _ => None,
            })
            .expect("a layer surface was created");
        let first_buffers = fake_compositor::buffers(&painted);
        let first_pools = fake_compositor::pools(&painted);
        assert!(!first_buffers.is_empty());

        wire.hide();

        let hidden = wire.wait_for(Duration::from_secs(1), |log| {
            log.iter()
                .any(|r| matches!(r, Recorded::DestroySurface { id } if *id == surface))
                && log
                    .iter()
                    .any(|r| matches!(r, Recorded::DestroyLayerSurface { id } if *id == layer))
                && first_buffers.iter().all(|b| {
                    log.iter()
                        .any(|r| matches!(r, Recorded::DestroyBuffer { id } if *id == *b))
                })
                && first_pools.iter().all(|p| {
                    log.iter()
                        .any(|r| matches!(r, Recorded::DestroyPool { id } if *id == *p))
                })
        });
        // The belt-and-braces ordering the Hide arm promises: both surface
        // proxies are destroyed before any buffer or pool goes.
        let surface_destroys = [
            fake_compositor::position_of(&hidden, &Recorded::DestroyLayerSurface { id: layer }),
            fake_compositor::position_of(&hidden, &Recorded::DestroySurface { id: surface }),
        ];
        let first_buffer_destroy = hidden
            .iter()
            .position(|r| matches!(r, Recorded::DestroyBuffer { .. }))
            .expect("a buffer was destroyed");
        assert!(
            surface_destroys
                .iter()
                .all(|p| p.unwrap() < first_buffer_destroy),
            "surface proxies must be destroyed before the buffers: {hidden:#?}"
        );

        // The next dictation gets a fresh memfd pair: its pool ids differ
        // from the first dictation's, whatever the orb reused.
        wire.show(OrbState::Transcribing);
        let second = wire.wait_for(Duration::from_secs(3), |log| {
            fake_compositor::pools(log).len() > first_pools.len() && paints(log).len() >= 2
        });
        let second_pools = fake_compositor::pools(&second);
        assert!(
            second_pools.iter().any(|p| !first_pools.contains(p)),
            "the second dictation must paint into fresh memfds, not the dead \
             pools: {second:#?}"
        );

        wire.shutdown();
    }

    /// The compositor's `closed` event must destroy the surface and the
    /// buffers on the wire - a dropped proxy is never destroyed, so without
    /// the destroy requests the object ids would stay in the client's map
    /// and the compositor would keep the surface for the life of the process.
    #[test]
    fn the_compositors_closed_event_takes_the_surface_and_buffers_with_it() {
        let wire = Fake::start();
        wire.show(OrbState::Recording);

        let painted = wire.wait_for(Duration::from_secs(3), |log| {
            fake_compositor::bound_globals(log) && !paints(log).is_empty() && commits(log) >= 2
        });
        let surface = painted
            .iter()
            .find_map(|r| match r {
                Recorded::CreateSurface { id } => Some(*id),
                _ => None,
            })
            .expect("a surface was created");
        let layer = painted
            .iter()
            .find_map(|r| match r {
                Recorded::GetLayerSurface { id, .. } => Some(*id),
                _ => None,
            })
            .expect("a layer surface was created");
        let painted_buffers = fake_compositor::buffers(&painted);
        let painted_pools = fake_compositor::pools(&painted);

        wire.closed(layer);

        let closed = wire.wait_for(Duration::from_secs(1), |log| {
            log.iter()
                .any(|r| matches!(r, Recorded::DestroySurface { id } if *id == surface))
                && log
                    .iter()
                    .any(|r| matches!(r, Recorded::DestroyLayerSurface { id } if *id == layer))
                && painted_buffers.iter().all(|b| {
                    log.iter()
                        .any(|r| matches!(r, Recorded::DestroyBuffer { id } if *id == *b))
                })
                && painted_pools.iter().all(|p| {
                    log.iter()
                        .any(|r| matches!(r, Recorded::DestroyPool { id } if *id == *p))
                })
        });
        // The same order as the Hide arm: surface proxies first, then the
        // buffer pair.
        let surface_destroys = [
            fake_compositor::position_of(&closed, &Recorded::DestroyLayerSurface { id: layer }),
            fake_compositor::position_of(&closed, &Recorded::DestroySurface { id: surface }),
        ];
        let first_buffer_destroy = closed
            .iter()
            .position(|r| matches!(r, Recorded::DestroyBuffer { .. }))
            .expect("a buffer was destroyed");
        assert!(
            surface_destroys
                .iter()
                .all(|p| p.unwrap() < first_buffer_destroy),
            "surface proxies must be destroyed before the buffers: {closed:#?}"
        );

        wire.shutdown();
    }
}
