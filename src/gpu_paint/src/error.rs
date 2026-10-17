use crate::FrameSize;
use thiserror::Error;

/// This surface can no longer present; the caller should abandon it.
///
/// Its *existence* is the whole signal — there is no severity to interrogate,
/// which is why it is opaque. Anything recoverable comes back as another
/// [`crate::PresentFailed`] arm, each naming what its producer still owes: a
/// stale, occluded or timed-out swapchain, and a shared-texture import that
/// failed.
///
/// The detail below exists for the log line, and reaches callers only through
/// `Display`.
#[derive(Debug, Error)]
#[error("{0}")]
pub struct SurfaceLost(Kind);

#[derive(Debug, Error)]
pub(crate) enum Kind {
    #[error("no usable adapter available")]
    NoAdapter,
    #[error("device request failed: {0}")]
    DeviceRequest(#[from] wgpu::RequestDeviceError),
    #[error("GPU device lost: {0}")]
    DeviceLost(String),
    #[error("surface creation failed: {0}")]
    SurfaceCreate(#[from] wgpu::CreateSurfaceError),
    #[error("adapter does not support requested surface")]
    SurfaceUnsupported,
    #[error("invalid frame dimensions: {}x{}", .0.w, .0.h)]
    BadDimensions(FrameSize),
    #[error(
        "frame buffer does not cover {}x{} at stride {stride}: {len} bytes",
        .size.w, .size.h
    )]
    BadPixelBuffer {
        size: FrameSize,
        stride: u32,
        len: usize,
    },
}

impl SurfaceLost {
    pub(crate) fn device_lost(reason: impl Into<String>) -> Self {
        Self(Kind::DeviceLost(reason.into()))
    }

    /// Whether this surface loss was caused by the underlying GPU device being
    /// lost or invalidated rather than a per-surface or per-frame parameter error.
    pub const fn is_device_lost(&self) -> bool {
        matches!(self.0, Kind::DeviceLost(_))
    }
}

impl<E: Into<Kind>> From<E> for SurfaceLost {
    fn from(e: E) -> Self {
        Self(e.into())
    }
}
