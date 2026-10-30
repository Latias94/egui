use super::*;
use crate::native::host_seam::{
    NativeEventSequencer, NativeHostHandler, NativeViewportCloseDecision,
    NativeViewportCloseRequest, NativeViewportCreateAdmission, NativeViewportVisibilityStatus,
    NativeWindowEvent, NativeWindowEventDisposition, resolve_native_viewport_close,
};
use std::sync::Mutex;

struct DeferringHost {
    context: egui::Context,
    retired: Mutex<Option<NativeViewportCloseRequest>>,
}

impl NativeHostHandler for DeferringHost {
    fn on_window_event(&self, event: NativeWindowEvent<'_>) -> NativeWindowEventDisposition {
        NativeWindowEventDisposition::HoldClose(event.close_request().unwrap())
    }

    fn begin_deferred_viewport_create(
        &self,
        _attempt: NativeViewportCreateAttempt,
    ) -> NativeViewportCreateAdmission {
        let retired = self.retired.lock().unwrap().unwrap();
        assert!(
            !resolve_native_viewport_close(
                &self.context,
                retired,
                NativeViewportCloseDecision::Allow
            ),
            "old close authority must retire before replacement admission can defer"
        );
        assert!(!resolve_native_viewport_close(
            &self.context,
            retired,
            NativeViewportCloseDecision::Veto
        ));
        NativeViewportCreateAdmission::Defer
    }
}

#[test]
fn builder_recreate_retires_close_and_maps_before_deferred_admission() {
    let context = egui::Context::default();
    let host = Arc::new(DeferringHost {
        context: context.clone(),
        retired: Mutex::new(None),
    });
    let native_host = NativeHostState::new(Some(host.clone()));
    let child = ViewportId::from_hash_of("recreated-held-child");
    let window = WindowId::from(20);
    let ids = ViewportIdPair::from_self_and_parent(child, ViewportId::ROOT);
    let mut viewports = OrderedViewportIdMap::default();
    let mut current = Some(child);
    let mut viewport_from_window = HashMap::default();
    let mut window_from_viewport = OrderedViewportIdMap::default();
    initialize_or_update_viewport(
        &mut viewports,
        &mut current,
        &mut ViewportRecreateCleanup {
            viewport_from_window: &mut viewport_from_window,
            window_from_viewport: &mut window_from_viewport,
            native_host: &native_host,
        },
        ids,
        ViewportClass::Deferred,
        ViewportBuilder::default().with_drag_and_drop(false),
        None,
    );
    // Exercise the renderer's retained mapping when no OS window is available
    // to the unit test. The next real creation-admission callback must already
    // observe retirement; no synthetic Destroyed callback is supplied.
    viewport_from_window.insert(window, child);
    window_from_viewport.insert(child, window);
    let mut sequence = NativeEventSequencer::default();
    let NativeWindowEventDisposition::HoldClose(request) = native_host.observe_window_event(
        Some(&context),
        sequence.next(),
        window,
        Some(child),
        None,
        &winit::event::WindowEvent::CloseRequested,
    ) else {
        panic!("the close must be held");
    };
    *host.retired.lock().unwrap() = Some(request);
    viewports
        .get_mut(&child)
        .unwrap()
        .info
        .events
        .push(egui::ViewportEvent::Close);

    initialize_or_update_viewport(
        &mut viewports,
        &mut current,
        &mut ViewportRecreateCleanup {
            viewport_from_window: &mut viewport_from_window,
            window_from_viewport: &mut window_from_viewport,
            native_host: &native_host,
        },
        ids,
        ViewportClass::Deferred,
        ViewportBuilder::default().with_drag_and_drop(true),
        None,
    );
    assert!(matches!(
        native_host.prepare_deferred_window(
            &context,
            NativeViewportVisibilityStatus::Dispatched,
            child,
            &viewports[&child].builder
        ),
        NativeDeferredWindowPreparation::Defer
    ));
    assert!(!native_host.is_close_held(child));
    assert!(viewport_from_window.is_empty());
    assert!(window_from_viewport.is_empty());
    assert!(viewports[&child].info.events.is_empty());
    assert_eq!(current, None);

    let successor = WindowId::from(21);
    viewport_from_window.insert(successor, child);
    window_from_viewport.insert(child, successor);
    native_host.observe_window_event(
        Some(&context),
        sequence.next(),
        successor,
        Some(child),
        None,
        &winit::event::WindowEvent::CloseRequested,
    );
    ViewportRecreateCleanup {
        viewport_from_window: &mut viewport_from_window,
        window_from_viewport: &mut window_from_viewport,
        native_host: &native_host,
    }
    .forget_replaced_window(child, window);
    assert_eq!(viewport_from_window.get(&successor), Some(&child));
    assert_eq!(window_from_viewport.get(&child), Some(&successor));
    assert!(native_host.is_close_held(child));
}
