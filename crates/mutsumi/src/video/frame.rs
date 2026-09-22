//! Platform-neutral plumbing between whoever produces video frames and the
//! paintable that displays them.
//!
//! On Linux frames come from the Wayland proxy in
//! [`crate::video::mpv::proxy`], on Windows they come from the software
//! renderer in [`crate::video::mpv::winrender`]. Both feed the same
//! [`FRAME_CHANNEL`], and [`crate::MutsumiVideoSink`] does not need to know
//! which platform it is running on.

use flume::{
    Receiver,
    Sender,
    unbounded,
};
use once_cell::sync::Lazy;
use tokio::sync::watch;

#[cfg(target_os = "linux")]
pub use crate::video::mpv::proxy::FrameCallbacks;

/// Frames on this platform do not belong to a compositor, so there is nothing
/// to acknowledge. The handle exists to keep the shared code path uniform.
#[cfg(not(target_os = "linux"))]
#[derive(Debug)]
pub struct FrameCallbacks;

#[cfg(not(target_os = "linux"))]
impl FrameCallbacks {
    pub fn done(self, _time_ms: u32) {}
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShmMemoryFormat {
    Argb8888,
    Xrgb8888,
}

pub struct ShmFrame {
    pub width: i32,
    pub height: i32,
    pub stride: usize,
    pub format: ShmMemoryFormat,
    pub data: Vec<u8>,
}

pub enum SurfaceContentUpdate {
    Unchanged,
    #[cfg(target_os = "linux")]
    Frame(crate::video::mpv::proxy::DmabufFrame),
    Shm(ShmFrame),
    Clear,
}

pub struct SurfaceUpdate {
    pub content: SurfaceContentUpdate,
    pub frame_callbacks: Option<FrameCallbacks>,
}

pub static FRAME_CHANNEL: Lazy<DmabufFrameChannel> = Lazy::new(|| {
    let (tx, rx) = flume::unbounded::<SurfaceUpdate>();
    DmabufFrameChannel { tx, rx }
});

pub struct DmabufFrameChannel {
    pub tx: Sender<SurfaceUpdate>,
    pub rx: Receiver<SurfaceUpdate>,
}

pub static VIEWPORT_CHANNEL: Lazy<ViewportChannel> = Lazy::new(|| {
    let (tx, _) = watch::channel(None);
    ViewportChannel { tx }
});

#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct Viewport {
    pub width: i32,
    pub height: i32,
    pub scale: f64,
}

impl Viewport {
    pub fn new(width: i32, height: i32, scale: f64) -> Self {
        Self {
            width,
            height,
            scale,
        }
    }
}

pub struct ViewportChannel {
    tx: watch::Sender<Option<Viewport>>,
}

impl ViewportChannel {
    pub fn send(&self, viewport: Viewport) {
        self.tx.send_replace(Some(viewport));
    }

    pub fn subscribe(&self) -> watch::Receiver<Option<Viewport>> {
        self.tx.subscribe()
    }
}
