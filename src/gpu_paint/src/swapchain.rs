//! A swapchain the caller draws into itself.
//!
//! [`crate::Surface`] owns both the swapchain and what goes on it: hand it a
//! frame and it encodes the draw. A [`Swapchain`] owns only the swapchain, and
//! hands out the texture view so another renderer — iced, on the shell overlay
//! — can encode its own work against the process's one device. The caller's
//! encoding runs inside the submit gate; the acquire and the present do not.
//!
//! The policies are not re-decided here. Format, present mode and
//! composite-alpha come from the same per-[`crate::WindowTarget`] rules
//! [`crate::Surface`] uses, so two swapchains on the same window system are
//! configured identically.

use std::sync::Arc;

use crate::FrameSize;
use crate::context::{DeviceGeneration, FORMAT, Surfaces};
use crate::error::{Kind, SurfaceLost};
use crate::painter::{ConfigureSite, PresentPolicy, create_surface, pick_alpha_mode, texels};
use crate::types::{Deferred, Presented, WindowTarget};

/// A swapchain on one window, presented to by its caller.
pub struct Swapchain<'ctx> {
    ctx: &'ctx Surfaces,
    device: Arc<DeviceGeneration>,
    // 'static is a lie wgpu accepts via `create_surface_unsafe`; the caller
    // guarantees the window outlives this swapchain, exactly as for
    // `crate::Surface`.
    surface: wgpu::Surface<'static>,
    config: wgpu::SurfaceConfiguration,
    configure_site: ConfigureSite,
}

impl<'ctx> Swapchain<'ctx> {
    pub(crate) fn new(
        ctx: &'ctx Surfaces,
        target: WindowTarget,
        size: FrameSize,
    ) -> Result<Self, SurfaceLost> {
        let device = ctx.active_generation()?;
        let present = PresentPolicy::for_target(&target);
        let configure_site = ConfigureSite::for_target(&target);
        let extent = texels(size).ok_or(Kind::BadDimensions(size))?;
        let max = device.max_texture_dim;
        if extent.0 > max || extent.1 > max {
            return Err(Kind::BadDimensions(size).into());
        }

        // SAFETY: the caller guarantees the target's window/layer outlives this
        // swapchain (see the `surface` field note).
        let surface = unsafe { create_surface(&device.instance, target)? };

        if !device.adapter.is_surface_supported(&surface) {
            return Err(Kind::SurfaceUnsupported.into());
        }

        let caps = surface.get_capabilities(&device.adapter);
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format: FORMAT,
            color_space: wgpu::SurfaceColorSpace::Auto,
            width: extent.0,
            height: extent.1,
            present_mode: present.mode(),
            desired_maximum_frame_latency: 2,
            alpha_mode: pick_alpha_mode(&caps),
            view_formats: vec![],
        };

        let swapchain = Self {
            ctx,
            device,
            surface,
            config,
            configure_site,
        };
        swapchain.configure();
        Ok(swapchain)
    }

    /// The GPU device generation this swapchain was bound to.
    pub fn device_generation(&self) -> u64 {
        self.device.generation
    }

    pub fn adapter(&self) -> &wgpu::Adapter {
        &self.device.adapter
    }

    pub fn device(&self) -> &wgpu::Device {
        &self.device.device
    }

    pub fn queue(&self) -> &wgpu::Queue {
        &self.device.queue
    }

    fn configure(&self) {
        self.device.configure_surface(&self.surface, &self.config);
    }

    /// Reconfigure to `size`, under the write side of the submit gate.
    pub fn resize(&mut self, size: FrameSize) {
        if self.ctx.check_generation_lost(&self.device).is_some() {
            return;
        }
        let Some((w, h)) = texels(size) else {
            return;
        };
        let (w, h) = (
            w.min(self.device.max_texture_dim),
            h.min(self.device.max_texture_dim),
        );
        if (w, h) == (self.config.width, self.config.height) {
            return;
        }
        self.config.width = w;
        self.config.height = h;
        self.configure();
    }

    pub fn format(&self) -> wgpu::TextureFormat {
        self.config.format
    }

    /// One acquire attempt, with no gate held.
    ///
    /// A stale swapchain is reconfigured and retried once where this thread may
    /// configure, and anything still without a texture — under
    /// [`ConfigureSite::Owner`] that includes every stale outcome — comes back
    /// as [`Acquired::Deferred`] rather than as a second attempt against the
    /// same configuration. When the underlying GPU device has been lost or
    /// invalidated, returns [`Acquired::Lost`] so the caller can coordinate
    /// device-loss recovery instead of retrying against a dead device.
    pub fn acquire(&mut self) -> Acquired<'ctx> {
        use wgpu::CurrentSurfaceTexture::*;
        if let Some(loss) = self.ctx.check_generation_lost(&self.device) {
            return Acquired::Lost(loss);
        }
        let mut reconfigured = false;
        loop {
            let texture = match self.surface.get_current_texture() {
                Success(texture) => texture,
                // The owner configures, and it is not this thread; its next
                // resize rebuilds the swapchain.
                Suboptimal(_) | Lost | Outdated if self.configure_site == ConfigureSite::Owner => {
                    if let Some(loss) = self.ctx.check_generation_lost(&self.device) {
                        return Acquired::Lost(loss);
                    }
                    return Acquired::Deferred(Deferred::new());
                }
                // Usable, but the swapchain no longer matches the surface.
                // Present it; the next acquire reconfigures.
                Suboptimal(texture) => texture,
                // Stale swapchain, typically a resize.
                Lost | Outdated if !reconfigured => {
                    if let Some(loss) = self.ctx.check_generation_lost(&self.device) {
                        return Acquired::Lost(loss);
                    }
                    reconfigured = true;
                    self.configure();
                    continue;
                }
                Lost | Outdated | Timeout | Occluded => {
                    if let Some(loss) = self.ctx.check_generation_lost(&self.device) {
                        return Acquired::Lost(loss);
                    }
                    return Acquired::Deferred(Deferred::new());
                }
                Validation => {
                    let reason = self.ctx.report_generation_lost(
                        &self.device,
                        "swapchain acquire failed validation",
                    );
                    tracing::error!(
                        "gpu_paint [overlay-d3d12]: swapchain acquire failed validation: {reason}"
                    );
                    return Acquired::Lost(SurfaceLost::device_lost(reason));
                }
            };
            let view = texture
                .texture
                .create_view(&wgpu::TextureViewDescriptor::default());
            return Acquired::Frame(Frame {
                device: Arc::clone(&self.device),
                texture,
                view,
                _lifetime: std::marker::PhantomData,
            });
        }
    }
}

/// One acquired swapchain frame, owed to the compositor.
///
/// It has no `Drop` of its own: the only ways to consume it are
/// [`Frame::present`] and [`Frame::supersede`].
#[must_use = "a frame is presented or superseded, never dropped"]
pub struct Frame<'ctx> {
    device: Arc<DeviceGeneration>,
    texture: wgpu::SurfaceTexture,
    view: wgpu::TextureView,
    _lifetime: std::marker::PhantomData<&'ctx Surfaces>,
}

impl<'ctx> Frame<'ctx> {
    pub fn view(&self) -> &wgpu::TextureView {
        &self.view
    }

    /// Run `encode` under the read side of the submit gate, release the gate,
    /// then commit.
    ///
    /// A FIFO present blocks until the compositor releases the previous frame,
    /// and nothing else may be forced to wait behind it: the gate exists to
    /// keep `configure` off a live submit, and the submit is done once `encode`
    /// returns.
    pub fn present(self, encode: impl FnOnce(&wgpu::TextureView)) -> Presented {
        let Frame {
            device,
            texture,
            view,
            ..
        } = self;
        {
            let _gate = device.submit_gate.read();
            encode(&view);
        }
        drop(view);
        device.queue.present(texture);
        Presented::issued()
    }

    /// Discharges this frame by acquiring the successor named here.
    ///
    /// The texture goes back to the swapchain first, so the successor is the
    /// next one the compositor has to give — which, on a swapchain with none
    /// this cycle, is the retry it owes instead.
    pub fn supersede(self, successor: &mut Swapchain<'ctx>) -> Acquired<'ctx> {
        drop(self);
        successor.acquire()
    }
}

/// What one acquire produced: a frame owed to the compositor, the retry the
/// swapchain owes the caller, or a lost surface/device requiring recovery.
#[must_use = "each arm carries an obligation"]
pub enum Acquired<'ctx> {
    Frame(Frame<'ctx>),
    Deferred(Deferred),
    Lost(SurfaceLost),
}
