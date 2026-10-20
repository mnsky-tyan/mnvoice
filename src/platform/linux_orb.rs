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

use crate::config;
use crate::orb_render::{render_gas_fluid, OrbState, ORB_HEIGHT, ORB_WIDTH};
use std::sync::mpsc::Sender;
use std::time::Duration;
use wayland_client::protocol::{wl_compositor, wl_region, wl_shm, wl_shm_pool, wl_surface};
use wayland_client::{Connection, Dispatch, QueueHandle};
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
    buffers: [Option<Buffer>; 2],
    next: usize,
    frame: u32,
    visible: bool,
    state: OrbState,
    color: (f32, f32, f32),
    fluid_level: f32,
    configured: bool,
    running: bool,
}

struct Buffer {
    wl: wayland_client::protocol::wl_buffer::WlBuffer,
    /// The mmap'd pixels the render writes into. Kept mapped for the life of
    /// the buffer; the file backs the shm pool and is unlinked immediately.
    map: *mut u8,
    _pool: wayland_client::protocol::wl_shm_pool::WlShmPool,
}

// The mapped pixels are only ever reached through the render's `&mut [u32]`,
// one buffer at a time, on the one thread that owns the connection.
unsafe impl Send for Buffer {}

impl OrbState_ {
    /// A dictation started. Only the flags move here, and `configured` is not
    /// one of them: it belongs to the layer-shell handshake and is never
    /// revisited, because a compositor that has already configured a surface
    /// sends no second configure for an unchanged size. Clearing it on show
    /// waited for an event that never arrives, so the orb never painted a
    /// frame - `shown_after_the_handshake_can_paint` pins that.
    fn mark_visible(&mut self, state: OrbState) {
        self.state = state;
        self.visible = true;
    }

    /// A dictation ended. The surface is unmapped by the caller detaching its
    /// buffer; the flag stops the render loop spending frames on nothing.
    fn mark_hidden(&mut self) {
        self.visible = false;
    }

    /// Whether a frame may be rendered and committed. Painting needs a
    /// configured surface; showing needs a live dictation.
    fn can_paint(&self) -> bool {
        self.visible && self.configured
    }
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

    let surface = compositor.create_surface(&qh, ());
    let layer = layer_shell.get_layer_surface(
        &surface,
        None,
        zwlr_layer_shell_v1::Layer::Overlay,
        "mnvoice-orb".into(),
        &qh,
        (),
    );
    layer.set_anchor(zwlr_layer_surface_v1::Anchor::Bottom);
    layer.set_size(ORB_WIDTH, ORB_HEIGHT);
    layer.set_margin(0, 0, BOTTOM_MARGIN, 0);
    layer.set_exclusive_zone(-1);

    // Click-through: an empty input region means the compositor delivers
    // pointer events to whatever is underneath, exactly like the Windows
    // orb's WS_EX_TRANSPARENT.
    let region: wl_region::WlRegion = compositor.create_region(&qh, ());
    region.add(0, 0, 0, 0);
    surface.set_input_region(Some(&region));
    region.destroy();

    let mut st = OrbState_ {
        buffers: [None, None],
        next: 0,
        frame: 0,
        visible: false,
        state: OrbState::Recording,
        color,
        fluid_level,
        configured: false,
        running: true,
    };

    surface.commit();
    // Roundtrip so the first configure event arrives before the loop below;
    // painting into an unconfigured surface is undefined.
    queue
        .roundtrip(&mut st)
        .map_err(|e| format!("Wayland roundtrip failed ({e})"))?;

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
                // No commit, and no configure reset: see `mark_visible`. The
                // paint branch below is what re-maps the surface - attach,
                // damage, commit - which is the Wayland way to show a layer
                // surface again after a Hide detached its buffer.
            }
            Ok(OrbCommand::State(s)) => st.state = s,
            Ok(OrbCommand::Hide) => {
                st.mark_hidden();
                // Unmap by detaching: the compositor forgets the buffer and the
                // surface stops being presented, which is the Wayland way to
                // hide without destroying.
                surface.attach(None, 0, 0);
                surface.commit();
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
                st.frame += 1;
                let index = st.next;
                if st.buffers[index].is_none() {
                    st.buffers[index] = Some(make_buffer(&shm, &qh));
                }
                // The render writes into the buffer's map, and the frame
                // counter advances once per paint, so both happen through one
                // borrow instead of two.
                let (paint, state, frame, color, fluid) =
                    (index, st.state, st.frame, st.color, st.fluid_level);
                if let Some(buffer) = st.buffers[paint].as_mut() {
                    let pixels = unsafe {
                        std::slice::from_raw_parts_mut(
                            buffer.map as *mut u32,
                            (ORB_WIDTH * ORB_HEIGHT) as usize,
                        )
                    };
                    render_gas_fluid(frame, pixels, state, color, fluid);
                    surface.attach(Some(&buffer.wl), 0, 0);
                    surface.damage(0, 0, ORB_WIDTH as i32, ORB_HEIGHT as i32);
                    surface.commit();
                    st.next = (st.next + 1) % 2;
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
    let size = (ORB_WIDTH * ORB_HEIGHT * 4) as usize;
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
        _: &mut Self,
        _: &wayland_client::protocol::wl_buffer::WlBuffer,
        _: wayland_client::protocol::wl_buffer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
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
                // before a buffer is committed against it. The ack was never
                // sent, which no compositor ever caught because the orb never
                // reached a buffer commit - but the first working render
                // would have been the place to find out.
                surface.ack_configure(serial);
                state.configured = true;
            }
            zwlr_layer_surface_v1::Event::Closed => {
                state.visible = false;
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
            state: OrbState::Recording,
            color: (1.0, 0.5, 0.9),
            fluid_level: 0.75,
            configured: false,
            running: true,
        }
    }

    /// The whole handshake, in order: nothing paints before the compositor
    /// configures the surface, everything paints after.
    #[test]
    fn nothing_paints_before_the_compositor_configures() {
        let mut st = state();
        st.mark_visible(OrbState::Recording);
        assert!(!st.can_paint(), "an unconfigured surface has no size yet");
        st.configured = true; // the configure event, acked in Dispatch
        assert!(st.can_paint());
    }

    /// The bug this pins: a dictation's second Show, after a first one already
    /// completed the handshake. A compositor sends no fresh configure for an
    /// unchanged size, so if Show cleared `configured` the orb painted no frame
    /// ever again - which is exactly how the orb came to be invisible on a
    /// desktop where every other part of the app worked.
    #[test]
    fn shown_after_the_handshake_can_paint() {
        let mut st = state();
        st.configured = true;
        st.mark_visible(OrbState::Recording);
        st.mark_hidden();
        st.mark_visible(OrbState::Recording);
        assert!(
            st.can_paint(),
            "a re-shown orb must paint without waiting for a configure that never comes"
        );
    }

    /// Hide is what stops the frame loop spending CPU on an unmapped surface.
    #[test]
    fn hidden_stops_painting() {
        let mut st = state();
        st.configured = true;
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
