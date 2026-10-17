//! DirectComposition presenter.
//!
//! Every frame CEF produces is presented 1:1 under a composition visual. The
//! composition target is the mpv HWND, so DWM clips a frame wider or taller
//! than the window and a frame smaller than it leaves the window's own
//! background beyond its edges. No frame is judged against a window size, and
//! the renderer holds no size state at all: a mis-sized frame is a
//! one-frame-old picture, corrected by the next paint after the window-changed
//! wakeup resizes CEF.
//!
//! Should DWM's window clip ever prove insufficient, the fallback is
//! `IDCompositionVisual::SetClip2` on the root visual, set to the client rect
//! from `crate::window::client_extent`.
//!
//! Threading. `STATE` is the process's DirectComposition serialization point:
//! every visual-tree call, every swapchain build, and every present happens
//! under it, each entry point taking it exactly once. That is only sound
//! while no window-message thread enters this module — `alloc`, `free`,
//! `apply_stack`, `set_visibility`, `present` and the popup entry points all
//! run on the CEF UI thread and are already serialized by it, leaving `init` and
//! `cleanup` on the app main thread at the process edges as the sole
//! cross-thread contention, where waiting out an in-flight `configure` costs
//! nothing. The WndProc hook in `crate::platform` must therefore stay
//! republish-only; routing any window message into the renderer would put a
//! GPU wait in front of the window's message queue.

mod device;
mod layer;

use std::ffi::c_int;

use jfn_gpu_paint::{FrameSize, Pixels, WindowTarget};
use jfn_platform_abi::{
    Ack, Content, PaintFrame, Presented, SurfaceHandle, SurfaceSize, Visibility, VisibilityCommit,
};
use parking_lot::Mutex;
use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::DirectComposition::IDCompositionVisual;

use crate::render::device::Devices;
use crate::render::layer::{Layer, LayerPresentOutcome};

/// Which of a surface's two visuals a call addresses.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub(crate) enum Part {
    /// The browser view, filling the window.
    Content,
    /// The OSR dropdown, nested under the content visual.
    Popup,
}

/// A live surface's identity. Handed to CEF packed into a [`SurfaceHandle`]
/// and never dereferenced; ids start at 1 and are never reused, so a stale
/// handle resolves to nothing.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub(crate) struct SurfaceId(u64);

/// One CEF surface. The only constructor builds both visuals, nests the popup
/// under the content visual, and parents the content visual to the root, so
/// an `Entry` that exists is a surface that is in the tree with its popup
/// nested — neither is a rule the rest of the module has to keep.
struct Entry {
    id: SurfaceId,
    content: Layer,
    popup: Layer,
}

impl Entry {
    fn create(devices: &Devices, id: SurfaceId) -> windows_core::Result<Entry> {
        let content = devices.new_visual()?;
        let popup = devices.new_visual()?;
        unsafe {
            content.AddVisual(&popup, true, None::<&IDCompositionVisual>)?;
            devices
                .root()
                .AddVisual(&content, true, None::<&IDCompositionVisual>)?;
        }
        Ok(Entry {
            id,
            content: Layer::new(content),
            popup: Layer::new(popup),
        })
    }

    fn layer_mut(&mut self, part: Part) -> &mut Layer {
        match part {
            Part::Content => &mut self.content,
            Part::Popup => &mut self.popup,
        }
    }

    fn invalidate_painters(&mut self) {
        self.content.invalidate_painter();
        self.popup.invalidate_painter();
    }

    /// Unparents the content visual from `root`; the popup goes with it.
    fn unparent(&mut self, root: &IDCompositionVisual) {
        unsafe {
            let _ = root.RemoveVisual(self.content.visual());
        }
    }
}

struct Registry {
    hwnd: Option<HWND>,
    devices: Option<Devices>,
    /// Live surfaces, bottom-to-top: exactly the root visual's children, in
    /// the root's child order.
    surfaces: Vec<Entry>,
    next_id: u64,
}

impl Registry {
    const fn new() -> Registry {
        Registry {
            hwnd: None,
            devices: None,
            surfaces: Vec::new(),
            next_id: 1,
        }
    }

    fn find_mut(&mut self, h: SurfaceHandle) -> Option<&mut Entry> {
        let id = h.id();
        self.surfaces.iter_mut().find(|e| e.id.0 == id)
    }

    fn commit(&self) -> bool {
        self.devices.as_ref().is_some_and(Devices::commit)
    }

    fn commit_and_wait(&self) -> bool {
        self.devices.as_ref().is_some_and(Devices::commit_and_wait)
    }

    /// Drops every bound swapchain painter across all surfaces and clears
    /// their composition visual content so no swapchain remains bound to a
    /// lost GPU device generation.
    fn invalidate_all_painters(&mut self) {
        for entry in &mut self.surfaces {
            entry.invalidate_painters();
            let _ = entry.content.take_needs_commit();
            let _ = entry.popup.take_needs_commit();
        }
    }

    /// Rebuilds the DirectComposition device, HWND target, root visual, and
    /// every live surface's content/popup visuals in their existing z-order,
    /// restoring stored offsets and visibility states.
    fn recreate_dcomp_tree(&mut self) -> Result<(), String> {
        let hwnd = self
            .hwnd
            .ok_or_else(|| "target HWND is not recorded".to_owned())?;
        self.invalidate_all_painters();
        self.devices = None;
        let devices = Devices::create(hwnd)
            .map_err(|e| format!("DirectComposition device recreation failed: {e:?}"))?;
        let mut prev: Option<IDCompositionVisual> = None;
        for entry in &mut self.surfaces {
            let content = devices
                .new_visual()
                .map_err(|e| format!("DirectComposition content visual creation failed: {e:?}"))?;
            let popup = devices
                .new_visual()
                .map_err(|e| format!("DirectComposition popup visual creation failed: {e:?}"))?;
            unsafe {
                content
                    .AddVisual(&popup, true, None::<&IDCompositionVisual>)
                    .map_err(|e| format!("DirectComposition popup nesting failed: {e:?}"))?;
                match prev.as_ref() {
                    Some(prev_visual) => devices
                        .root()
                        .AddVisual(&content, true, prev_visual)
                        .map_err(|e| format!("DirectComposition root AddVisual failed: {e:?}"))?,
                    None => devices
                        .root()
                        .AddVisual(&content, false, None::<&IDCompositionVisual>)
                        .map_err(|e| format!("DirectComposition root AddVisual failed: {e:?}"))?,
                }
            }
            prev = Some(content.clone());
            entry.content.replace_visual(content);
            entry.popup.replace_visual(popup);
            let _ = entry.content.take_needs_commit();
            let _ = entry.popup.take_needs_commit();
        }
        if !devices.commit() {
            return Err("DirectComposition Commit failed on recreated device".to_owned());
        }
        self.devices = Some(devices);
        Ok(())
    }

    /// Coordinated invalidation and recreation path for GPU device loss:
    /// 1. Detaches and drops every surface painter bound to the lost device.
    /// 2. Commits the visual detachment (or recreates the DirectComposition
    ///    tree if the composition device itself failed).
    /// 3. Recreates the `jfn_gpu_paint` device generation and verifies adapter
    ///    LUID and shared-texture compatibility with CEF.
    /// 4. Initiates application shutdown if recovery requires a restart.
    fn recover_from_device_loss(
        &mut self,
        failed_generation: u64,
        reason: &str,
    ) -> jfn_gpu_paint::DeviceRecoveryOutcome {
        tracing::warn!(
            target: "platform",
            "[overlay-d3d12] initiating coordinated GPU device-loss recovery from generation {failed_generation}: {reason}"
        );
        self.invalidate_all_painters();
        if !self.commit()
            && let Err(dcomp_err) = self.recreate_dcomp_tree()
        {
            let restart_reason =
                format!("{reason}; DirectComposition recovery failed: {dcomp_err}");
            tracing::error!(
                target: "platform",
                "[overlay-d3d12] [cef-shared-import] GPU device-loss recovery requires application restart: {restart_reason}"
            );
            jfn_playback::shutdown::jfn_shutdown_initiate();
            return jfn_gpu_paint::DeviceRecoveryOutcome::RestartRequired {
                generation: failed_generation,
                reason: restart_reason,
            };
        }
        let Some(gpu) = crate::render::device::gpu() else {
            let restart_reason =
                format!("{reason}; overlay D3D12 device coordinator is unavailable");
            tracing::error!(
                target: "platform",
                "[overlay-d3d12] [cef-shared-import] GPU device-loss recovery requires application restart: {restart_reason}"
            );
            jfn_playback::shutdown::jfn_shutdown_initiate();
            return jfn_gpu_paint::DeviceRecoveryOutcome::RestartRequired {
                generation: failed_generation,
                reason: restart_reason,
            };
        };
        let outcome = gpu.recover_generation(failed_generation, reason);
        if let jfn_gpu_paint::DeviceRecoveryOutcome::RestartRequired {
            generation,
            ref reason,
        } = outcome
        {
            tracing::error!(
                target: "platform",
                "[overlay-d3d12] [cef-shared-import] GPU device-loss recovery at generation {generation} requires application restart: {reason}"
            );
            jfn_playback::shutdown::jfn_shutdown_initiate();
        }
        outcome
    }
}

// SAFETY: `Registry` holds DirectComposition interface pointers, which COM
// marks apartment-bound. DComp objects are in fact free-threaded (documented
// "DirectComposition and multithreading": any thread may call them, the
// device serializing internally), so moving them across threads is sound; the
// mutual exclusion this module additionally relies on comes from `STATE`'s
// Mutex plus the threading contract in the module doc — every entry point
// takes the lock exactly once, and no window-message thread enters.
unsafe impl Send for Registry {}

static STATE: Mutex<Registry> = Mutex::new(Registry::new());

/// Build the DComp device, HWND target, and root visual. Partial device state
/// is dropped on failure; errors retain the native operation's cause.
pub(crate) fn init(hwnd: HWND) -> Result<(), jfn_platform_abi::PlatformInitError> {
    use jfn_platform_abi::PlatformInitError as Error;
    if !jfn_gpu_paint::any_adapter() {
        return Err(Error::backend(
            "DirectComposition adapter selection",
            "no usable GPU adapter",
        ));
    }
    let mut st = STATE.lock();
    if st.devices.is_some() {
        return Err(Error::backend(
            "DirectComposition initialization",
            "already initialized",
        ));
    }
    st.devices = Some(
        Devices::create(hwnd)
            .map_err(|e| Error::backend("DirectComposition device creation", e))?,
    );
    st.hwnd = Some(hwnd);
    Ok(())
}

/// Drop every remaining surface, then the devices. Runs after the WndProc
/// hook is unhooked and the input thread is joined.
pub(crate) fn cleanup() {
    let mut st = STATE.lock();
    let Registry {
        devices, surfaces, ..
    } = &mut *st;
    if let Some(devices) = devices.as_ref() {
        for entry in surfaces.iter_mut() {
            entry.unparent(devices.root());
        }
    }
    surfaces.clear();
    let _ = st.commit();
    st.devices = None;
    st.hwnd = None;
}

/// Allocate a surface: a content visual parented on top of the root and a
/// popup visual nested under it.
pub(crate) fn alloc() -> SurfaceHandle {
    let mut st = STATE.lock();
    let id = SurfaceId(st.next_id);
    let Some(devices) = st.devices.as_ref() else {
        return SurfaceHandle::NONE;
    };
    let entry = match Entry::create(devices, id) {
        Ok(entry) => entry,
        Err(e) => {
            tracing::error!(target: "platform", "surface creation failed: {e:?}");
            return SurfaceHandle::NONE;
        }
    };
    let _ = devices.commit();
    st.next_id += 1;
    st.surfaces.push(entry);
    SurfaceHandle::from_id(id.0)
}

/// Unparent and drop a surface, its visuals and its painters.
pub(crate) fn free(h: SurfaceHandle) {
    let mut st = STATE.lock();
    let id = h.id();
    let Some(pos) = st.surfaces.iter().position(|e| e.id.0 == id) else {
        return;
    };
    let mut entry = st.surfaces.remove(pos);
    if let Some(devices) = st.devices.as_ref() {
        entry.unparent(devices.root());
        let _ = devices.commit();
    }
}

/// Apply the whole z-order to the root's children, bottom-to-top. Live
/// surfaces `ordered` does not name keep their relative order above those it
/// does, so every live surface stays parented. Video needs no placing: the
/// composition target is mpv's own HWND, so every visual is above it.
pub(crate) fn apply_stack(ordered: &[SurfaceHandle]) {
    let mut st = STATE.lock();
    let Registry {
        devices, surfaces, ..
    } = &mut *st;
    let Some(root) = devices.as_ref().map(Devices::root) else {
        return;
    };
    let rank = |e: &Entry| ordered.iter().position(|h| h.id() == e.id.0);

    let (mut named, mut unnamed): (Vec<Entry>, Vec<Entry>) = std::mem::take(surfaces)
        .into_iter()
        .partition(|e| rank(e).is_some());
    named.sort_by_key(|e| rank(e).unwrap_or(usize::MAX));
    named.append(&mut unnamed);

    unsafe {
        for entry in &named {
            let _ = root.RemoveVisual(entry.content.visual());
        }
        let mut prev: Option<&IDCompositionVisual> = None;
        for entry in &named {
            let visual = entry.content.visual();
            let placed = match prev {
                Some(prev) => root.AddVisual(visual, true, prev),
                None => root.AddVisual(visual, false, None::<&IDCompositionVisual>),
            };
            match placed {
                Ok(()) => prev = Some(visual),
                Err(e) => {
                    tracing::error!(target: "platform", "apply_stack AddVisual failed: {e:?}");
                }
            }
        }
    }

    *surfaces = named;
    let _ = st.commit();
}

/// Set a surface's content visual to `visibility`, publish it, and wait for
/// the composition engine to process the commit. Nothing here moves the visual
/// or touches its content binding.
pub(crate) fn set_visibility(h: SurfaceHandle, visibility: Visibility) -> VisibilityCommit {
    let mut st = STATE.lock();
    if let Some(entry) = st.find_mut(h) {
        entry.content.set_visible(visibility.is_shown());
    }
    let _ = st.commit_and_wait();
    VisibilityCommit::issued(visibility, Ack::immediate())
}

/// The extent a frame's content asks the swapchain for, or the diagnostic
/// reason the content carries nothing presentable.
fn frame_extent(content: &Content<'_>) -> Result<FrameSize, (&'static str, String)> {
    match content {
        Content::Accelerated(tex) => {
            if tex.handle().is_null() {
                return Err((
                    "[cef-shared-import]",
                    format!(
                        "null shared texture handle (coded={}x{})",
                        tex.coded().w,
                        tex.coded().h
                    ),
                ));
            }
            let coded = tex.coded();
            if coded.w <= 0 || coded.h <= 0 {
                return Err((
                    "[cef-shared-import]",
                    format!(
                        "non-positive shared texture coded size {}x{}",
                        coded.w, coded.h
                    ),
                ));
            }
            Ok(coded)
        }
        Content::Software { size, pixels, .. } => {
            if pixels.is_empty() || size.w <= 0 || size.h <= 0 {
                return Err((
                    "[cef-angle]",
                    format!(
                        "unusable software frame (size={}x{}, bytes={})",
                        size.w,
                        size.h,
                        pixels.len()
                    ),
                ));
            }
            Ok(FrameSize {
                w: size.w,
                h: size.h,
            })
        }
    }
}

fn present_once(
    st: &mut Registry,
    h: SurfaceHandle,
    part: Part,
    size: FrameSize,
    content: &Content<'_>,
) -> LayerPresentOutcome {
    let (outcome, needs_commit) = {
        let Some(entry) = st.find_mut(h) else {
            return LayerPresentOutcome::Rejected;
        };
        let layer = entry.layer_mut(part);
        let outcome = match content {
            Content::Accelerated(tex) => layer.present_shared(part, tex),
            Content::Software { pixels, dirty, .. } => layer.present_pixels(
                part,
                Pixels {
                    size,
                    stride: size.w as u32 * 4,
                    bgra: pixels,
                    dirty,
                },
            ),
        };
        (outcome, layer.take_needs_commit())
    };
    if needs_commit && !st.commit() {
        let failed_generation =
            crate::render::device::gpu().map_or(1, jfn_gpu_paint::Surfaces::current_generation);
        return LayerPresentOutcome::DeviceLost {
            failed_generation,
            reason: "DirectComposition Commit failed after swapchain configure".to_owned(),
        };
    }
    outcome
}

/// Present one frame to `part`. The frame comes back undischarged when the
/// surface is gone, the content carries no picture, or the layer has no
/// swapchain to take it.
///
/// When the underlying GPU device was lost, runs coordinated surface
/// invalidation and device recreation once, resuming presentation on the
/// recreated device if compatible or initiating shutdown if restart is required.
pub(crate) fn present<'a>(
    h: SurfaceHandle,
    part: Part,
    frame: PaintFrame<'a>,
) -> Result<Presented, PaintFrame<'a>> {
    let mut st = STATE.lock();
    if st.devices.is_none() {
        tracing::warn!(
            target: "platform",
            "[overlay-d3d12] frame rejected on surface {} ({part:?}): DirectComposition device not initialized",
            h.id(),
        );
        return Err(frame);
    }
    let size = match frame_extent(frame.content()) {
        Ok(size) => size,
        Err((subsystem, reason)) => {
            tracing::warn!(
                target: "platform",
                "{subsystem} frame rejected on surface {} ({part:?}): {reason}",
                h.id(),
            );
            return Err(frame);
        }
    };
    let outcome = match present_once(&mut st, h, part, size, frame.content()) {
        LayerPresentOutcome::Presented => LayerPresentOutcome::Presented,
        LayerPresentOutcome::Rejected => LayerPresentOutcome::Rejected,
        LayerPresentOutcome::DeviceLost {
            failed_generation,
            reason,
        } => match st.recover_from_device_loss(failed_generation, &reason) {
            jfn_gpu_paint::DeviceRecoveryOutcome::Recovered { .. } => {
                present_once(&mut st, h, part, size, frame.content())
            }
            jfn_gpu_paint::DeviceRecoveryOutcome::RestartRequired { .. } => {
                LayerPresentOutcome::Rejected
            }
        },
    };
    if outcome != LayerPresentOutcome::Presented {
        return Err(frame);
    }
    Ok(frame.present(|_content| Presented::issued()))
}

/// Offsets the content visual by the reserved top inset and sizes it to match.
pub(crate) fn resize(h: SurfaceHandle, size: SurfaceSize) {
    let mut st = STATE.lock();
    let Some(entry) = st.find_mut(h) else {
        return;
    };
    entry.content.set_offset(0.0, size.physical_top as f32);
    let _ = st.commit();
}

/// The content `IDCompositionVisual` for `h`.
///
/// The first call marks the surface external: its [`Layer`] builds no painter
/// and drops every present.
pub(crate) fn window_target(h: SurfaceHandle) -> Option<WindowTarget> {
    let mut st = STATE.lock();
    st.find_mut(h)?.content.window_target()
}

/// Place the popup visual at `x`, `y` — logical pixels inside the owning
/// surface. The popup shows itself with its first frame.
pub(crate) fn popup_show(h: SurfaceHandle, x: c_int, y: c_int) {
    let scale = crate::platform::win_get_scale();
    let (Some(px), Some(py)) = (scale.to_physical(x), scale.to_physical(y)) else {
        tracing::error!(target: "platform", "popup offset {x},{y} is unrepresentable at scale {scale}");
        return;
    };
    let mut st = STATE.lock();
    let Some(entry) = st.find_mut(h) else {
        return;
    };
    entry.popup.set_offset(px as f32, py as f32);
    let _ = st.commit();
}

/// Detach the popup visual's content, so reopening it cannot flash the frame
/// it was closed with.
pub(crate) fn popup_hide(h: SurfaceHandle) {
    let mut st = STATE.lock();
    let Some(entry) = st.find_mut(h) else {
        return;
    };
    entry.popup.detach();
    let _ = st.commit();
}

#[cfg(test)]
mod tests {
    use super::*;
    use jfn_gpu_paint::{DeviceLifecycleState, SharedTexture};
    use jfn_platform_abi::PhysicalSize;
    use windows::Win32::UI::WindowsAndMessaging::{CreateWindowExW, DestroyWindow};
    use windows::core::w;

    struct TestWindow(HWND);

    impl Drop for TestWindow {
        fn drop(&mut self) {
            let _ = unsafe { DestroyWindow(self.0) };
        }
    }

    fn create_test_window() -> windows_core::Result<TestWindow> {
        Ok(TestWindow(unsafe {
            CreateWindowExW(
                Default::default(),
                w!("STATIC"),
                w!(""),
                Default::default(),
                0,
                0,
                64,
                64,
                None,
                None,
                None,
                None,
            )
        }?))
    }

    #[test]
    fn frame_extent_attributes_null_or_bad_shared_texture_to_cef_shared_import() {
        let null_tex = SharedTexture::new(
            std::ptr::null_mut(),
            FrameSize { w: 1920, h: 1080 },
            FrameSize { w: 1920, h: 1080 },
        );
        let err = frame_extent(&Content::Accelerated(null_tex)).err();
        assert_eq!(
            err.as_ref().map(|(sub, _)| *sub),
            Some("[cef-shared-import]")
        );
        assert!(
            err.as_ref()
                .is_some_and(|(_, msg)| msg.contains("null shared texture handle"))
        );

        let zero_coded = SharedTexture::new(
            std::ptr::dangling_mut::<std::ffi::c_void>(),
            FrameSize { w: 0, h: 1080 },
            FrameSize { w: 0, h: 1080 },
        );
        let err = frame_extent(&Content::Accelerated(zero_coded)).err();
        assert_eq!(
            err.as_ref().map(|(sub, _)| *sub),
            Some("[cef-shared-import]")
        );
        assert!(
            err.as_ref()
                .is_some_and(|(_, msg)| msg.contains("non-positive shared texture coded size"))
        );
    }

    #[test]
    fn frame_extent_attributes_unusable_software_buffer_to_cef_angle() {
        let err = frame_extent(&Content::Software {
            size: PhysicalSize { w: 800, h: 600 },
            pixels: &[],
            dirty: &[],
        })
        .err();
        assert_eq!(err.as_ref().map(|(sub, _)| *sub), Some("[cef-angle]"));
        assert!(
            err.as_ref()
                .is_some_and(|(_, msg)| msg.contains("unusable software frame"))
        );
    }

    #[test]
    fn recreate_dcomp_tree_preserves_surface_order_offsets_and_visibility()
    -> windows_core::Result<()> {
        let window = create_test_window()?;
        let devices = Devices::create(window.0)?;
        let mut registry = Registry {
            hwnd: Some(window.0),
            devices: Some(devices),
            surfaces: Vec::new(),
            next_id: 1,
        };
        if let Some(dev) = registry.devices.as_ref() {
            let mut first = Entry::create(dev, SurfaceId(1))?;
            first.content.set_offset(0.0, 36.0);
            first.content.set_visible(false);
            first.popup.set_offset(120.0, 240.0);
            let mut second = Entry::create(dev, SurfaceId(2))?;
            second.content.set_offset(0.0, 48.0);
            registry.surfaces.push(first);
            registry.surfaces.push(second);
        }

        assert_eq!(registry.recreate_dcomp_tree(), Ok(()));
        assert_eq!(registry.surfaces.len(), 2);
        assert_eq!(registry.surfaces[0].id, SurfaceId(1));
        assert_eq!(registry.surfaces[0].content.offset(), (0.0, 36.0));
        assert!(!registry.surfaces[0].content.is_visible());
        assert_eq!(registry.surfaces[0].popup.offset(), (120.0, 240.0));
        assert_eq!(registry.surfaces[1].id, SurfaceId(2));
        assert_eq!(registry.surfaces[1].content.offset(), (0.0, 48.0));
        assert!(registry.surfaces[1].content.is_visible());
        Ok(())
    }

    #[test]
    fn injected_device_loss_invalidates_dependent_surfaces_and_resumes_frames_on_compatible_device()
    -> windows_core::Result<()> {
        struct NoopSource;
        impl jfn_platform_abi::FrameSource for NoopSource {
            fn request_frame(&self) {}
        }

        let Some(gpu) = crate::render::device::gpu() else {
            return Ok(());
        };
        let window = create_test_window()?;
        cleanup();
        assert!(init(window.0).is_ok());

        let s1 = alloc();
        let s2 = alloc();
        let pixels = [0xff_u8; 4 * 4 * 4];
        let size = PhysicalSize { w: 4, h: 4 };
        let source: std::sync::Arc<dyn jfn_platform_abi::FrameSource> =
            std::sync::Arc::new(NoopSource);

        let first_frame = PaintFrame::software(std::sync::Arc::clone(&source), size, &pixels, &[]);
        assert!(present(s1, Part::Content, first_frame).is_ok());
        let second_frame = PaintFrame::software(std::sync::Arc::clone(&source), size, &pixels, &[]);
        assert!(present(s2, Part::Content, second_frame).is_ok());

        let initial_gen = gpu.current_generation();
        assert_eq!(
            gpu.lifecycle_state(),
            DeviceLifecycleState::Healthy {
                generation: initial_gen
            }
        );

        // Inject a simulated GPU device loss on the active generation.
        let lost_state = gpu.report_device_lost("simulated DXGI_ERROR_DEVICE_REMOVED");
        assert_eq!(
            lost_state,
            DeviceLifecycleState::Lost {
                generation: initial_gen,
                reason: "simulated DXGI_ERROR_DEVICE_REMOVED".to_owned(),
            }
        );

        // Presenting the next frame triggers coordinated invalidation of both
        // surfaces, recreates a compatible D3D12 device on the pinned LUID, and
        // resumes presentation on the new generation without dropping the frame.
        let recovery_frame = PaintFrame::software(source, size, &pixels, &[]);
        assert!(present(s1, Part::Content, recovery_frame).is_ok());

        let recovered_gen = gpu.current_generation();
        assert_eq!(recovered_gen, initial_gen + 1);
        assert_eq!(
            gpu.lifecycle_state(),
            DeviceLifecycleState::Healthy {
                generation: recovered_gen
            }
        );
        {
            let mut st = STATE.lock();
            assert_eq!(
                st.find_mut(s1).and_then(|e| e.content.painter_generation()),
                Some(recovered_gen)
            );
            // The sibling surface's stale painter was invalidated during recovery
            // and will lazily rebuild on `recovered_gen` when next presented.
            assert_eq!(
                st.find_mut(s2).and_then(|e| e.content.painter_generation()),
                None
            );
        }

        cleanup();
        Ok(())
    }
}
