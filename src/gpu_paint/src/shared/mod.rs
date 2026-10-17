//! Importing CEF's shared-texture frames, and the device selection that makes
//! importing possible at all.
//!
//! One implementation per platform behind one interface: a dmabuf on Linux, a
//! shared NT handle on Windows, an `IOSurface` on macOS. What the platforms
//! share is the shape — name the device CEF produces on, open a device that can
//! take its buffers, and turn one frame into a `wgpu::Texture`.

#[cfg(target_os = "linux")]
#[path = "vulkan.rs"]
mod backend;

#[cfg(windows)]
#[path = "dx12.rs"]
mod backend;

#[cfg(target_os = "macos")]
#[path = "metal.rs"]
mod backend;

pub use backend::ProducerId;
pub(crate) use backend::{Importer, acquire_barrier, adapter_matches, open_device};
#[cfg(windows)]
pub(crate) use backend::{adapter_luid, poll_device_removed_reason};
#[cfg(windows)]
pub use backend::{format_luid_switch, is_device_removed_hresult, unpack_luid};

#[cfg(not(windows))]
pub(crate) fn poll_device_removed_reason(_device: &wgpu::Device) -> Option<String> {
    None
}

/// A shared-texture import that did not work.
///
/// A per-frame import failure (`device_lost: None`) is not a [`crate::SurfaceLost`]:
/// a shared frame has no CPU pixels, so there is nothing to fall back to. When
/// the underlying D3D12 device itself has entered the removed state
/// (`device_lost: Some(_)`), the caller escalates to [`crate::SurfaceLost`] so
/// coordinated device-loss recovery runs instead of silently dropping frames.
#[derive(Debug, thiserror::Error)]
#[error("shared-texture import failed: {reason}")]
pub(crate) struct ImportFailed {
    pub(crate) reason: &'static str,
    pub(crate) device_lost: Option<String>,
}

impl ImportFailed {
    #[cfg(windows)]
    pub(crate) fn device_lost(reason: &'static str, detail: impl Into<String>) -> Self {
        Self {
            reason,
            device_lost: Some(detail.into()),
        }
    }
}

#[allow(non_snake_case)]
pub(crate) const fn ImportFailed(reason: &'static str) -> ImportFailed {
    ImportFailed {
        reason,
        device_lost: None,
    }
}

/// One imported frame, ready to sample.
pub(crate) struct Imported {
    pub(crate) texture: wgpu::Texture,
    /// The raw image handle [`acquire_barrier`] needs, where the platform has
    /// a queue-family transfer to do. `None` where it does not.
    pub(crate) acquire: Option<u64>,
    /// `texture` is the importer's cached wrapper from an earlier frame, so
    /// anything the painter built over it (its bind group) is reusable too.
    pub(crate) reused: bool,
}

/// A device opened for painting, plus whether it is capable of importing at
/// all — the consumer half of the shared-texture question, before it is ANDed
/// with whether this is the adapter CEF produces on.
pub(crate) struct Opened {
    pub(crate) device: wgpu::Device,
    pub(crate) queue: wgpu::Queue,
    pub(crate) import_capable: bool,
}
