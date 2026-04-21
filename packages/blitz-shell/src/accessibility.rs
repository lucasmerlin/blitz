use crate::{BlitzShellEvent, event::BlitzShellProxy};
use accesskit::{Rect, TreeUpdate};
use accesskit_xplat::{Adapter, EventHandler, WindowEvent as AccessKitEvent};
use blitz_dom::BaseDocument;
use std::sync::{Arc, Mutex};
use winit::{
    event::WindowEvent,
    raw_window_handle::HasWindowHandle,
    window::{Window, WindowId},
};

/// Pluggable implementation of Blitz's accessibility plumbing.
///
/// The default implementation ([`XplatAccessibilityBackend`]) wraps
/// [`accesskit_xplat::Adapter`] and registers with the host operating system's
/// accessibility APIs. Test harnesses (e.g. `kittest-winit`) can install a
/// custom backend — see [`CapturingAccessibilityBackend`] — to intercept
/// [`TreeUpdate`]s without touching the real OS accessibility pipeline.
pub trait AccessibilityBackend {
    /// Called when Blitz has a fresh accessibility tree ready for the backend.
    ///
    /// `build` is a one-shot closure that produces the tree. Implementations
    /// may skip calling it if they know no consumer is listening (this is how
    /// the real `accesskit_xplat::Adapter` avoids tree-building work when no
    /// assistive technology has connected). Test backends should always invoke
    /// it and capture the result.
    fn update(&mut self, build: &mut dyn FnMut() -> TreeUpdate);

    /// Forward a focus change from winit to the backend.
    fn set_focus(&mut self, is_focused: bool);

    /// Forward a window-bounds change from winit to the backend.
    fn set_window_bounds(&mut self, outer_bounds: Rect, inner_bounds: Rect);
}

/// State of the accessibility node tree and platform adapter.
pub struct AccessibilityState {
    backend: Box<dyn AccessibilityBackend>,
}

impl AccessibilityState {
    /// Build an `AccessibilityState` backed by the default OS adapter.
    pub fn new(window: &dyn Window, proxy: BlitzShellProxy) -> Self {
        Self::with_backend(Box::new(XplatAccessibilityBackend::new(window, proxy)))
    }

    /// Build an `AccessibilityState` backed by a custom [`AccessibilityBackend`].
    ///
    /// Used by tests to install a [`CapturingAccessibilityBackend`] instead of
    /// the OS adapter.
    pub fn with_backend(backend: Box<dyn AccessibilityBackend>) -> Self {
        Self { backend }
    }

    pub fn update_tree(&mut self, doc: &BaseDocument) {
        self.backend
            .update(&mut || doc.build_accessibility_tree());
    }

    /// Allows reacting to window events.
    ///
    /// This must be called whenever a new window event is received
    /// and before it is handled by the application.
    pub fn process_window_event(&mut self, window: &dyn Window, event: &WindowEvent) {
        match event {
            WindowEvent::Focused(is_focused) => {
                self.backend.set_focus(*is_focused);
            }
            WindowEvent::Moved(_) | WindowEvent::SurfaceResized(_) => {
                let outer_position: (_, _) = window
                    .outer_position()
                    .unwrap_or_default()
                    .cast::<f64>()
                    .into();
                let outer_size: (_, _) = window.outer_size().cast::<f64>().into();
                let inner_position: (_, _) = window.surface_position().cast::<f64>().into();
                let inner_size: (_, _) = window.surface_size().cast::<f64>().into();

                self.backend.set_window_bounds(
                    Rect::from_origin_size(outer_position, outer_size),
                    Rect::from_origin_size(inner_position, inner_size),
                );
            }
            _ => (),
        }
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// Default backend: wraps accesskit_xplat::Adapter (OS accessibility).
// ──────────────────────────────────────────────────────────────────────────────

struct Handler {
    window_id: WindowId,
    proxy: BlitzShellProxy,
}
impl EventHandler for Handler {
    fn handle_accesskit_event(&self, event: AccessKitEvent) {
        self.proxy.send_event(BlitzShellEvent::Accessibility {
            window_id: self.window_id,
            data: Arc::new(event),
        });
    }
}

/// [`AccessibilityBackend`] that bridges to [`accesskit_xplat::Adapter`] and
/// registers with the host OS accessibility APIs.
pub struct XplatAccessibilityBackend {
    adapter: Adapter,
}

impl XplatAccessibilityBackend {
    pub fn new(window: &dyn Window, proxy: BlitzShellProxy) -> Self {
        let window_id = window.id();
        let adapter = Adapter::with_combined_handler(
            #[cfg(target_os = "android")]
            &crate::current_android_app(),
            #[cfg(not(target_os = "android"))]
            window.window_handle().unwrap().as_raw(),
            Arc::new(Handler { window_id, proxy }),
        );
        Self { adapter }
    }
}

impl AccessibilityBackend for XplatAccessibilityBackend {
    fn update(&mut self, build: &mut dyn FnMut() -> TreeUpdate) {
        self.adapter.update_if_active(|| build());
    }

    fn set_focus(&mut self, is_focused: bool) {
        self.adapter.set_focus(is_focused);
    }

    fn set_window_bounds(&mut self, outer_bounds: Rect, inner_bounds: Rect) {
        self.adapter.set_window_bounds(outer_bounds, inner_bounds);
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// Test backend: captures the latest TreeUpdate into a shared handle.
// ──────────────────────────────────────────────────────────────────────────────

/// [`AccessibilityBackend`] intended for automated tests.
///
/// Every call to [`AccessibilityBackend::update`] invokes the `build` closure
/// and stashes the resulting [`TreeUpdate`] in a shared slot that the test can
/// drain via [`CapturedAccessibility::take`]. Nothing is registered with the
/// OS accessibility API, so this works with a headless `dyn Window` whose
/// `window_handle()` returns an error.
pub struct CapturingAccessibilityBackend {
    pending: Arc<Mutex<Option<TreeUpdate>>>,
}

impl CapturingAccessibilityBackend {
    /// Construct a new capturing backend, returning the backend and a handle
    /// the test can use to drain captured tree updates.
    pub fn new() -> (Self, CapturedAccessibility) {
        let pending = Arc::new(Mutex::new(None));
        let handle = CapturedAccessibility {
            pending: pending.clone(),
        };
        (Self { pending }, handle)
    }
}

impl Default for CapturingAccessibilityBackend {
    fn default() -> Self {
        Self::new().0
    }
}

impl AccessibilityBackend for CapturingAccessibilityBackend {
    fn update(&mut self, build: &mut dyn FnMut() -> TreeUpdate) {
        *self.pending.lock().unwrap() = Some(build());
    }
    fn set_focus(&mut self, _is_focused: bool) {}
    fn set_window_bounds(&mut self, _outer: Rect, _inner: Rect) {}
}

/// Drain side of a [`CapturingAccessibilityBackend`].
#[derive(Clone)]
pub struct CapturedAccessibility {
    pending: Arc<Mutex<Option<TreeUpdate>>>,
}

impl CapturedAccessibility {
    /// Consume the most recent captured [`TreeUpdate`], if any.
    pub fn take(&self) -> Option<TreeUpdate> {
        self.pending.lock().unwrap().take()
    }
}
