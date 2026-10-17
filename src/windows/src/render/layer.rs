//! One visual and the painter bound to it — the single shape the content view
//! and the OSR popup both take. A layer owns its painter for its whole life;
//! nothing checks one out.

use std::ptr::NonNull;

use jfn_gpu_paint::{
    FrameSize, Pixels, PresentFailed, SharedTexture, Surface as Painter, WindowTarget,
};
use windows::Win32::Graphics::DirectComposition::{IDCompositionVisual, IDCompositionVisual3};
use windows_core::Interface;

use crate::render::{Part, device};

pub(crate) struct Layer {
    visual: IDCompositionVisual,
    /// `None` only until the first frame builds the swapchain, and again
    /// after a present reported the surface lost.
    painter: Option<Painter<'static>>,
    /// Set by [`Layer::window_target`]: the caller presents to this visual
    /// itself, so no painter is ever built for it and no frame is ever
    /// accepted here.
    external: bool,
    /// Physical-pixel offset inside the parent visual, retained so a rebuilt
    /// DirectComposition visual tree restores the exact placement.
    offset: (f32, f32),
    /// Visibility state set through `IDCompositionVisual3::SetVisible`,
    /// retained across visual tree recreation.
    visible: bool,
    /// Set whenever a present changed the visual's content binding; drained by
    /// [`Layer::take_needs_commit`].
    needs_commit: bool,
}

/// Outcome of presenting a frame to a [`Layer`].
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) enum LayerPresentOutcome {
    /// The frame reached the swapchain's commit stream.
    Presented,
    /// The frame was deferred or rejected without losing the process-wide GPU device.
    Rejected,
    /// The underlying GPU device generation was lost and requires coordinated
    /// invalidation and device recovery across all surfaces.
    DeviceLost {
        failed_generation: u64,
        reason: String,
    },
}

/// Diagnostic classification for a failed frame presentation on Windows.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) struct FailureDiagnostic {
    pub(crate) subsystem: &'static str,
    pub(crate) detail: String,
    pub(crate) surface_lost: bool,
    pub(crate) device_lost: bool,
}

pub(crate) fn diagnose_present_failure(part: Part, err: &PresentFailed) -> FailureDiagnostic {
    match err {
        PresentFailed::Deferred(_) => FailureDiagnostic {
            subsystem: "[overlay-d3d12]",
            detail: format!("swapchain deferred frame ({part:?})"),
            surface_lost: false,
            device_lost: false,
        },
        PresentFailed::Import(reason) => FailureDiagnostic {
            subsystem: "[cef-shared-import]",
            detail: format!("frame rejected ({part:?}): shared-texture import failed ({reason})"),
            surface_lost: false,
            device_lost: false,
        },
        PresentFailed::Kind => FailureDiagnostic {
            subsystem: "[cef-shared-import]",
            detail: format!(
                "frame rejected ({part:?}): frame kind changed on a live latched surface"
            ),
            surface_lost: false,
            device_lost: false,
        },
        PresentFailed::Lost(e) => FailureDiagnostic {
            subsystem: "[overlay-d3d12]",
            detail: format!("present failed, surface lost ({part:?}): {e}"),
            surface_lost: true,
            device_lost: e.is_device_lost(),
        },
    }
}

impl Layer {
    pub(crate) fn new(visual: IDCompositionVisual) -> Layer {
        Layer {
            visual,
            painter: None,
            external: false,
            offset: (0.0, 0.0),
            visible: true,
            needs_commit: false,
        }
    }

    pub(crate) fn visual(&self) -> &IDCompositionVisual {
        &self.visual
    }

    #[cfg(test)]
    pub(crate) fn offset(&self) -> (f32, f32) {
        self.offset
    }

    #[cfg(test)]
    pub(crate) fn is_visible(&self) -> bool {
        self.visible
    }

    #[cfg(test)]
    pub(crate) fn painter_generation(&self) -> Option<u64> {
        self.painter.as_ref().map(Painter::device_generation)
    }

    /// Physical-pixel offset of the visual inside its parent.
    pub(crate) fn set_offset(&mut self, x: f32, y: f32) {
        self.offset = (x, y);
        self.apply_offset();
    }

    fn apply_offset(&self) {
        unsafe {
            let _ = self.visual.SetOffsetX2(self.offset.0);
            let _ = self.visual.SetOffsetY2(self.offset.1);
        }
    }

    /// Sever the visual's content and mark the painter for a rebind: wgpu
    /// binds the swapchain to the visual inside `configure` and nowhere else,
    /// so an owner-side `SetContent(None)` leaves a painter whose extent never
    /// moved and whose content is unbound.
    pub(crate) fn detach(&mut self) {
        self.clear_content();
        if let Some(painter) = self.painter.as_mut() {
            painter.content_detached();
        }
    }

    /// Invalidates and drops any bound swapchain painter during coordinated GPU
    /// device-loss recovery, detaching the visual's content so no reference to
    /// the lost device's swapchain remains attached to the composition tree.
    pub(crate) fn invalidate_painter(&mut self) {
        if self.painter.take().is_some() {
            self.clear_content();
            self.needs_commit = true;
        }
    }

    /// Replaces the underlying `IDCompositionVisual` when the DirectComposition
    /// device itself is recreated, restoring stored offset and visibility state.
    pub(crate) fn replace_visual(&mut self, visual: IDCompositionVisual) {
        self.painter = None;
        self.visual = visual;
        self.apply_offset();
        if !self.visible {
            self.apply_visible();
        }
        self.needs_commit = true;
    }

    /// Sets the visual's visibility through `IDCompositionVisual3::SetVisible`.
    /// The swapchain's binding to the visual is untouched, so a layer shown
    /// again presents into the visual it was already bound to, and an
    /// externally presented layer keeps its content across a hide.
    pub(crate) fn set_visible(&mut self, visible: bool) {
        self.visible = visible;
        self.apply_visible();
    }

    fn apply_visible(&self) {
        let visual3: IDCompositionVisual3 = match self.visual.cast() {
            Ok(visual3) => visual3,
            Err(e) => {
                tracing::error!(target: "platform", "[overlay-d3d12] IDCompositionVisual3 unavailable: {e:?}");
                return;
            }
        };
        if let Err(e) = unsafe { visual3.SetVisible(self.visible) } {
            tracing::error!(target: "platform", "[overlay-d3d12] SetVisible failed: {e:?}");
        }
    }

    fn clear_content(&self) {
        unsafe {
            let _ = self.visual.SetContent(None::<&windows_core::IUnknown>);
        }
    }

    /// The content `IDCompositionVisual` for this layer.
    ///
    /// The first call marks the layer external: it builds no painter and
    /// accepts no frame.
    pub(crate) fn window_target(&mut self) -> Option<WindowTarget> {
        self.external = true;
        self.painter = None;
        let visual = NonNull::new(self.visual.as_raw())?;
        Some(WindowTarget::CompositionVisual { visual })
    }

    /// Ensures a painter is ready for a frame of `size`, building the swapchain
    /// from `size` on first use or after recovery.
    fn ensure_painter(&mut self, part: Part, size: FrameSize) -> Result<(), LayerPresentOutcome> {
        if self.external {
            return Err(LayerPresentOutcome::Rejected);
        }
        if self.painter.is_some() {
            return Ok(());
        }
        self.build_painter(part, size)
    }

    /// Presents CPU pixels, latching the copied kind on this layer's first
    /// frame. Reports whether the frame reached the swapchain's commit stream
    /// or triggered device-loss recovery.
    pub(crate) fn present_pixels(&mut self, part: Part, pixels: Pixels<'_>) -> LayerPresentOutcome {
        if let Err(outcome) = self.ensure_painter(part, pixels.size) {
            return outcome;
        }
        let Some(painter) = self.painter.as_mut() else {
            return LayerPresentOutcome::Rejected;
        };
        let outcome = painter.present_pixels(pixels, || {});
        self.settle(part, outcome)
    }

    /// Presents a shared texture, latching the shared kind on this layer's
    /// first frame. Reports whether the frame reached the commit stream or
    /// triggered device-loss recovery.
    pub(crate) fn present_shared(
        &mut self,
        part: Part,
        texture: &SharedTexture,
    ) -> LayerPresentOutcome {
        if let Err(outcome) = self.ensure_painter(part, texture.coded()) {
            return outcome;
        }
        let Some(painter) = self.painter.as_mut() else {
            return LayerPresentOutcome::Rejected;
        };
        let outcome = painter.present_shared(texture, || {});
        self.settle(part, outcome)
    }

    /// Records whether the visual's content binding changed (a configure bound
    /// the swapchain, or a failure severed it), so the device must `Commit` to
    /// publish it; plain presents flip the bound swapchain without one.
    fn settle<T>(&mut self, part: Part, outcome: Result<T, PresentFailed>) -> LayerPresentOutcome {
        match outcome {
            Ok(_presented) => {
                if let Some(painter) = self.painter.as_ref()
                    && painter.take_configured()
                {
                    self.needs_commit = true;
                }
                LayerPresentOutcome::Presented
            }
            Err(err) => {
                let diag = diagnose_present_failure(part, &err);
                if diag.surface_lost {
                    tracing::error!(target: "platform", "{} {}", diag.subsystem, diag.detail);
                    let failed_generation = self.painter.as_ref().map_or_else(
                        || device::gpu().map_or(1, jfn_gpu_paint::Surfaces::current_generation),
                        Painter::device_generation,
                    );
                    self.painter = None;
                    self.clear_content();
                    self.needs_commit = true;
                    if diag.device_lost {
                        return LayerPresentOutcome::DeviceLost {
                            failed_generation,
                            reason: diag.detail,
                        };
                    }
                } else if matches!(err, PresentFailed::Deferred(_)) {
                    tracing::debug!(target: "platform", "{} {}", diag.subsystem, diag.detail);
                } else {
                    tracing::warn!(target: "platform", "{} {}", diag.subsystem, diag.detail);
                }
                LayerPresentOutcome::Rejected
            }
        }
    }

    /// Whether a `Commit` is owed for a content binding this layer changed.
    /// Drained, so one commit answers every present since the last.
    pub(crate) fn take_needs_commit(&mut self) -> bool {
        std::mem::take(&mut self.needs_commit)
    }

    fn build_painter(&mut self, part: Part, size: FrameSize) -> Result<(), LayerPresentOutcome> {
        let Some(gpu) = device::gpu() else {
            tracing::error!(
                target: "platform",
                "[overlay-d3d12] gpu_paint surface creation failed ({part:?}, size={}x{}): overlay D3D12 device unavailable",
                size.w,
                size.h,
            );
            return Err(LayerPresentOutcome::Rejected);
        };
        let Some(visual) = NonNull::new(self.visual.as_raw()) else {
            tracing::error!(
                target: "platform",
                "[overlay-d3d12] gpu_paint surface creation failed ({part:?}, size={}x{}): DirectComposition visual pointer is null",
                size.w,
                size.h,
            );
            return Err(LayerPresentOutcome::Rejected);
        };
        let current_generation = gpu.current_generation();
        match gpu.new_surface(WindowTarget::CompositionVisual { visual }, size) {
            Ok(painter) => {
                self.painter = Some(painter);
                Ok(())
            }
            Err(e) => {
                tracing::error!(
                    target: "platform",
                    "[overlay-d3d12] gpu_paint surface creation failed ({part:?}, size={}x{}): {e}",
                    size.w,
                    size.h,
                );
                if e.is_device_lost() {
                    Err(LayerPresentOutcome::DeviceLost {
                        failed_generation: current_generation,
                        reason: e.to_string(),
                    })
                } else {
                    Err(LayerPresentOutcome::Rejected)
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jfn_gpu_paint::Deferred;

    #[test]
    fn diagnose_present_failure_attributes_import_and_kind_to_shared_import() {
        let import_diag =
            diagnose_present_failure(Part::Content, &PresentFailed::Import("OpenSharedHandle"));
        assert_eq!(import_diag.subsystem, "[cef-shared-import]");
        assert!(import_diag.detail.contains("OpenSharedHandle"));
        assert!(!import_diag.surface_lost);
        assert!(!import_diag.device_lost);

        let kind_diag = diagnose_present_failure(Part::Popup, &PresentFailed::Kind);
        assert_eq!(kind_diag.subsystem, "[cef-shared-import]");
        assert!(kind_diag.detail.contains("Popup"));
        assert!(!kind_diag.surface_lost);
        assert!(!kind_diag.device_lost);
    }

    #[test]
    fn diagnose_present_failure_attributes_deferred_to_overlay_d3d12() {
        let deferred =
            diagnose_present_failure(Part::Content, &PresentFailed::Deferred(Deferred::new()));
        assert_eq!(deferred.subsystem, "[overlay-d3d12]");
        assert!(!deferred.surface_lost);
        assert!(!deferred.device_lost);
    }
}
