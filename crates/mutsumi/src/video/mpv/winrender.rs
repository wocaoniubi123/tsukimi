//! The Windows video path.
//!
//! On Linux mpv renders into a Wayland surface and the proxy hands GTK a
//! dmabuf; on Windows there is no such surface to render into. Instead mpv is
//! asked to draw into a plain CPU buffer through the software render API, and
//! that buffer is uploaded as a texture which the sink in
//! [`super::paintable`] paints like any other frame.
//!
//! Doing it this way keeps the whole GTK scene graph intact: danmaku, OSD and
//! control widgets stay ordinary widgets drawn on top of the video instead of
//! fighting a native child window for z-order.

use std::{
    ffi::c_void,
    ptr,
    sync::Arc,
    thread,
};

use flume::{
    Receiver,
    Sender,
    unbounded,
};
use libmpv2::Mpv;
use libmpv2_sys as sys;

use crate::Viewport;

/// Bytes per pixel of the `bgr0` surface mpv renders into.
const BYTES_PER_PIXEL: usize = 4;
/// mpv asks for 64 byte alignment of both the buffer pointer and the stride,
/// otherwise it silently falls back to a much slower copy path.
const ALIGNMENT: usize = 64;
/// Pixel format string handed to mpv. `bgr0` puts blue at the lowest address,
/// which is exactly what `gdk::MemoryFormat::B8g8r8a8` describes.
const SW_FORMAT: &[u8] = b"bgr0\0";

/// A finished frame on its way to the GTK main thread.
pub struct Frame {
    pub width: i32,
    pub height: i32,
    pub stride: usize,
    pub data: Vec<u8>,
}

enum Msg {
    Viewport(Viewport),
    Render,
    Shutdown,
}

/// Owns the render thread. Dropping it stops the thread and frees the mpv
/// render context.
pub struct WinRenderer {
    frames: Receiver<Frame>,
    tx: Sender<Msg>,
}

impl WinRenderer {
    pub fn new(mpv: Arc<Mpv>) -> Self {
        let (frame_tx, frames) = unbounded::<Frame>();
        let (tx, rx) = unbounded::<Msg>();

        let wake = tx.clone();
        thread::Builder::new()
            .name("mpv-sw-render".into())
            .spawn(move || {
                if let Err(error) = render_loop(&mpv, &rx, &frame_tx, wake) {
                    tracing::error!(target: "mutsumi::mpv", %error, "software renderer stopped");
                }
            })
            .expect("failed to spawn the mpv software render thread");

        Self { frames, tx }
    }

    /// Frames rendered so far, oldest first.
    pub fn frames(&self) -> &Receiver<Frame> {
        &self.frames
    }

    /// Tell the renderer how large the video should be drawn. Called whenever
    /// the player widget is allocated.
    pub fn set_viewport(&self, viewport: Viewport) {
        let _ = self.tx.send(Msg::Viewport(viewport));
    }
}

impl Drop for WinRenderer {
    fn drop(&mut self) {
        let _ = self.tx.send(Msg::Shutdown);
    }
}

struct SoftwareRenderer {
    ctx: *mut sys::mpv_render_context,
    viewport: Viewport,
}

impl SoftwareRenderer {
    fn render(&mut self, frames: &Sender<Frame>) {
        let width = (self.viewport.width as f64 * self.viewport.scale).round() as i32;
        let height = (self.viewport.height as f64 * self.viewport.scale).round() as i32;
        if width <= 0 || height <= 0 {
            return;
        }

        let stride = (width as usize * BYTES_PER_PIXEL).next_multiple_of(ALIGNMENT);
        let mut data = vec![0u8; stride * height as usize];

        let mut size = [width, height];
        let mut stride_param = stride;
        let mut format = SW_FORMAT.as_ptr() as *mut std::os::raw::c_char;
        // mpv renders at its own pace and blocks in render() until the frame is
        // due, which keeps playback timing correct without a timer of our own.
        let mut block_for_target_time: i32 = 1;

        let mut params = [
            sys::mpv_render_param {
                type_: sys::mpv_render_param_type_MPV_RENDER_PARAM_SW_SIZE,
                data: size.as_mut_ptr() as *mut c_void,
            },
            sys::mpv_render_param {
                type_: sys::mpv_render_param_type_MPV_RENDER_PARAM_SW_FORMAT,
                data: format as *mut c_void,
            },
            sys::mpv_render_param {
                type_: sys::mpv_render_param_type_MPV_RENDER_PARAM_SW_STRIDE,
                data: &mut stride_param as *mut usize as *mut c_void,
            },
            sys::mpv_render_param {
                type_: sys::mpv_render_param_type_MPV_RENDER_PARAM_SW_POINTER,
                data: data.as_mut_ptr() as *mut c_void,
            },
            sys::mpv_render_param {
                type_: sys::mpv_render_param_type_MPV_RENDER_PARAM_BLOCK_FOR_TARGET_TIME,
                data: &mut block_for_target_time as *mut i32 as *mut c_void,
            },
            sys::mpv_render_param {
                type_: sys::mpv_render_param_type_MPV_RENDER_PARAM_INVALID,
                data: ptr::null_mut(),
            },
        ];

        // SAFETY: the context was created by this thread and outlives the call;
        // `data` stays alive and correctly sized until render() returns.
        let code = unsafe { sys::mpv_render_context_render(self.ctx, params.as_mut_ptr()) };
        if code < 0 {
            tracing::warn!(target: "mutsumi::mpv", code, "software frame render failed");
            return;
        }

        // mpv leaves the fourth byte of every pixel undefined. The texture is
        // premultiplied, so undefined alpha would show up as transparency.
        for pixel in data.chunks_exact_mut(BYTES_PER_PIXEL) {
            pixel[3] = 0xff;
        }

        let _ = frames.send(Frame {
            width,
            height,
            stride,
            data,
        });
    }
}

extern "C" fn update_callback(ctx: *mut c_void) {
    // SAFETY: `ctx` is the leaked sender installed below, which stays alive
    // until the render context is freed in the same thread.
    let wake = unsafe { &*(ctx as *const Sender<Msg>) };
    let _ = wake.send(Msg::Render);
}

fn render_loop(
    mpv: &Arc<Mpv>, rx: &Receiver<Msg>, frames: &Sender<Frame>, wake: Sender<Msg>,
) -> Result<(), String> {
    let api_type = sys::MPV_RENDER_API_TYPE_SW.as_ptr() as *mut c_void;
    let mut params = [
        sys::mpv_render_param {
            type_: sys::mpv_render_param_type_MPV_RENDER_PARAM_API_TYPE,
            data: api_type,
        },
        sys::mpv_render_param {
            type_: sys::mpv_render_param_type_MPV_RENDER_PARAM_INVALID,
            data: ptr::null_mut(),
        },
    ];

    let mut ctx: *mut sys::mpv_render_context = ptr::null_mut();
    // SAFETY: `mpv` outlives the render context because the caller keeps an Arc
    // alive until the thread has exited.
    let code = unsafe { sys::mpv_render_context_create(&mut ctx, mpv.ctx.as_ptr(), params.as_mut_ptr()) };
    if code < 0 {
        return Err(format!("mpv_render_context_create failed with {code}"));
    }

    let callback_ctx = Box::into_raw(Box::new(wake)) as *mut c_void;
    // SAFETY: the callback only sends on a channel, and the boxed sender is
    // kept alive until the matching from_raw below.
    unsafe { sys::mpv_render_context_set_update_callback(ctx, Some(update_callback), callback_ctx) };

    let mut renderer = SoftwareRenderer {
        ctx,
        viewport: Viewport::default(),
    };

    for msg in rx.iter() {
        match msg {
            Msg::Viewport(viewport) => renderer.viewport = viewport,
            Msg::Render => renderer.render(frames),
            Msg::Shutdown => break,
        }
    }

    // SAFETY: no more renders happen after the loop, and the callback is
    // removed before its context is dropped.
    unsafe {
        sys::mpv_render_context_set_update_callback(ctx, None, ptr::null_mut());
        sys::mpv_render_context_free(ctx);
        drop(Box::from_raw(callback_ctx as *mut Sender<Msg>));
    }

    Ok(())
}
