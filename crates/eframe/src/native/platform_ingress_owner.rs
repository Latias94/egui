//! Winit-owned native platform ingress lifecycle.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    sync::Arc,
};

use egui::ViewportId;
use winit::window::{Window, WindowId};

use super::native_work_area_authority::NativeWorkAreaAuthority;
use super::platform_provider::NativeHostIngressSettlementKey;
use super::platform_provider::{
    NativeAccessibilityEdge, NativeAuthority, NativeBackendCapabilities, NativeCaptureOwner,
    NativeCloseState, NativeEffectDispatchOutcome, NativeEffectProperty, NativeEffectRequest,
    NativeFiniteScrollVector, NativeFocusedWindow, NativeHostIngress, NativeHoveredWindow,
    NativeKeyEdge, NativePhysicalPoint, NativePhysicalRect, NativePlatformError,
    NativePointerButton, NativePointerCoordinateCapture, NativePointerDeliveryOwner,
    NativePointerDeviceId, NativePointerEdgeFacts, NativePointerEdgeKind, NativePointerId,
    NativePointerIdentity, NativePointerInputState, NativePointerSource, NativePresentationState,
    NativeScrollCancelReason, NativeScrollDelta, NativeScrollEdge, NativeScrollModifiers,
    NativeScrollPhase, NativeScrollSequenceToken, NativeUnavailableReason, NativeViewportBinding,
    NativeWindowEffect, NativeWorkAreaRoute, ObservationAcknowledgement, PreparedNativeHostIngress,
    SharedNativePlatformCoordinator,
};
#[cfg(feature = "native-test-support")]
use super::test_support::{
    NativeTestPointerAction, NativeTestPointerEvent, NativeTestPointerLocation,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum WinitPointerStream {
    Mouse,
    Touch(u64),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct WinitPointerKey {
    device_id: winit::event::DeviceId,
    stream: WinitPointerStream,
}

#[derive(Clone, Debug)]
struct WinitPointerState {
    identity: NativePointerIdentity,
    source: NativePointerSource,
    position: NativeAuthority<NativePhysicalPoint>,
}

/// One deliberately single-pointer test stream.
///
/// The default-disabled test driver has no API for supplying a device, pointer, capture owner,
/// binding, or desktop coordinates. Keeping one state here lets the native event loop mint those
/// facts just as it does for a real winit pointer stream.
#[cfg(feature = "native-test-support")]
#[derive(Clone, Debug)]
struct NativeTestPointerState {
    identity: NativePointerIdentity,
    capture_owner: Option<NativeViewportBinding>,
}

#[derive(Clone, Debug)]
struct WinitPointerAuthority {
    delivery_owner: NativeAuthority<NativePointerDeliveryOwner>,
    hovered: NativeAuthority<NativeHoveredWindow>,
    hovered_coordinates: NativeAuthority<NativePointerCoordinateCapture>,
    delivery_coordinates: NativeAuthority<NativePointerCoordinateCapture>,
    work_area: NativeAuthority<NativeWorkAreaRoute>,
    capture: NativeAuthority<NativeCaptureOwner>,
}

#[derive(Clone, Copy, Debug)]
struct WinitScrollSample {
    device_id: winit::event::DeviceId,
    delta: winit::event::MouseScrollDelta,
    phase: winit::event::TouchPhase,
    modifiers: Option<winit::keyboard::ModifiersState>,
    position: Option<winit::dpi::PhysicalPosition<f64>>,
    backend_event_sequence: egui::BackendEventSequence,
}

#[derive(Clone, Debug)]
struct WinitWindowFacts {
    window_id: WindowId,
    viewport_id: ViewportId,
    content_rect: NativeAuthority<NativePhysicalRect>,
    outer_rect: NativeAuthority<NativePhysicalRect>,
    native_scale_factor: NativeAuthority<f64>,
    presentation_scale_factor: NativeAuthority<f64>,
    work_area: NativeAuthority<NativePhysicalRect>,
    presentation: NativeAuthority<NativePresentationState>,
    pointer_input: NativeAuthority<NativePointerInputState>,
    capabilities: NativeBackendCapabilities,
    focused: bool,
    native_staging_surface: bool,
}

impl WinitWindowFacts {
    fn read(viewport_id: ViewportId, window: &Window, egui_ctx: &egui::Context) -> Self {
        let visible = window.is_visible();
        let minimized = window.is_minimized();
        let inner_size = window.inner_size();
        let native = super::native_window_probe::probe(window);
        Self {
            window_id: window.id(),
            viewport_id,
            content_rect: physical_rect(&window.inner_position(), inner_size),
            outer_rect: physical_rect(&window.outer_position(), window.outer_size()),
            native_scale_factor: NativeAuthority::known(window.scale_factor()),
            presentation_scale_factor: NativeAuthority::known(f64::from(
                egui_winit::pixels_per_point(egui_ctx, window),
            )),
            work_area: native.work_area,
            presentation: presentation_state(visible, minimized),
            pointer_input: native.pointer_input,
            capabilities: super::native_window_probe::capabilities(window),
            focused: window.has_focus(),
            native_staging_surface: super::hosted_cycle::is_native_staging_surface(
                visible,
                minimized,
                inner_size.into(),
            ),
        }
    }
}

pub(super) struct FrozenNativePlatformIngress {
    ingress: NativeHostIngress,
    native_staging_presentations: Vec<NativeViewportBinding>,
    effect_sink: crate::NativeEffectSink,
    viewport_create_sink: crate::NativeViewportCreateSink,
    settlement: NativeHostIngressSettlement,
}

#[must_use = "a frozen native ingress batch must reach the hosted commit boundary"]
pub(super) struct NativeHostIngressSettlement {
    coordinator: SharedNativePlatformCoordinator,
    key: Option<NativeHostIngressSettlementKey>,
}

/// Prevalidated native-ingress watermark publication.
///
/// Dropping this value aborts the prepared ingress. Once application semantic
/// state publishes, [`Self::commit`] cannot report another recoverable error.
#[must_use = "a prepared native ingress commit must publish or abort"]
pub(super) struct PreparedNativeHostIngressCommit {
    coordinator: SharedNativePlatformCoordinator,
    key: Option<NativeHostIngressSettlementKey>,
}

impl NativeHostIngressSettlement {
    fn new(
        coordinator: SharedNativePlatformCoordinator,
        key: NativeHostIngressSettlementKey,
    ) -> Self {
        Self {
            coordinator,
            key: Some(key),
        }
    }

    pub(super) fn prepare_commit(
        mut self,
    ) -> Result<PreparedNativeHostIngressCommit, NativePlatformIngressError> {
        let key = self
            .key
            .expect("an affine native ingress settlement is consumed once");
        self.coordinator
            .lock()
            .validate_host_ingress_settlement(key)?;
        self.key
            .take()
            .expect("an affine native ingress settlement is consumed once");
        Ok(PreparedNativeHostIngressCommit {
            coordinator: self.coordinator.clone(),
            key: Some(key),
        })
    }
}

impl Drop for NativeHostIngressSettlement {
    fn drop(&mut self) {
        let Some(key) = self.key.take() else {
            return;
        };
        let aborted = self.coordinator.lock().abort_host_ingress(key);
        debug_assert!(
            aborted.is_ok(),
            "an unsettled native ingress ticket must poison its prepared batch"
        );
    }
}

impl PreparedNativeHostIngressCommit {
    pub(super) fn commit(mut self) {
        let key = self
            .key
            .take()
            .expect("a prepared native ingress commit is consumed once");
        self.coordinator
            .lock()
            .commit_host_ingress(key)
            .expect("a prevalidated affine native ingress commit remains current");
    }
}

impl Drop for PreparedNativeHostIngressCommit {
    fn drop(&mut self) {
        let Some(key) = self.key.take() else {
            return;
        };
        let aborted = self.coordinator.lock().abort_host_ingress(key);
        debug_assert!(
            aborted.is_ok(),
            "an unpublished prevalidated native ingress commit must poison its batch"
        );
    }
}

pub(super) struct NativeEffectDispatchBatch {
    destroyed_viewports: BTreeSet<ViewportId>,
    first_error: Option<NativePlatformIngressError>,
}

impl NativeEffectDispatchBatch {
    pub(super) fn into_parts(self) -> (BTreeSet<ViewportId>, Option<NativePlatformIngressError>) {
        (self.destroyed_viewports, self.first_error)
    }
}

impl FrozenNativePlatformIngress {
    pub(super) fn into_parts(
        self,
    ) -> (
        NativeHostIngress,
        Vec<NativeViewportBinding>,
        crate::NativeEffectSink,
        crate::NativeViewportCreateSink,
        NativeHostIngressSettlement,
    ) {
        (
            self.ingress,
            self.native_staging_presentations,
            self.effect_sink,
            self.viewport_create_sink,
            self.settlement,
        )
    }
}

/// Owns native binding incarnations and freezes one atomic ingress per host cycle.
pub(super) struct NativePlatformIngressOwner {
    coordinator: SharedNativePlatformCoordinator,
    window_bindings: HashMap<WindowId, NativeViewportBinding>,
    windows_by_viewport: BTreeMap<ViewportId, WindowId>,
    pointer_devices: HashMap<winit::event::DeviceId, NativePointerDeviceId>,
    pointer_states: HashMap<WinitPointerKey, WinitPointerState>,
    scroll_sequences: HashMap<WinitPointerKey, NativeScrollSequenceToken>,
    #[cfg(feature = "native-test-support")]
    test_pointer_state: Option<NativeTestPointerState>,
    dispatched_effects:
        BTreeMap<(NativeViewportBinding, NativeEffectProperty), NativeEffectRequest>,
    close_states: BTreeMap<NativeViewportBinding, NativeCloseState>,
    next_pointer_device_id: u64,
    next_pointer_id: u64,
    next_scroll_sequence: u64,
    hovered: NativeAuthority<NativeHoveredWindow>,
    capture: NativeAuthority<NativeCaptureOwner>,
    work_areas: NativeWorkAreaAuthority,
}

impl Default for NativePlatformIngressOwner {
    fn default() -> Self {
        Self {
            coordinator: Default::default(),
            window_bindings: Default::default(),
            windows_by_viewport: Default::default(),
            pointer_devices: Default::default(),
            pointer_states: Default::default(),
            scroll_sequences: Default::default(),
            #[cfg(feature = "native-test-support")]
            test_pointer_state: None,
            dispatched_effects: Default::default(),
            close_states: Default::default(),
            next_pointer_device_id: 0,
            next_pointer_id: 0,
            next_scroll_sequence: 0,
            hovered: NativeAuthority::unknown(NativeUnavailableReason::Unsupported),
            capture: NativeAuthority::unknown(NativeUnavailableReason::Unsupported),
            work_areas: NativeWorkAreaAuthority::default(),
        }
    }
}

impl NativePlatformIngressOwner {
    pub(super) fn coordinator(&self) -> SharedNativePlatformCoordinator {
        Arc::clone(&self.coordinator)
    }

    pub(super) fn active_binding(&self, viewport_id: ViewportId) -> Option<NativeViewportBinding> {
        self.coordinator.lock().active_binding(viewport_id)
    }

    /// Records a terminal native-resource failure for an accepted create request.
    ///
    /// Returns `false` for ordinary egui viewports that have no application-correlated
    /// creation request.
    pub(super) fn fail_viewport_create(
        &mut self,
        viewport_id: ViewportId,
    ) -> Result<bool, NativePlatformError> {
        self.coordinator
            .lock()
            .fail_scheduled_viewport_create(viewport_id)
    }

    pub(super) fn dispatch_effects(
        &mut self,
        sink: &crate::NativeEffectSink,
        windows: impl IntoIterator<Item = (ViewportId, Arc<Window>)>,
    ) -> NativeEffectDispatchBatch {
        let windows = windows
            .into_iter()
            .map(|(_, window)| (window.id(), window))
            .collect::<HashMap<_, _>>();
        let requests = sink.close_and_drain();
        let mut first_error: Option<NativePlatformIngressError> = None;
        let mut destroyed_viewports = BTreeSet::new();
        for request in requests {
            let (outcome, destroyed_viewport) = self.dispatch_effect(&request, &windows);
            if outcome == NativeEffectDispatchOutcome::Dispatched
                && let Err(error) = self.supersede_dispatched_effect(&request)
                && first_error.is_none()
            {
                first_error = Some(error);
            }
            let report = self
                .coordinator
                .lock()
                .report_effect_dispatch(&request, outcome);
            match report {
                Ok(()) if outcome == NativeEffectDispatchOutcome::Dispatched => {
                    if let Some(viewport) = destroyed_viewport {
                        destroyed_viewports.insert(viewport);
                    }
                    if matches!(request.effect(), NativeWindowEffect::CancelClose) {
                        let binding = request.binding();
                        let observed = self
                            .coordinator
                            .lock()
                            .record_close_observation_after_effect(
                                binding,
                                NativeCloseState::LiveClear,
                                &request,
                            );
                        match observed {
                            Ok(_) => {
                                self.close_states
                                    .insert(binding, NativeCloseState::LiveClear);
                            }
                            Err(error) if first_error.is_none() => {
                                first_error = Some(error.into());
                            }
                            Err(_) => {}
                        }
                    } else {
                        self.dispatched_effects
                            .insert((request.binding(), request.property()), request);
                    }
                }
                Ok(()) => {}
                Err(error) if first_error.is_none() => first_error = Some(error.into()),
                Err(_) => {}
            }
        }
        NativeEffectDispatchBatch {
            destroyed_viewports,
            first_error,
        }
    }

    /// Marks the prior observable request in this exact property lane indeterminate.
    ///
    /// Winit can dispatch two same-property operations before the next complete platform
    /// snapshot. Only the most recent operation can then receive an exact state
    /// acknowledgement; retaining the predecessor would either attach the observation to the
    /// wrong request or leave it pending forever.
    fn supersede_dispatched_effect(
        &mut self,
        request: &NativeEffectRequest,
    ) -> Result<(), NativePlatformIngressError> {
        let key = (request.binding(), request.property());
        let Some(previous) = self.dispatched_effects.get(&key) else {
            return Ok(());
        };
        self.coordinator
            .lock()
            .report_effect_acknowledgement_lost(previous)?;
        self.dispatched_effects.remove(&key);
        Ok(())
    }

    pub(super) fn record_window_event(
        &mut self,
        window_id: WindowId,
        window: &Window,
        route_windows: &[(ViewportId, Arc<Window>)],
        egui_ctx: &egui::Context,
        event: &winit::event::WindowEvent,
        backend_event_sequence: egui::BackendEventSequence,
    ) -> Result<(), NativePlatformIngressError> {
        let Some(binding) = self.window_bindings.get(&window_id).copied() else {
            return Ok(());
        };
        if window.id() != window_id || self.active_binding(binding.viewport_id()) != Some(binding) {
            return Err(NativePlatformIngressError::IncompleteRoster);
        }

        let bound_route_windows = route_windows
            .iter()
            .filter_map(|(viewport_id, window)| {
                self.active_binding(*viewport_id)
                    .map(|binding| (binding, Arc::clone(window)))
            })
            .collect::<Vec<_>>();
        if bound_route_windows.len() != self.window_bindings.len() {
            return Err(NativePlatformIngressError::IncompleteRoster);
        }
        let pointer_route =
            super::native_pointer_probe::probe_event(binding, window, &bound_route_windows, event);
        if pointer_route.observed_pointer_event {
            self.work_areas.observe_window(window)?;
        }

        match event {
            winit::event::WindowEvent::KeyboardInput {
                event,
                is_synthetic,
                ..
            } => {
                let presentation = egui_ctx.presented_pointer_hit_graph_for(binding.viewport_id());
                if let Some(edge) =
                    NativeKeyEdge::from_winit_event(binding, event, *is_synthetic, presentation)
                {
                    self.coordinator
                        .lock()
                        .record_key_edge_for_backend(backend_event_sequence, edge)?;
                }
            }
            winit::event::WindowEvent::CursorEntered { .. } => {
                self.hovered = NativeAuthority::known(NativeHoveredWindow::Viewport(binding));
            }
            winit::event::WindowEvent::CursorLeft { .. } => {
                self.hovered = NativeAuthority::unknown(NativeUnavailableReason::NotObserved);
            }
            winit::event::WindowEvent::CursorMoved {
                device_id,
                position,
            } => {
                let key = WinitPointerKey {
                    device_id: *device_id,
                    stream: WinitPointerStream::Mouse,
                };
                let position = desktop_pointer_position(window, *position);
                let hovered_coordinates = pointer_coordinate_capture(
                    &bound_route_windows,
                    &pointer_route.hovered,
                    &position,
                    egui_ctx,
                );
                let delivery_coordinates = pointer_delivery_coordinate_capture(
                    &bound_route_windows,
                    &pointer_route.delivery_owner,
                    &position,
                    egui_ctx,
                );
                let work_area = self.event_work_area(&pointer_route.hovered, &position);
                self.record_pointer_event(
                    key,
                    NativePointerSource::Viewport(binding),
                    NativePointerEdgeKind::Moved,
                    false,
                    position,
                    WinitPointerAuthority {
                        delivery_owner: pointer_route.delivery_owner.clone(),
                        hovered: pointer_route.hovered.clone(),
                        hovered_coordinates,
                        delivery_coordinates,
                        work_area,
                        capture: pointer_route.capture.clone(),
                    },
                    backend_event_sequence,
                )?;
            }
            winit::event::WindowEvent::MouseInput {
                device_id,
                state,
                button,
            } => {
                let key = WinitPointerKey {
                    device_id: *device_id,
                    stream: WinitPointerStream::Mouse,
                };
                let source = NativePointerSource::Viewport(binding);
                let position = self.retained_pointer_position(key, source);
                let button = native_mouse_button(*button);
                let kind = match state {
                    winit::event::ElementState::Pressed => {
                        NativePointerEdgeKind::ButtonPressed(button)
                    }
                    winit::event::ElementState::Released => {
                        NativePointerEdgeKind::ButtonReleased(button)
                    }
                };
                let hovered_coordinates = pointer_coordinate_capture(
                    &bound_route_windows,
                    &pointer_route.hovered,
                    &position,
                    egui_ctx,
                );
                let delivery_coordinates = pointer_delivery_coordinate_capture(
                    &bound_route_windows,
                    &pointer_route.delivery_owner,
                    &position,
                    egui_ctx,
                );
                let work_area = self.event_work_area(&pointer_route.hovered, &position);
                self.record_pointer_event(
                    key,
                    source,
                    kind,
                    false,
                    position,
                    WinitPointerAuthority {
                        delivery_owner: pointer_route.delivery_owner.clone(),
                        hovered: pointer_route.hovered.clone(),
                        hovered_coordinates,
                        delivery_coordinates,
                        work_area,
                        capture: pointer_route.capture.clone(),
                    },
                    backend_event_sequence,
                )?;
            }
            winit::event::WindowEvent::MouseWheel {
                device_id,
                delta,
                phase,
                modifiers,
                position,
            } => {
                self.record_winit_scroll_event(
                    binding,
                    window,
                    &bound_route_windows,
                    &pointer_route,
                    egui_ctx,
                    WinitScrollSample {
                        device_id: *device_id,
                        delta: *delta,
                        phase: *phase,
                        modifiers: *modifiers,
                        position: *position,
                        backend_event_sequence,
                    },
                )?;
            }
            winit::event::WindowEvent::PanGesture {
                device_id,
                delta,
                phase,
                modifiers,
                position,
            } => {
                self.record_winit_scroll_event(
                    binding,
                    window,
                    &bound_route_windows,
                    &pointer_route,
                    egui_ctx,
                    WinitScrollSample {
                        device_id: *device_id,
                        delta: winit::event::MouseScrollDelta::PixelDelta(
                            winit::dpi::PhysicalPosition::new(
                                f64::from(delta.x),
                                f64::from(delta.y),
                            ),
                        ),
                        phase: *phase,
                        modifiers: *modifiers,
                        position: *position,
                        backend_event_sequence,
                    },
                )?;
            }
            winit::event::WindowEvent::Touch(touch) => {
                let key = WinitPointerKey {
                    device_id: touch.device_id,
                    stream: WinitPointerStream::Touch(touch.id),
                };
                let kind = match touch.phase {
                    winit::event::TouchPhase::Started => {
                        NativePointerEdgeKind::ButtonPressed(NativePointerButton::Primary)
                    }
                    winit::event::TouchPhase::Moved => NativePointerEdgeKind::Moved,
                    winit::event::TouchPhase::Ended => {
                        NativePointerEdgeKind::ButtonReleased(NativePointerButton::Primary)
                    }
                    winit::event::TouchPhase::Cancelled => NativePointerEdgeKind::Cancelled,
                };
                let position = desktop_pointer_position(window, touch.location);
                let hovered_coordinates = pointer_coordinate_capture(
                    &bound_route_windows,
                    &pointer_route.hovered,
                    &position,
                    egui_ctx,
                );
                let delivery_coordinates = pointer_delivery_coordinate_capture(
                    &bound_route_windows,
                    &pointer_route.delivery_owner,
                    &position,
                    egui_ctx,
                );
                let work_area = self.event_work_area(&pointer_route.hovered, &position);
                self.record_pointer_event(
                    key,
                    NativePointerSource::Viewport(binding),
                    kind,
                    touch.phase == winit::event::TouchPhase::Ended,
                    position,
                    WinitPointerAuthority {
                        delivery_owner: pointer_route.delivery_owner.clone(),
                        hovered: pointer_route.hovered.clone(),
                        hovered_coordinates,
                        delivery_coordinates,
                        work_area,
                        capture: pointer_route.capture.clone(),
                    },
                    backend_event_sequence,
                )?;
                if matches!(
                    touch.phase,
                    winit::event::TouchPhase::Ended | winit::event::TouchPhase::Cancelled
                ) {
                    self.pointer_states.remove(&key);
                }
            }
            winit::event::WindowEvent::CloseRequested
                if binding.viewport_id() != ViewportId::ROOT =>
            {
                self.coordinator
                    .lock()
                    .record_close_observation_for_backend(
                        backend_event_sequence,
                        binding,
                        NativeCloseState::LiveRequested,
                        ObservationAcknowledgement::Baseline,
                    )?;
                self.close_states
                    .insert(binding, NativeCloseState::LiveRequested);
            }
            _ => {}
        }
        if pointer_route.observed_pointer_event {
            self.hovered = pointer_route.hovered;
            self.capture = pointer_route.capture_after;
        }
        Ok(())
    }

    /// Records one deterministic pointer edge through the same coordinator path as winit input.
    ///
    /// The test driver supplies only a viewport-local logical point or an asserted outside-all
    /// desktop point plus an action. The event loop resolves the live binding, complete native
    /// roster, presented hit graph, coordinate capture, work-area route, and capture owner. It
    /// rejects unavailable facts instead of accepting test-supplied native authority.
    #[cfg(feature = "native-test-support")]
    pub(super) fn record_native_test_pointer_event(
        &mut self,
        event: NativeTestPointerEvent,
        route_windows: &[(ViewportId, Arc<Window>)],
        egui_ctx: &egui::Context,
        backend_event_sequence: egui::BackendEventSequence,
    ) -> Result<ViewportId, NativePlatformIngressError> {
        let bound_route_windows = route_windows
            .iter()
            .filter_map(|(candidate, window)| {
                self.active_binding(*candidate)
                    .map(|binding| (binding, Arc::clone(window)))
            })
            .collect::<Vec<_>>();
        if bound_route_windows.len() != self.window_bindings.len()
            || bound_route_windows
                .iter()
                .map(|(binding, _)| *binding)
                .collect::<BTreeSet<_>>()
                .len()
                != bound_route_windows.len()
        {
            return Err(NativePlatformIngressError::IncompleteRoster);
        }
        let Some((_, first_window)) = bound_route_windows.first() else {
            return Err(NativePlatformIngressError::IncompleteRoster);
        };
        self.work_areas.observe_window(first_window)?;

        let viewport_location = match event.location() {
            NativeTestPointerLocation::Viewport { viewport, position } => {
                Some((viewport, position))
            }
            NativeTestPointerLocation::UniqueChild { position } => {
                let children = bound_route_windows
                    .iter()
                    .filter(|(binding, _)| binding.viewport_id() != ViewportId::ROOT)
                    .collect::<Vec<_>>();
                let [(binding, _)] = children.as_slice() else {
                    return Err(NativePlatformIngressError::IncompleteRoster);
                };
                Some((binding.viewport_id(), position))
            }
            NativeTestPointerLocation::OutsideAll { .. } => None,
        };
        let (
            repaint_viewport,
            source_binding,
            uncaptured_source,
            uncaptured_delivery_owner,
            hovered,
            hovered_coordinates,
            position,
            work_area,
        ) = if let Some((viewport, position)) = viewport_location {
            let Some((binding, window)) = bound_route_windows.iter().find(|(binding, _)| {
                binding.viewport_id() == viewport && self.active_binding(viewport) == Some(*binding)
            }) else {
                return Err(NativePlatformIngressError::IncompleteRoster);
            };
            let Some(presentation) = egui_ctx.presented_pointer_hit_graph_for(viewport) else {
                return Err(NativePlatformIngressError::NativeTestPointerUnavailable(
                    viewport,
                    NativeUnavailableReason::NotObserved,
                ));
            };
            let (desktop_position, coordinates, presented_position) =
                native_test_pointer_coordinates(window, *binding, &presentation, position).ok_or(
                    NativePlatformIngressError::NativeTestPointerUnavailable(
                        viewport,
                        NativeUnavailableReason::Unsupported,
                    ),
                )?;
            let Some(point) = desktop_position.value().copied() else {
                unreachable!("native test coordinate construction returns known desktop position")
            };
            if native_test_owned_viewports_contain(&bound_route_windows, Some(*binding), point)
                != Some(false)
            {
                return Err(NativePlatformIngressError::NativeTestPointerUnavailable(
                    viewport,
                    NativeUnavailableReason::StaleSource,
                ));
            }
            if !matches!(
                presentation.probe(presented_position),
                egui::PointerReceiverAuthority::Known(_)
            ) {
                return Err(NativePlatformIngressError::NativeTestPointerUnavailable(
                    viewport,
                    NativeUnavailableReason::NotObserved,
                ));
            }
            (
                viewport,
                Some(*binding),
                NativePointerSource::Viewport(*binding),
                NativeAuthority::known(NativePointerDeliveryOwner::Viewport(*binding)),
                NativeAuthority::known(NativeHoveredWindow::Viewport(*binding)),
                coordinates,
                desktop_position,
                NativeAuthority::unknown(NativeUnavailableReason::NotObserved),
            )
        } else {
            let NativeTestPointerLocation::OutsideAll { desktop_position } = event.location()
            else {
                unreachable!("only an outside-all location lacks a viewport target")
            };
            let Some(desktop_position) = native_test_desktop_point(desktop_position) else {
                return Err(NativePlatformIngressError::NativeTestPointerUnavailable(
                    ViewportId::ROOT,
                    NativeUnavailableReason::Unsupported,
                ));
            };
            if native_test_owned_viewports_contain(&bound_route_windows, None, desktop_position)
                != Some(false)
            {
                return Err(NativePlatformIngressError::NativeTestPointerUnavailable(
                    ViewportId::ROOT,
                    NativeUnavailableReason::Unsupported,
                ));
            }
            let desktop_position = NativeAuthority::known(desktop_position);
            (
                ViewportId::ROOT,
                None,
                NativePointerSource::None,
                NativeAuthority::known(NativePointerDeliveryOwner::None),
                NativeAuthority::known(NativeHoveredWindow::None),
                NativeAuthority::unknown(NativeUnavailableReason::NotObserved),
                desktop_position.clone(),
                self.work_areas.route(&desktop_position),
            )
        };

        let mut state = match self.test_pointer_state.clone() {
            Some(state) => state,
            None => NativeTestPointerState {
                identity: self.next_native_test_pointer_identity()?,
                capture_owner: None,
            },
        };
        if let Some(capture_owner) = state.capture_owner
            && self.active_binding(capture_owner.viewport_id()) != Some(capture_owner)
        {
            self.test_pointer_state = None;
            return Err(NativePlatformIngressError::NativeTestPointerUnavailable(
                repaint_viewport,
                NativeUnavailableReason::StaleSource,
            ));
        }

        let (kind, capture_after) = match event.action() {
            NativeTestPointerAction::Move => (NativePointerEdgeKind::Moved, state.capture_owner),
            NativeTestPointerAction::PrimaryPressed
                if state.capture_owner.is_none() && source_binding.is_some() =>
            {
                (
                    NativePointerEdgeKind::ButtonPressed(NativePointerButton::Primary),
                    source_binding,
                )
            }
            NativeTestPointerAction::PrimaryReleased if state.capture_owner.is_some() => (
                NativePointerEdgeKind::ButtonReleased(NativePointerButton::Primary),
                None,
            ),
            _ => {
                return Err(
                    NativePlatformIngressError::NativeTestPointerActionOutOfOrder(event.action()),
                );
            }
        };
        let capture_authority_after = NativeAuthority::known(
            capture_after.map_or(NativeCaptureOwner::None, NativeCaptureOwner::Viewport),
        );
        let (source, delivery_owner) = state.capture_owner.map_or(
            (uncaptured_source, uncaptured_delivery_owner),
            |capture_owner| {
                (
                    NativePointerSource::Viewport(capture_owner),
                    NativeAuthority::known(NativePointerDeliveryOwner::Viewport(capture_owner)),
                )
            },
        );
        let authority = WinitPointerAuthority {
            delivery_coordinates: pointer_delivery_coordinate_capture(
                &bound_route_windows,
                &delivery_owner,
                &position,
                egui_ctx,
            ),
            delivery_owner,
            hovered,
            hovered_coordinates,
            work_area,
            capture: capture_authority_after,
        };
        let facts = NativePointerEdgeFacts::new(
            source,
            authority.delivery_owner,
            state.identity,
            kind,
            position,
            authority.hovered,
            authority.capture,
        )
        .with_hovered_coordinates(authority.hovered_coordinates)
        .with_delivery_coordinates(authority.delivery_coordinates)
        .with_work_area(authority.work_area);
        self.coordinator
            .lock()
            .record_pointer_edge_without_derivative_for_backend(backend_event_sequence, facts)?;

        state.capture_owner = capture_after;
        self.test_pointer_state = Some(state);
        self.hovered = hovered;
        self.capture = NativeAuthority::known(
            capture_after.map_or(NativeCaptureOwner::None, NativeCaptureOwner::Viewport),
        );
        Ok(repaint_viewport)
    }

    fn record_winit_scroll_event(
        &mut self,
        binding: NativeViewportBinding,
        window: &Window,
        bound_route_windows: &[(NativeViewportBinding, Arc<Window>)],
        pointer_route: &super::native_pointer_probe::NativePointerEventRoute,
        egui_ctx: &egui::Context,
        sample: WinitScrollSample,
    ) -> Result<(), NativePlatformIngressError> {
        let key = WinitPointerKey {
            device_id: sample.device_id,
            stream: WinitPointerStream::Mouse,
        };
        let source = NativePointerSource::Viewport(binding);
        let identity = self.pointer_identity(key)?;
        let scroll = self.native_scroll_edge(
            key,
            identity.device_id(),
            sample.delta,
            sample.phase,
            sample.modifiers.map(native_scroll_modifiers),
        )?;
        let position = sample.position.map_or_else(
            || NativeAuthority::unknown(NativeUnavailableReason::NotObserved),
            |position| desktop_pointer_position(window, position),
        );
        let hovered_coordinates = pointer_coordinate_capture(
            bound_route_windows,
            &pointer_route.hovered,
            &position,
            egui_ctx,
        );
        let delivery_coordinates = pointer_delivery_coordinate_capture(
            bound_route_windows,
            &pointer_route.delivery_owner,
            &position,
            egui_ctx,
        );
        let work_area = self.event_work_area(&pointer_route.hovered, &position);
        self.record_pointer_facts(
            source,
            NativePointerEdgeKind::Scrolled(scroll),
            false,
            position,
            WinitPointerAuthority {
                delivery_owner: pointer_route.delivery_owner,
                hovered: pointer_route.hovered,
                hovered_coordinates,
                delivery_coordinates,
                work_area,
                capture: pointer_route.capture,
            },
            identity,
            sample.backend_event_sequence,
        )?;
        self.pointer_states.entry(key).or_insert(WinitPointerState {
            identity,
            source,
            position,
        });
        Ok(())
    }

    fn retained_pointer_position(
        &self,
        key: WinitPointerKey,
        source: NativePointerSource,
    ) -> NativeAuthority<NativePhysicalPoint> {
        self.pointer_states
            .get(&key)
            .filter(|state| state.source == source)
            .map_or_else(
                || NativeAuthority::unknown(NativeUnavailableReason::NotObserved),
                |state| state.position.clone(),
            )
    }

    #[cfg(feature = "accesskit")]
    pub(super) fn record_accesskit_event(
        &mut self,
        window_id: WindowId,
        egui_ctx: &egui::Context,
        event: &egui_winit::accesskit_winit::WindowEvent,
        backend_event_sequence: egui::BackendEventSequence,
    ) -> Result<(), NativePlatformIngressError> {
        let Some(binding) = self.window_bindings.get(&window_id).copied() else {
            return Ok(());
        };
        if self.active_binding(binding.viewport_id()) != Some(binding) {
            return Err(NativePlatformIngressError::IncompleteRoster);
        }
        let egui_winit::accesskit_winit::WindowEvent::ActionRequested(request) = event else {
            return Ok(());
        };
        let presentation = egui_ctx.presented_pointer_hit_graph_for(binding.viewport_id());
        let Some(edge) =
            NativeAccessibilityEdge::from_accesskit_request(binding, request, presentation)
        else {
            return Ok(());
        };
        self.coordinator
            .lock()
            .record_accessibility_edge_for_backend(backend_event_sequence, edge)?;
        Ok(())
    }

    pub(super) fn record_device_event(
        &mut self,
        device_id: winit::event::DeviceId,
        event: &winit::event::DeviceEvent,
        backend_event_sequence: egui::BackendEventSequence,
    ) -> Result<(), NativePlatformIngressError> {
        if !matches!(event, winit::event::DeviceEvent::Removed) {
            return Ok(());
        }
        let keys = self
            .pointer_states
            .keys()
            .filter(|key| key.device_id == device_id)
            .copied()
            .collect::<Vec<_>>();
        for key in keys {
            let Some(state) = self.pointer_states.remove(&key) else {
                continue;
            };
            self.coordinator.lock().record_pointer_edge_for_backend(
                backend_event_sequence,
                NativePointerEdgeFacts::new(
                    state.source,
                    NativeAuthority::known(NativePointerDeliveryOwner::None),
                    state.identity,
                    NativePointerEdgeKind::Cancelled,
                    state.position,
                    NativeAuthority::unknown(NativeUnavailableReason::Retired),
                    NativeAuthority::unknown(NativeUnavailableReason::Retired),
                ),
            )?;
        }
        self.scroll_sequences
            .retain(|key, _| key.device_id != device_id);
        self.pointer_devices.remove(&device_id);
        Ok(())
    }

    fn record_pointer_event(
        &mut self,
        key: WinitPointerKey,
        source: NativePointerSource,
        kind: NativePointerEdgeKind,
        stream_terminal: bool,
        position: NativeAuthority<NativePhysicalPoint>,
        authority: WinitPointerAuthority,
        backend_event_sequence: egui::BackendEventSequence,
    ) -> Result<(), NativePlatformIngressError> {
        let identity = self.pointer_identity(key)?;
        self.record_pointer_facts(
            source,
            kind,
            stream_terminal,
            position.clone(),
            authority,
            identity,
            backend_event_sequence,
        )?;
        self.pointer_states.insert(
            key,
            WinitPointerState {
                identity,
                source,
                position,
            },
        );
        Ok(())
    }

    fn record_pointer_facts(
        &self,
        source: NativePointerSource,
        kind: NativePointerEdgeKind,
        stream_terminal: bool,
        position: NativeAuthority<NativePhysicalPoint>,
        authority: WinitPointerAuthority,
        identity: NativePointerIdentity,
        backend_event_sequence: egui::BackendEventSequence,
    ) -> Result<(), NativePlatformIngressError> {
        let mut facts = NativePointerEdgeFacts::new(
            source,
            authority.delivery_owner,
            identity,
            kind,
            position,
            authority.hovered,
            authority.capture,
        )
        .with_hovered_coordinates(authority.hovered_coordinates)
        .with_delivery_coordinates(authority.delivery_coordinates)
        .with_work_area(authority.work_area);
        if stream_terminal {
            facts = facts.ending_stream();
        }
        self.coordinator
            .lock()
            .record_pointer_edge_for_backend(backend_event_sequence, facts)?;
        Ok(())
    }

    fn native_scroll_edge(
        &mut self,
        key: WinitPointerKey,
        device: NativePointerDeviceId,
        delta: winit::event::MouseScrollDelta,
        phase: winit::event::TouchPhase,
        modifiers: Option<NativeScrollModifiers>,
    ) -> Result<NativeScrollEdge, NativePlatformIngressError> {
        let delta = native_scroll_delta(delta)?;
        let (sequence, phase) = match phase {
            winit::event::TouchPhase::Started => {
                if self.scroll_sequences.contains_key(&key) {
                    return Err(NativePlatformIngressError::ScrollSequenceOutOfOrder(phase));
                }
                self.next_scroll_sequence = self
                    .next_scroll_sequence
                    .checked_add(1)
                    .ok_or(NativePlatformError::CounterExhausted)?;
                let sequence = NativeScrollSequenceToken::new(self.next_scroll_sequence);
                self.scroll_sequences.insert(key, sequence);
                (Some(sequence), NativeScrollPhase::Begin)
            }
            winit::event::TouchPhase::Moved => self
                .scroll_sequences
                .get(&key)
                .copied()
                .map_or((None, NativeScrollPhase::Discrete), |sequence| {
                    (Some(sequence), NativeScrollPhase::Update)
                }),
            winit::event::TouchPhase::Ended => (
                Some(
                    self.scroll_sequences
                        .remove(&key)
                        .ok_or(NativePlatformIngressError::ScrollSequenceOutOfOrder(phase))?,
                ),
                NativeScrollPhase::End,
            ),
            winit::event::TouchPhase::Cancelled => (
                Some(
                    self.scroll_sequences
                        .remove(&key)
                        .ok_or(NativePlatformIngressError::ScrollSequenceOutOfOrder(phase))?,
                ),
                NativeScrollPhase::Cancel(NativeScrollCancelReason::PlatformCancelled),
            ),
        };
        let delta = (!matches!(phase, NativeScrollPhase::Cancel(_))).then_some(delta);
        NativeScrollEdge::new(
            device,
            sequence,
            phase,
            delta,
            NativeAuthority::unknown(NativeUnavailableReason::NotObserved),
            modifiers.map_or_else(
                || NativeAuthority::unknown(NativeUnavailableReason::NotObserved),
                NativeAuthority::known,
            ),
        )
        .ok_or(NativePlatformIngressError::InvalidScrollEdge)
    }

    #[cfg(feature = "native-test-support")]
    fn next_native_test_pointer_identity(
        &mut self,
    ) -> Result<NativePointerIdentity, NativePlatformIngressError> {
        self.next_pointer_device_id = self
            .next_pointer_device_id
            .checked_add(1)
            .ok_or(NativePlatformError::CounterExhausted)?;
        self.next_pointer_id = self
            .next_pointer_id
            .checked_add(1)
            .ok_or(NativePlatformError::CounterExhausted)?;
        Ok(NativePointerIdentity::new(
            NativePointerDeviceId::new(self.next_pointer_device_id),
            NativePointerId::new(self.next_pointer_id),
        ))
    }

    fn event_work_area(
        &self,
        hovered: &NativeAuthority<NativeHoveredWindow>,
        position: &NativeAuthority<NativePhysicalPoint>,
    ) -> NativeAuthority<NativeWorkAreaRoute> {
        match hovered.value() {
            Some(NativeHoveredWindow::None) => self.work_areas.route(position),
            Some(NativeHoveredWindow::Viewport(_) | NativeHoveredWindow::Foreign) => {
                NativeAuthority::unknown(NativeUnavailableReason::NotObserved)
            }
            None => NativeAuthority::unknown(
                hovered
                    .unavailable_reason()
                    .unwrap_or(NativeUnavailableReason::NotObserved),
            ),
        }
    }

    fn pointer_identity(
        &mut self,
        key: WinitPointerKey,
    ) -> Result<NativePointerIdentity, NativePlatformIngressError> {
        if let Some(state) = self.pointer_states.get(&key) {
            return Ok(state.identity);
        }
        let device_id = match self.pointer_devices.get(&key.device_id).copied() {
            Some(device_id) => device_id,
            None => {
                self.next_pointer_device_id = self
                    .next_pointer_device_id
                    .checked_add(1)
                    .ok_or(NativePlatformError::CounterExhausted)?;
                let device_id = NativePointerDeviceId::new(self.next_pointer_device_id);
                self.pointer_devices.insert(key.device_id, device_id);
                device_id
            }
        };
        self.next_pointer_id = self
            .next_pointer_id
            .checked_add(1)
            .ok_or(NativePlatformError::CounterExhausted)?;
        Ok(NativePointerIdentity::new(
            device_id,
            NativePointerId::new(self.next_pointer_id),
        ))
    }

    fn dispatch_effect(
        &self,
        request: &NativeEffectRequest,
        windows: &HashMap<WindowId, Arc<Window>>,
    ) -> (NativeEffectDispatchOutcome, Option<ViewportId>) {
        let binding = request.binding();
        if self.active_binding(binding.viewport_id()) != Some(binding) {
            return (NativeEffectDispatchOutcome::Rejected, None);
        }
        let Some(window_id) = self.windows_by_viewport.get(&binding.viewport_id()) else {
            return (NativeEffectDispatchOutcome::Rejected, None);
        };
        if self.window_bindings.get(window_id) != Some(&binding) {
            return (NativeEffectDispatchOutcome::Rejected, None);
        }
        let Some(window) = windows.get(window_id) else {
            return (NativeEffectDispatchOutcome::Rejected, None);
        };

        if let Some(dispatch) = destroy_dispatch(binding.viewport_id(), request.effect()) {
            return dispatch;
        }
        if binding.viewport_id() == ViewportId::ROOT
            && matches!(request.effect(), NativeWindowEffect::CancelClose)
        {
            return (NativeEffectDispatchOutcome::Unsupported, None);
        }

        (dispatch_window_effect(window, request.effect()), None)
    }

    /// Reads the exact current window roster and freezes its complete native facts.
    pub(super) fn freeze(
        &mut self,
        windows: impl IntoIterator<Item = (ViewportId, Arc<Window>)>,
        egui_ctx: &egui::Context,
    ) -> Result<FrozenNativePlatformIngress, NativePlatformIngressError> {
        let windows = windows.into_iter().collect::<Vec<_>>();
        self.continue_dispatched_geometry(&windows)?;
        if let Some((_, window)) = windows.first() {
            self.work_areas.observe_window(window)?;
        } else {
            self.work_areas
                .observe_unavailable(NativeUnavailableReason::NotObserved)?;
        }
        let facts = windows
            .iter()
            .map(|(viewport_id, window)| WinitWindowFacts::read(*viewport_id, window, egui_ctx))
            .collect::<Vec<_>>();
        self.reconcile_roster(&facts)?;
        let bound_windows = facts
            .iter()
            .zip(&windows)
            .map(|(facts, (_, window))| Ok((self.binding_for(facts)?, Arc::clone(window))))
            .collect::<Result<Vec<_>, NativePlatformIngressError>>()?;
        let pointer_route = super::native_pointer_probe::probe(&bound_windows);
        self.hovered = pointer_route.hovered;
        self.capture = pointer_route.capture;
        let prepared_ingress = self.prepare_facts(&facts)?;
        let (ingress, settlement_key) = prepared_ingress.into_parts();
        let settlement = NativeHostIngressSettlement::new(self.coordinator(), settlement_key);
        let native_staging_presentations = facts
            .iter()
            .filter(|facts| facts.native_staging_surface)
            .map(|facts| self.binding_for(facts))
            .collect::<Result<Vec<_>, _>>()?;
        let effect_sink = crate::NativeEffectSink::new(
            self.coordinator(),
            ingress.platform().inventory().iter().copied(),
        );
        let viewport_create_sink = crate::NativeViewportCreateSink::new(
            self.coordinator(),
            ingress.platform().inventory().iter().copied(),
        );
        Ok(FrozenNativePlatformIngress {
            ingress,
            native_staging_presentations,
            effect_sink,
            viewport_create_sink,
            settlement,
        })
    }

    fn continue_dispatched_geometry(
        &mut self,
        windows: &[(ViewportId, Arc<Window>)],
    ) -> Result<(), NativePlatformIngressError> {
        let windows = windows
            .iter()
            .map(|(_, window)| (window.id(), window))
            .collect::<HashMap<_, _>>();
        let mut terminal = Vec::new();
        for (key, request) in &self.dispatched_effects {
            if request.property() != NativeEffectProperty::Geometry {
                continue;
            }
            let binding = request.binding();
            let Some(window_id) = self.windows_by_viewport.get(&binding.viewport_id()) else {
                continue;
            };
            if self.window_bindings.get(window_id) != Some(&binding) {
                continue;
            }
            let Some(window) = windows.get(window_id) else {
                continue;
            };
            if dispatch_window_effect(window, request.effect())
                != NativeEffectDispatchOutcome::Dispatched
            {
                self.coordinator
                    .lock()
                    .report_effect_acknowledgement_lost(request)?;
                terminal.push(*key);
            }
        }
        for key in terminal {
            self.dispatched_effects.remove(&key);
        }
        Ok(())
    }

    fn prepare_facts(
        &mut self,
        facts: &[WinitWindowFacts],
    ) -> Result<PreparedNativeHostIngress, NativePlatformIngressError> {
        self.reconcile_roster(facts)?;

        let focused_bindings = facts
            .iter()
            .filter(|facts| facts.focused)
            .map(|facts| self.binding_for(facts))
            .collect::<Result<Vec<_>, _>>()?;
        let focused = match focused_bindings.as_slice() {
            [binding] => NativeAuthority::known(NativeFocusedWindow::Viewport(*binding)),
            [] => NativeAuthority::unknown(NativeUnavailableReason::Unsupported),
            _ => NativeAuthority::unknown(NativeUnavailableReason::StaleSource),
        };
        let focus_roster_is_consistent = focused_bindings.len() <= 1;

        for facts in facts {
            self.record_window_facts(facts, focus_roster_is_consistent)?;
        }
        let capabilities = facts
            .iter()
            .map(|facts| facts.capabilities)
            .reduce(NativeBackendCapabilities::intersect)
            .unwrap_or_default();
        self.coordinator
            .lock()
            .record_platform_facts_with_capabilities(
                focused,
                self.hovered.clone(),
                self.capture.clone(),
                capabilities,
                self.work_areas.observation(),
            )?;
        self.coordinator
            .lock()
            .prepare_host_ingress()
            .map_err(Into::into)
    }

    #[cfg(test)]
    fn freeze_facts(
        &mut self,
        facts: &[WinitWindowFacts],
    ) -> Result<NativeHostIngress, NativePlatformIngressError> {
        let prepared = self.prepare_facts(facts)?;
        let (ingress, settlement) = prepared.into_parts();
        self.coordinator.lock().commit_host_ingress(settlement)?;
        Ok(ingress)
    }

    fn reconcile_roster(
        &mut self,
        facts: &[WinitWindowFacts],
    ) -> Result<(), NativePlatformIngressError> {
        let mut window_ids = HashSet::with_capacity(facts.len());
        let mut viewport_ids = BTreeSet::new();
        for facts in facts {
            if !window_ids.insert(facts.window_id) {
                return Err(NativePlatformIngressError::DuplicateWindow(facts.window_id));
            }
            if !viewport_ids.insert(facts.viewport_id) {
                return Err(NativePlatformIngressError::DuplicateViewport(
                    facts.viewport_id,
                ));
            }
        }

        let exact_pairs = facts
            .iter()
            .map(|facts| (facts.window_id, facts.viewport_id))
            .collect::<HashSet<_>>();
        let retired = self
            .window_bindings
            .iter()
            .filter_map(|(window_id, binding)| {
                (!exact_pairs.contains(&(*window_id, binding.viewport_id())))
                    .then_some((*window_id, *binding))
            })
            .collect::<Vec<_>>();
        for (window_id, binding) in retired {
            self.retire_pointer_state(binding)?;
            let lifecycle = self
                .dispatched_effects
                .get(&(binding, NativeEffectProperty::Lifecycle));
            if let Some(request) = lifecycle {
                self.coordinator
                    .lock()
                    .retire_viewport_after_effect(binding, request)?;
            } else {
                self.coordinator.lock().retire_viewport(binding)?;
            }
            self.dispatched_effects
                .retain(|(pending_binding, _), _| *pending_binding != binding);
            self.close_states.remove(&binding);
            self.window_bindings.remove(&window_id);
            self.windows_by_viewport.remove(&binding.viewport_id());
        }

        for facts in facts {
            if self.window_bindings.contains_key(&facts.window_id) {
                continue;
            }
            let binding = self
                .coordinator
                .lock()
                .register_viewport(facts.viewport_id)?;
            self.window_bindings.insert(facts.window_id, binding);
            self.windows_by_viewport
                .insert(facts.viewport_id, facts.window_id);
            if facts.viewport_id != ViewportId::ROOT {
                self.close_states
                    .insert(binding, NativeCloseState::LiveClear);
            }
        }
        Ok(())
    }

    fn retire_pointer_state(
        &mut self,
        binding: NativeViewportBinding,
    ) -> Result<(), NativePlatformIngressError> {
        let retired_keys = self
            .pointer_states
            .iter()
            .filter_map(|(key, state)| {
                (state.source == NativePointerSource::Viewport(binding)).then_some(*key)
            })
            .collect::<Vec<_>>();
        for key in retired_keys {
            let state = self
                .pointer_states
                .get(&key)
                .expect("the retired key was derived from the same pointer-state map");
            if let Some(token) = self.scroll_sequences.get(&key).copied() {
                let scroll = NativeScrollEdge::new(
                    state.identity.device_id(),
                    Some(token),
                    NativeScrollPhase::Cancel(NativeScrollCancelReason::DeviceRemoved),
                    None,
                    NativeAuthority::unknown(NativeUnavailableReason::Retired),
                    NativeAuthority::unknown(NativeUnavailableReason::Retired),
                )
                .expect("a provider-owned scroll cancellation has a legal terminal shape");
                self.coordinator.lock().record_terminal_pointer_edge(
                    state.source,
                    state.identity,
                    NativePointerEdgeKind::Scrolled(scroll),
                )?;
            }
            self.pointer_states.remove(&key);
            self.scroll_sequences.remove(&key);
        }
        #[cfg(feature = "native-test-support")]
        if self
            .test_pointer_state
            .as_ref()
            .is_some_and(|state| state.capture_owner == Some(binding))
        {
            self.test_pointer_state = None;
        }
        if self.hovered.value() == Some(&NativeHoveredWindow::Viewport(binding)) {
            self.hovered = NativeAuthority::unknown(NativeUnavailableReason::Retired);
        }
        if self.capture.value() == Some(&NativeCaptureOwner::Viewport(binding)) {
            self.capture = NativeAuthority::unknown(NativeUnavailableReason::Retired);
        }
        Ok(())
    }

    fn binding_for(
        &self,
        facts: &WinitWindowFacts,
    ) -> Result<NativeViewportBinding, NativePlatformIngressError> {
        let binding = self
            .window_bindings
            .get(&facts.window_id)
            .copied()
            .ok_or(NativePlatformIngressError::IncompleteRoster)?;
        if binding.viewport_id() != facts.viewport_id
            || self.windows_by_viewport.get(&facts.viewport_id) != Some(&facts.window_id)
        {
            return Err(NativePlatformIngressError::IncompleteRoster);
        }
        Ok(binding)
    }

    fn record_window_facts(
        &mut self,
        facts: &WinitWindowFacts,
        focus_roster_is_consistent: bool,
    ) -> Result<(), NativePlatformIngressError> {
        let binding = self.binding_for(facts)?;
        let geometry_key = (binding, NativeEffectProperty::Geometry);
        let presentation_key = (binding, NativeEffectProperty::Presentation);
        let pointer_input_key = (binding, NativeEffectProperty::PointerInput);
        let focus_key = (binding, NativeEffectProperty::Focus);
        let geometry_applied = self
            .dispatched_effects
            .get(&geometry_key)
            .is_some_and(|request| geometry_effect_is_observed(request.effect(), facts));
        let presentation_applied = self
            .dispatched_effects
            .get(&presentation_key)
            .is_some_and(|request| presentation_effect_is_observed(request.effect(), facts));
        let pointer_input_applied = self
            .dispatched_effects
            .get(&pointer_input_key)
            .is_some_and(|request| pointer_input_effect_is_observed(request.effect(), facts));
        let focus_applied = focus_roster_is_consistent
            && facts.focused
            && self
                .dispatched_effects
                .get(&focus_key)
                .is_some_and(|request| {
                    matches!(request.effect(), NativeWindowEffect::RequestFocus)
                });
        let geometry_request = geometry_applied
            .then(|| self.dispatched_effects.get(&geometry_key))
            .flatten();
        let presentation_request = presentation_applied
            .then(|| self.dispatched_effects.get(&presentation_key))
            .flatten();
        let pointer_input_request = pointer_input_applied
            .then(|| self.dispatched_effects.get(&pointer_input_key))
            .flatten();
        let focus_request = focus_applied
            .then(|| self.dispatched_effects.get(&focus_key))
            .flatten();
        let mut coordinator = self.coordinator.lock();
        let geometry = coordinator.window_geometry(
            binding,
            facts.content_rect.clone(),
            facts.outer_rect.clone(),
            facts.native_scale_factor.clone(),
            facts.presentation_scale_factor.clone(),
            facts.work_area.clone(),
        )?;
        let geometry = coordinator.observe_property(
            binding,
            NativeEffectProperty::Geometry,
            NativeAuthority::known(geometry),
            geometry_request.map_or(
                ObservationAcknowledgement::Baseline,
                ObservationAcknowledgement::Applied,
            ),
        )?;
        let presentation = coordinator.observe_property(
            binding,
            NativeEffectProperty::Presentation,
            facts.presentation.clone(),
            presentation_request.map_or(
                ObservationAcknowledgement::Baseline,
                ObservationAcknowledgement::Applied,
            ),
        )?;
        let pointer_input = coordinator.observe_property(
            binding,
            NativeEffectProperty::PointerInput,
            facts.pointer_input.clone(),
            pointer_input_request.map_or(
                ObservationAcknowledgement::Baseline,
                ObservationAcknowledgement::Applied,
            ),
        )?;
        let focus = coordinator.observe_property(
            binding,
            NativeEffectProperty::Focus,
            if focus_roster_is_consistent {
                NativeAuthority::known(facts.focused)
            } else {
                NativeAuthority::unknown(NativeUnavailableReason::StaleSource)
            },
            focus_request.map_or(
                ObservationAcknowledgement::Baseline,
                ObservationAcknowledgement::Applied,
            ),
        )?;
        let close = coordinator.observe_property(
            binding,
            NativeEffectProperty::Close,
            self.close_states.get(&binding).copied().map_or_else(
                || NativeAuthority::unknown(NativeUnavailableReason::NotObserved),
                NativeAuthority::known,
            ),
            ObservationAcknowledgement::Baseline,
        )?;
        let snapshot = coordinator.window_snapshot(
            binding,
            geometry,
            presentation,
            pointer_input,
            focus,
            close,
        )?;
        coordinator.record_window_snapshot(snapshot)?;
        drop(coordinator);
        if geometry_applied {
            self.dispatched_effects.remove(&geometry_key);
        }
        if presentation_applied {
            self.dispatched_effects.remove(&presentation_key);
        }
        if pointer_input_applied {
            self.dispatched_effects.remove(&pointer_input_key);
        }
        if focus_applied {
            self.dispatched_effects.remove(&focus_key);
        }
        Ok(())
    }
}

/// Failure to form one exact winit-backed native ingress batch.
#[derive(Debug)]
pub(super) enum NativePlatformIngressError {
    DuplicateWindow(WindowId),
    DuplicateViewport(ViewportId),
    IncompleteRoster,
    InvalidScrollDelta,
    InvalidScrollEdge,
    ScrollSequenceOutOfOrder(winit::event::TouchPhase),
    #[cfg(feature = "native-test-support")]
    NativeTestPointerUnavailable(ViewportId, NativeUnavailableReason),
    #[cfg(feature = "native-test-support")]
    NativeTestPointerActionOutOfOrder(NativeTestPointerAction),
    Platform(NativePlatformError),
}

impl From<NativePlatformError> for NativePlatformIngressError {
    fn from(error: NativePlatformError) -> Self {
        Self::Platform(error)
    }
}

impl std::fmt::Display for NativePlatformIngressError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::DuplicateWindow(window_id) => {
                write!(formatter, "native roster repeats window {window_id:?}")
            }
            Self::DuplicateViewport(viewport_id) => {
                write!(formatter, "native roster repeats viewport {viewport_id:?}")
            }
            Self::IncompleteRoster => formatter.write_str("native window roster is inconsistent"),
            Self::InvalidScrollDelta => {
                formatter.write_str("native scroll delta contains a non-finite component")
            }
            Self::InvalidScrollEdge => {
                formatter.write_str("native scroll phase, sequence, and delta are inconsistent")
            }
            Self::ScrollSequenceOutOfOrder(phase) => {
                write!(formatter, "native scroll phase is out of order: {phase:?}")
            }
            #[cfg(feature = "native-test-support")]
            Self::NativeTestPointerUnavailable(viewport_id, reason) => write!(
                formatter,
                "native test pointer facts are unavailable for {viewport_id:?}: {reason:?}"
            ),
            #[cfg(feature = "native-test-support")]
            Self::NativeTestPointerActionOutOfOrder(action) => {
                write!(
                    formatter,
                    "native test pointer action is out of order: {action:?}"
                )
            }
            Self::Platform(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for NativePlatformIngressError {}

fn physical_rect<E>(
    position: &Result<winit::dpi::PhysicalPosition<i32>, E>,
    size: winit::dpi::PhysicalSize<u32>,
) -> NativeAuthority<NativePhysicalRect> {
    let Ok(position) = position else {
        return NativeAuthority::unknown(NativeUnavailableReason::Unsupported);
    };
    let Ok(width) = i32::try_from(size.width) else {
        return NativeAuthority::unknown(NativeUnavailableReason::Unsupported);
    };
    let Ok(height) = i32::try_from(size.height) else {
        return NativeAuthority::unknown(NativeUnavailableReason::Unsupported);
    };
    let Some(max_x) = position.x.checked_add(width) else {
        return NativeAuthority::unknown(NativeUnavailableReason::Unsupported);
    };
    let Some(max_y) = position.y.checked_add(height) else {
        return NativeAuthority::unknown(NativeUnavailableReason::Unsupported);
    };
    NativeAuthority::known(NativePhysicalRect::new(
        NativePhysicalPoint::new(position.x, position.y),
        NativePhysicalPoint::new(max_x, max_y),
    ))
}

fn presentation_state(
    visible: Option<bool>,
    minimized: Option<bool>,
) -> NativeAuthority<NativePresentationState> {
    match (visible, minimized) {
        (_, Some(true)) => NativeAuthority::known(NativePresentationState::Minimized),
        (Some(false), _) => NativeAuthority::known(NativePresentationState::Hidden),
        (Some(true), Some(false)) => NativeAuthority::known(NativePresentationState::Visible),
        _ => NativeAuthority::unknown(NativeUnavailableReason::Unsupported),
    }
}

fn geometry_effect_is_observed(effect: &NativeWindowEffect, facts: &WinitWindowFacts) -> bool {
    match effect {
        NativeWindowEffect::SetOuterRect(expected) => facts.outer_rect.value() == Some(expected),
        _ => false,
    }
}

fn presentation_effect_is_observed(effect: &NativeWindowEffect, facts: &WinitWindowFacts) -> bool {
    match effect {
        NativeWindowEffect::SetVisible(true) => {
            facts.presentation.value() == Some(&NativePresentationState::Visible)
        }
        NativeWindowEffect::SetVisible(false) => {
            facts.presentation.value() == Some(&NativePresentationState::Hidden)
        }
        _ => false,
    }
}

fn pointer_input_effect_is_observed(effect: &NativeWindowEffect, facts: &WinitWindowFacts) -> bool {
    match effect {
        NativeWindowEffect::SetPointerPassThrough(true) => {
            facts.pointer_input.value() == Some(&NativePointerInputState::PassThrough)
        }
        NativeWindowEffect::SetPointerPassThrough(false) => {
            facts.pointer_input.value() == Some(&NativePointerInputState::ReceivesInput)
        }
        _ => false,
    }
}

fn native_mouse_button(button: winit::event::MouseButton) -> NativePointerButton {
    match button {
        winit::event::MouseButton::Left => NativePointerButton::Primary,
        winit::event::MouseButton::Right => NativePointerButton::Secondary,
        winit::event::MouseButton::Middle => NativePointerButton::Middle,
        winit::event::MouseButton::Back => NativePointerButton::Back,
        winit::event::MouseButton::Forward => NativePointerButton::Forward,
        winit::event::MouseButton::Other(button) => NativePointerButton::Other(button),
    }
}

fn native_scroll_delta(
    delta: winit::event::MouseScrollDelta,
) -> Result<NativeScrollDelta, NativePlatformIngressError> {
    let (x, y, physical) = match delta {
        winit::event::MouseScrollDelta::LineDelta(x, y) => (f64::from(x), f64::from(y), false),
        winit::event::MouseScrollDelta::PixelDelta(delta) => (delta.x, delta.y, true),
    };
    let vector = NativeFiniteScrollVector::new(x, y)
        .ok_or(NativePlatformIngressError::InvalidScrollDelta)?;
    Ok(if physical {
        NativeScrollDelta::PhysicalPixels(vector)
    } else {
        NativeScrollDelta::Lines(vector)
    })
}

fn native_scroll_modifiers(state: winit::keyboard::ModifiersState) -> NativeScrollModifiers {
    NativeScrollModifiers::new(
        state.shift_key(),
        state.control_key(),
        state.alt_key(),
        if cfg!(target_os = "macos") {
            state.super_key()
        } else {
            state.control_key()
        },
    )
}

fn desktop_pointer_position(
    window: &Window,
    position: winit::dpi::PhysicalPosition<f64>,
) -> NativeAuthority<NativePhysicalPoint> {
    let Ok(origin) = window.inner_position() else {
        return NativeAuthority::unknown(NativeUnavailableReason::Unsupported);
    };
    let Some(x) = exact_physical_component(position.x) else {
        return NativeAuthority::unknown(NativeUnavailableReason::Unsupported);
    };
    let Some(y) = exact_physical_component(position.y) else {
        return NativeAuthority::unknown(NativeUnavailableReason::Unsupported);
    };
    let Some(x) = origin.x.checked_add(x) else {
        return NativeAuthority::unknown(NativeUnavailableReason::Unsupported);
    };
    let Some(y) = origin.y.checked_add(y) else {
        return NativeAuthority::unknown(NativeUnavailableReason::Unsupported);
    };
    NativeAuthority::known(NativePhysicalPoint::new(x, y))
}

#[cfg(feature = "native-test-support")]
fn native_test_pointer_coordinates(
    window: &Window,
    binding: NativeViewportBinding,
    presentation: &egui::PointerHitGraphSnapshot,
    position: egui::Pos2,
) -> Option<(
    NativeAuthority<NativePhysicalPoint>,
    NativeAuthority<NativePointerCoordinateCapture>,
    egui::Pos2,
)> {
    let presentation_scale_factor = f64::from(presentation.pixels_per_point());
    let native_scale_factor = window.scale_factor();
    if !presentation_scale_factor.is_finite()
        || presentation_scale_factor <= 0.0
        || !native_scale_factor.is_finite()
        || native_scale_factor <= 0.0
    {
        return None;
    }
    let local_x = rounded_physical_component(f64::from(position.x) * presentation_scale_factor)?;
    let local_y = rounded_physical_component(f64::from(position.y) * presentation_scale_factor)?;
    let inner_size = window.inner_size();
    if local_x < 0
        || local_y < 0
        || u32::try_from(local_x).ok()? >= inner_size.width
        || u32::try_from(local_y).ok()? >= inner_size.height
    {
        return None;
    }
    let origin = window.inner_position().ok()?;
    let desktop_x = origin.x.checked_add(local_x)?;
    let desktop_y = origin.y.checked_add(local_y)?;
    let origin = NativePhysicalPoint::new(origin.x, origin.y);
    let presented_position = egui::Pos2::new(
        (f64::from(local_x) / presentation_scale_factor) as f32,
        (f64::from(local_y) / presentation_scale_factor) as f32,
    );
    if !presented_position.is_finite() {
        return None;
    }
    Some((
        NativeAuthority::known(NativePhysicalPoint::new(desktop_x, desktop_y)),
        NativeAuthority::known(NativePointerCoordinateCapture::new(
            binding,
            origin,
            native_scale_factor,
            presentation_scale_factor,
        )),
        presented_position,
    ))
}

#[cfg(feature = "native-test-support")]
fn native_test_desktop_point(position: egui::Pos2) -> Option<NativePhysicalPoint> {
    Some(NativePhysicalPoint::new(
        exact_physical_component(f64::from(position.x))?,
        exact_physical_component(f64::from(position.y))?,
    ))
}

#[cfg(feature = "native-test-support")]
fn native_test_owned_viewports_contain(
    windows: &[(NativeViewportBinding, Arc<Window>)],
    excluded: Option<NativeViewportBinding>,
    point: NativePhysicalPoint,
) -> Option<bool> {
    for (binding, window) in windows {
        if Some(*binding) == excluded {
            continue;
        }
        let origin = window.inner_position().ok()?;
        let width = i32::try_from(window.inner_size().width).ok()?;
        let height = i32::try_from(window.inner_size().height).ok()?;
        let max_x = origin.x.checked_add(width)?;
        let max_y = origin.y.checked_add(height)?;
        if point.x() >= origin.x && point.x() < max_x && point.y() >= origin.y && point.y() < max_y
        {
            return Some(true);
        }
    }
    Some(false)
}

fn pointer_coordinate_capture(
    windows: &[(NativeViewportBinding, Arc<Window>)],
    hovered: &NativeAuthority<NativeHoveredWindow>,
    position: &NativeAuthority<NativePhysicalPoint>,
    egui_ctx: &egui::Context,
) -> NativeAuthority<NativePointerCoordinateCapture> {
    let Some(NativeHoveredWindow::Viewport(binding)) = hovered.value() else {
        return NativeAuthority::unknown(
            hovered
                .unavailable_reason()
                .unwrap_or(NativeUnavailableReason::NotObserved),
        );
    };
    pointer_coordinate_capture_for_binding(windows, *binding, position, egui_ctx)
}

fn pointer_delivery_coordinate_capture(
    windows: &[(NativeViewportBinding, Arc<Window>)],
    delivery: &NativeAuthority<NativePointerDeliveryOwner>,
    position: &NativeAuthority<NativePhysicalPoint>,
    egui_ctx: &egui::Context,
) -> NativeAuthority<NativePointerCoordinateCapture> {
    let Some(NativePointerDeliveryOwner::Viewport(binding)) = delivery.value() else {
        return NativeAuthority::unknown(
            delivery
                .unavailable_reason()
                .unwrap_or(NativeUnavailableReason::NotObserved),
        );
    };
    pointer_coordinate_capture_for_binding(windows, *binding, position, egui_ctx)
}

fn pointer_coordinate_capture_for_binding(
    windows: &[(NativeViewportBinding, Arc<Window>)],
    binding: NativeViewportBinding,
    position: &NativeAuthority<NativePhysicalPoint>,
    egui_ctx: &egui::Context,
) -> NativeAuthority<NativePointerCoordinateCapture> {
    if position.value().is_none() {
        return NativeAuthority::unknown(
            position
                .unavailable_reason()
                .unwrap_or(NativeUnavailableReason::NotObserved),
        );
    }
    let Some(window) = windows
        .iter()
        .find_map(|(candidate, window)| (*candidate == binding).then_some(window))
    else {
        return NativeAuthority::unknown(NativeUnavailableReason::StaleSource);
    };
    let Ok(origin) = window.inner_position() else {
        return NativeAuthority::unknown(NativeUnavailableReason::Unsupported);
    };
    let native_scale_factor = window.scale_factor();
    let presentation_scale_factor = f64::from(egui_winit::pixels_per_point(egui_ctx, window));
    if !native_scale_factor.is_finite()
        || native_scale_factor <= 0.0
        || !presentation_scale_factor.is_finite()
        || presentation_scale_factor <= 0.0
    {
        return NativeAuthority::unknown(NativeUnavailableReason::Unsupported);
    }
    NativeAuthority::known(NativePointerCoordinateCapture::new(
        binding,
        NativePhysicalPoint::new(origin.x, origin.y),
        native_scale_factor,
        presentation_scale_factor,
    ))
}

fn exact_physical_component(value: f64) -> Option<i32> {
    (value.is_finite()
        && value.fract() == 0.0
        && value >= f64::from(i32::MIN)
        && value <= f64::from(i32::MAX))
    .then_some(value as i32)
}

#[cfg(feature = "native-test-support")]
fn rounded_physical_component(value: f64) -> Option<i32> {
    let rounded = value.round();
    (value.is_finite() && rounded >= f64::from(i32::MIN) && rounded <= f64::from(i32::MAX))
        .then_some(rounded as i32)
}

fn inner_size_for_target_outer(
    target_outer: winit::dpi::PhysicalSize<u32>,
    current_outer: winit::dpi::PhysicalSize<u32>,
    current_inner: winit::dpi::PhysicalSize<u32>,
) -> Option<winit::dpi::PhysicalSize<u32>> {
    let decoration_width = current_outer.width.checked_sub(current_inner.width)?;
    let decoration_height = current_outer.height.checked_sub(current_inner.height)?;
    let width = target_outer.width.checked_sub(decoration_width)?;
    let height = target_outer.height.checked_sub(decoration_height)?;
    (width > 0 && height > 0).then_some(winit::dpi::PhysicalSize::new(width, height))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OuterRectDispatchAction {
    Complete,
    Move(winit::dpi::PhysicalPosition<i32>),
    Resize(winit::dpi::PhysicalSize<u32>),
}

fn outer_rect_dispatch_action(
    target_position: winit::dpi::PhysicalPosition<i32>,
    target_size: winit::dpi::PhysicalSize<u32>,
    current_position: winit::dpi::PhysicalPosition<i32>,
    current_outer: winit::dpi::PhysicalSize<u32>,
    current_inner: winit::dpi::PhysicalSize<u32>,
) -> Option<OuterRectDispatchAction> {
    if current_position != target_position {
        return Some(OuterRectDispatchAction::Move(target_position));
    }
    if current_outer == target_size {
        return Some(OuterRectDispatchAction::Complete);
    }
    inner_size_for_target_outer(target_size, current_outer, current_inner)
        .map(OuterRectDispatchAction::Resize)
}

fn outer_rect_target(
    rect: NativePhysicalRect,
) -> Option<(
    winit::dpi::PhysicalPosition<i32>,
    winit::dpi::PhysicalSize<u32>,
)> {
    let min = rect.min();
    let max = rect.max();
    let width = u32::try_from(max.x().checked_sub(min.x())?).ok()?;
    let height = u32::try_from(max.y().checked_sub(min.y())?).ok()?;
    (width > 0 && height > 0).then_some((
        winit::dpi::PhysicalPosition::new(min.x(), min.y()),
        winit::dpi::PhysicalSize::new(width, height),
    ))
}

fn dispatch_window_effect(
    window: &Window,
    effect: &NativeWindowEffect,
) -> NativeEffectDispatchOutcome {
    match effect {
        NativeWindowEffect::SetVisible(visible) => {
            window.set_visible(*visible);
            NativeEffectDispatchOutcome::Dispatched
        }
        NativeWindowEffect::SetOuterRect(rect) => {
            let Some((target_position, target_size)) = outer_rect_target(*rect) else {
                return NativeEffectDispatchOutcome::Rejected;
            };
            let Ok(current_position) = window.outer_position() else {
                return NativeEffectDispatchOutcome::Unsupported;
            };
            let Some(action) = outer_rect_dispatch_action(
                target_position,
                target_size,
                current_position,
                window.outer_size(),
                window.inner_size(),
            ) else {
                return NativeEffectDispatchOutcome::Rejected;
            };
            match action {
                OuterRectDispatchAction::Complete => {}
                OuterRectDispatchAction::Move(position) => window.set_outer_position(position),
                OuterRectDispatchAction::Resize(inner_size) => {
                    let _ = window.request_inner_size(inner_size);
                }
            }
            NativeEffectDispatchOutcome::Dispatched
        }
        NativeWindowEffect::RequestFocus => {
            window.focus_window();
            NativeEffectDispatchOutcome::Dispatched
        }
        NativeWindowEffect::SetPointerPassThrough(pass_through) => {
            match window.set_cursor_hittest(!pass_through) {
                Ok(()) => NativeEffectDispatchOutcome::Dispatched,
                Err(error) => {
                    log::warn!("native pointer pass-through dispatch failed: {error}");
                    NativeEffectDispatchOutcome::Rejected
                }
            }
        }
        NativeWindowEffect::CancelClose => NativeEffectDispatchOutcome::Dispatched,
        NativeWindowEffect::Destroy => NativeEffectDispatchOutcome::Unsupported,
    }
}

fn destroy_dispatch(
    viewport: ViewportId,
    effect: &NativeWindowEffect,
) -> Option<(NativeEffectDispatchOutcome, Option<ViewportId>)> {
    matches!(effect, NativeWindowEffect::Destroy).then(|| {
        if viewport == ViewportId::ROOT {
            (NativeEffectDispatchOutcome::Unsupported, None)
        } else {
            (NativeEffectDispatchOutcome::Dispatched, Some(viewport))
        }
    })
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use super::*;
    use crate::native::platform_provider::NativeEffectCorrelation;

    fn viewport(name: &str) -> ViewportId {
        ViewportId::from_hash_of(name)
    }

    fn window_id() -> WindowId {
        WindowId::dummy()
    }

    fn facts(window_id: WindowId, viewport_id: ViewportId, focused: bool) -> WinitWindowFacts {
        WinitWindowFacts {
            window_id,
            viewport_id,
            content_rect: NativeAuthority::unknown(NativeUnavailableReason::Unsupported),
            outer_rect: NativeAuthority::unknown(NativeUnavailableReason::Unsupported),
            native_scale_factor: NativeAuthority::known(1.0),
            presentation_scale_factor: NativeAuthority::known(1.0),
            work_area: NativeAuthority::unknown(NativeUnavailableReason::Unsupported),
            presentation: NativeAuthority::known(NativePresentationState::Visible),
            pointer_input: NativeAuthority::unknown(NativeUnavailableReason::Unsupported),
            capabilities: NativeBackendCapabilities::default(),
            focused,
            native_staging_surface: false,
        }
    }

    fn native_staging_facts(viewport_id: ViewportId) -> WinitWindowFacts {
        WinitWindowFacts {
            presentation: NativeAuthority::known(NativePresentationState::Hidden),
            native_staging_surface: true,
            ..facts(window_id(), viewport_id, false)
        }
    }

    fn mouse_pointer_key() -> WinitPointerKey {
        WinitPointerKey {
            device_id: winit::event::DeviceId::dummy(),
            stream: WinitPointerStream::Mouse,
        }
    }

    #[test]
    fn dropping_unsettled_native_ingress_poison_prevents_reuse() {
        let mut owner = NativePlatformIngressOwner::default();
        let prepared = owner.prepare_facts(&[]).unwrap();
        let (_, key) = prepared.into_parts();
        let settlement = NativeHostIngressSettlement::new(owner.coordinator(), key);

        drop(settlement);

        assert!(matches!(
            owner.prepare_facts(&[]),
            Err(NativePlatformIngressError::Platform(
                NativePlatformError::HostIngressPoisoned
            ))
        ));
    }

    #[test]
    fn committed_native_ingress_settlement_allows_the_successor_batch() {
        let mut owner = NativePlatformIngressOwner::default();
        let prepared = owner.prepare_facts(&[]).unwrap();
        let (_, key) = prepared.into_parts();
        NativeHostIngressSettlement::new(owner.coordinator(), key)
            .prepare_commit()
            .unwrap()
            .commit();

        let successor = owner.prepare_facts(&[]).unwrap();
        let (_, successor_key) = successor.into_parts();
        NativeHostIngressSettlement::new(owner.coordinator(), successor_key)
            .prepare_commit()
            .unwrap()
            .commit();
    }

    #[test]
    fn dropping_prevalidated_native_ingress_commit_poison_prevents_reuse() {
        let mut owner = NativePlatformIngressOwner::default();
        let prepared = owner.prepare_facts(&[]).unwrap();
        let (_, key) = prepared.into_parts();
        let commit = NativeHostIngressSettlement::new(owner.coordinator(), key)
            .prepare_commit()
            .unwrap();

        drop(commit);

        assert!(matches!(
            owner.prepare_facts(&[]),
            Err(NativePlatformIngressError::Platform(
                NativePlatformError::HostIngressPoisoned
            ))
        ));
    }

    #[test]
    fn roster_retirement_clears_only_pointer_state_owned_by_the_retired_binding() {
        let mut owner = NativePlatformIngressOwner::default();
        let viewport_id = viewport("retired-pointer-state");
        let initial = facts(window_id(), viewport_id, false);
        let binding = owner
            .freeze_facts(std::slice::from_ref(&initial))
            .unwrap()
            .platform()
            .inventory()[0];
        let retired_key = mouse_pointer_key();
        let unrelated_key = WinitPointerKey {
            device_id: retired_key.device_id,
            stream: WinitPointerStream::Touch(7),
        };
        let device = NativePointerDeviceId::new(9);
        owner.pointer_states.insert(
            retired_key,
            WinitPointerState {
                identity: NativePointerIdentity::new(device, NativePointerId::new(1)),
                source: NativePointerSource::Viewport(binding),
                position: NativeAuthority::unknown(NativeUnavailableReason::NotObserved),
            },
        );
        owner.pointer_states.insert(
            unrelated_key,
            WinitPointerState {
                identity: NativePointerIdentity::new(device, NativePointerId::new(2)),
                source: NativePointerSource::Foreign,
                position: NativeAuthority::unknown(NativeUnavailableReason::NotObserved),
            },
        );
        owner
            .scroll_sequences
            .insert(retired_key, NativeScrollSequenceToken::new(11));
        owner
            .scroll_sequences
            .insert(unrelated_key, NativeScrollSequenceToken::new(12));

        let ingress = owner.freeze_facts(&[]).unwrap();

        let records = ingress.ordered().records();
        let cancel = records
            .iter()
            .position(|record| {
                matches!(
                    record.event(),
                    crate::NativeIngressEvent::PointerEdge(edge)
                        if edge.ends_stream()
                            && matches!(
                                edge.kind(),
                                NativePointerEdgeKind::Scrolled(scroll)
                                    if scroll.sequence() == Some(NativeScrollSequenceToken::new(11))
                                        && scroll.phase()
                                            == NativeScrollPhase::Cancel(
                                                NativeScrollCancelReason::DeviceRemoved,
                                            )
                            )
                )
            })
            .expect("retirement emits one exact scroll terminal");
        let retirement = records
            .iter()
            .position(|record| {
                matches!(
                    record.event(),
                    crate::NativeIngressEvent::Retirement(tombstone)
                        if tombstone.binding() == binding
                )
            })
            .expect("the viewport retirement remains present");
        assert!(cancel < retirement);

        assert!(!owner.pointer_states.contains_key(&retired_key));
        assert!(!owner.scroll_sequences.contains_key(&retired_key));
        assert!(owner.pointer_states.contains_key(&unrelated_key));
        assert!(owner.scroll_sequences.contains_key(&unrelated_key));
        owner
            .record_device_event(
                retired_key.device_id,
                &winit::event::DeviceEvent::Removed,
                egui::BackendEventSequence::new(1),
            )
            .expect("device removal must not replay a retired viewport binding");
    }

    #[test]
    fn wheel_position_never_falls_back_to_retained_cursor_state() {
        let mut owner = NativePlatformIngressOwner::default();
        let viewport_id = viewport("wheel-position");
        let initial = facts(window_id(), viewport_id, false);
        let binding = owner
            .freeze_facts(std::slice::from_ref(&initial))
            .unwrap()
            .platform()
            .inventory()[0];
        let key = mouse_pointer_key();
        let position = NativePhysicalPoint::new(123, 456);
        owner.pointer_states.insert(
            key,
            WinitPointerState {
                identity: NativePointerIdentity::new(
                    NativePointerDeviceId::new(9),
                    NativePointerId::new(1),
                ),
                source: NativePointerSource::Viewport(binding),
                position: NativeAuthority::known(position),
            },
        );

        assert_eq!(
            owner
                .retained_pointer_position(key, NativePointerSource::Viewport(binding))
                .value(),
            Some(&position)
        );
        let event_probe =
            NativeAuthority::<NativePhysicalPoint>::unknown(NativeUnavailableReason::NotObserved);
        assert!(event_probe.value().is_none());
        assert_eq!(
            event_probe.unavailable_reason(),
            Some(NativeUnavailableReason::NotObserved)
        );
    }

    #[test]
    fn phaseful_wheel_samples_retain_one_exact_session_token() {
        let mut owner = NativePlatformIngressOwner::default();
        let key = mouse_pointer_key();
        let device = NativePointerDeviceId::new(7);
        let delta = winit::event::MouseScrollDelta::LineDelta(1.0, -2.0);

        let begin = owner
            .native_scroll_edge(key, device, delta, winit::event::TouchPhase::Started, None)
            .unwrap();
        let update = owner
            .native_scroll_edge(key, device, delta, winit::event::TouchPhase::Moved, None)
            .unwrap();
        let end = owner
            .native_scroll_edge(key, device, delta, winit::event::TouchPhase::Ended, None)
            .unwrap();

        assert_eq!(begin.phase(), NativeScrollPhase::Begin);
        assert_eq!(update.phase(), NativeScrollPhase::Update);
        assert_eq!(end.phase(), NativeScrollPhase::End);
        assert_eq!(begin.sequence(), update.sequence());
        assert_eq!(update.sequence(), end.sequence());
        assert_eq!(begin.modifiers().value(), None);
        assert_eq!(
            begin.modifiers().unavailable_reason(),
            Some(NativeUnavailableReason::NotObserved)
        );
    }

    #[test]
    fn moved_wheel_without_a_session_start_is_discrete() {
        let mut owner = NativePlatformIngressOwner::default();
        let scroll = owner
            .native_scroll_edge(
                mouse_pointer_key(),
                NativePointerDeviceId::new(9),
                winit::event::MouseScrollDelta::PixelDelta(winit::dpi::PhysicalPosition::new(
                    3.0, -4.0,
                )),
                winit::event::TouchPhase::Moved,
                Some(NativeScrollModifiers::default()),
            )
            .unwrap();

        assert_eq!(scroll.phase(), NativeScrollPhase::Discrete);
        assert_eq!(scroll.sequence(), None);
        assert!(matches!(
            scroll.delta(),
            Some(NativeScrollDelta::PhysicalPixels(_))
        ));
        assert_eq!(
            scroll.modifiers().value(),
            Some(&NativeScrollModifiers::default())
        );
    }

    #[test]
    fn terminal_wheel_phase_without_a_session_fails_closed() {
        let mut owner = NativePlatformIngressOwner::default();
        let error = owner
            .native_scroll_edge(
                mouse_pointer_key(),
                NativePointerDeviceId::new(11),
                winit::event::MouseScrollDelta::LineDelta(0.0, 1.0),
                winit::event::TouchPhase::Ended,
                None,
            )
            .unwrap_err();

        assert!(matches!(
            error,
            NativePlatformIngressError::ScrollSequenceOutOfOrder(winit::event::TouchPhase::Ended)
        ));
    }

    #[test]
    fn outer_rect_dispatch_subtracts_the_current_window_decoration_extent() {
        assert_eq!(
            inner_size_for_target_outer(
                winit::dpi::PhysicalSize::new(800, 600),
                winit::dpi::PhysicalSize::new(820, 640),
                winit::dpi::PhysicalSize::new(800, 600),
            ),
            Some(winit::dpi::PhysicalSize::new(780, 560)),
        );
        assert_eq!(
            inner_size_for_target_outer(
                winit::dpi::PhysicalSize::new(800, 600),
                winit::dpi::PhysicalSize::new(800, 600),
                winit::dpi::PhysicalSize::new(800, 600),
            ),
            Some(winit::dpi::PhysicalSize::new(800, 600)),
        );
    }

    #[test]
    fn outer_rect_dispatch_waits_for_destination_decoration_extent_after_a_dpi_move() {
        let target_position = winit::dpi::PhysicalPosition::new(2_000, 0);
        let target_size = winit::dpi::PhysicalSize::new(800, 600);

        assert_eq!(
            outer_rect_dispatch_action(
                target_position,
                target_size,
                winit::dpi::PhysicalPosition::new(0, 0),
                winit::dpi::PhysicalSize::new(820, 640),
                winit::dpi::PhysicalSize::new(800, 600),
            ),
            Some(OuterRectDispatchAction::Move(target_position)),
        );
        assert_eq!(
            outer_rect_dispatch_action(
                target_position,
                target_size,
                target_position,
                winit::dpi::PhysicalSize::new(840, 680),
                winit::dpi::PhysicalSize::new(800, 600),
            ),
            Some(OuterRectDispatchAction::Resize(
                winit::dpi::PhysicalSize::new(760, 520),
            )),
        );
    }

    #[test]
    fn outer_rect_dispatch_rejects_unrepresentable_decoration_geometry() {
        assert_eq!(
            inner_size_for_target_outer(
                winit::dpi::PhysicalSize::new(10, 10),
                winit::dpi::PhysicalSize::new(100, 100),
                winit::dpi::PhysicalSize::new(80, 80),
            ),
            None,
        );
        assert_eq!(
            inner_size_for_target_outer(
                winit::dpi::PhysicalSize::new(100, 100),
                winit::dpi::PhysicalSize::new(80, 80),
                winit::dpi::PhysicalSize::new(100, 100),
            ),
            None,
        );
    }

    #[test]
    fn exact_roster_replacement_mints_and_tombstones_binding() {
        let mut owner = NativePlatformIngressOwner::default();
        let viewport_id = viewport("recreated");
        let first = owner
            .freeze_facts(&[facts(window_id(), viewport_id, true)])
            .unwrap();
        let first_binding = first.platform().inventory()[0];

        let retired = owner.freeze_facts(&[]).unwrap();
        let second = owner
            .freeze_facts(&[facts(window_id(), viewport_id, true)])
            .unwrap();
        let second_binding = second.platform().inventory()[0];

        assert_ne!(first_binding, second_binding);
        assert_eq!(retired.retirement_tombstones().len(), 1);
        assert_eq!(retired.retirement_tombstones()[0].binding(), first_binding);
    }

    #[test]
    fn old_window_pair_cannot_authorize_replacement_binding() {
        let mut owner = NativePlatformIngressOwner::default();
        let viewport_id = viewport("aba");
        let old_facts = facts(window_id(), viewport_id, true);
        let first = owner
            .freeze_facts(std::slice::from_ref(&old_facts))
            .unwrap();
        let first_binding = first.platform().inventory()[0];
        owner.freeze_facts(&[]).unwrap();
        let second = owner
            .freeze_facts(&[facts(window_id(), viewport_id, true)])
            .unwrap();

        assert_ne!(first_binding, second.platform().inventory()[0]);
        assert!(!second.platform().inventory().contains(&first_binding));
    }

    #[test]
    fn portable_global_facts_remain_unknown() {
        let mut owner = NativePlatformIngressOwner::default();
        let ingress = owner
            .freeze_facts(&[facts(window_id(), viewport("unknown"), false)])
            .unwrap();

        assert_eq!(
            ingress.platform().hovered().unavailable_reason(),
            Some(NativeUnavailableReason::Unsupported)
        );
        assert_eq!(
            ingress.platform().capture().unavailable_reason(),
            Some(NativeUnavailableReason::Unsupported)
        );
        assert_eq!(
            ingress.platform().focused().unavailable_reason(),
            Some(NativeUnavailableReason::Unsupported)
        );
    }

    #[test]
    fn later_exact_visibility_observation_acknowledges_dispatched_effect() {
        let mut owner = NativePlatformIngressOwner::default();
        let viewport_id = viewport("visibility-ack");
        let initial = facts(window_id(), viewport_id, false);
        let first = owner.freeze_facts(std::slice::from_ref(&initial)).unwrap();
        let binding = first.platform().inventory()[0];
        let correlation = NativeEffectCorrelation::new(egui::UserData::new("show-window"));
        let request = owner
            .coordinator
            .lock()
            .issue_effect(binding, NativeWindowEffect::SetVisible(false), correlation)
            .unwrap();
        owner
            .coordinator
            .lock()
            .report_effect_dispatch(&request, NativeEffectDispatchOutcome::Dispatched)
            .unwrap();
        owner
            .dispatched_effects
            .insert((binding, request.property()), request);

        let hidden = WinitWindowFacts {
            presentation: NativeAuthority::known(NativePresentationState::Hidden),
            ..initial
        };
        let second = owner.freeze_facts(&[hidden]).unwrap();
        let acknowledgement = second.platform().windows()[0]
            .presentation()
            .acknowledgement();

        assert_eq!(
            acknowledgement
                .correlation()
                .and_then(|value| value.user_data().downcast_ref::<&str>()),
            Some(&"show-window")
        );
        assert!(owner.dispatched_effects.is_empty());
    }

    #[test]
    fn later_exact_focus_observation_acknowledges_only_the_focused_binding() {
        let mut owner = NativePlatformIngressOwner::default();
        let viewport_id = viewport("focus-ack");
        let unfocused = facts(window_id(), viewport_id, false);
        let first = owner
            .freeze_facts(std::slice::from_ref(&unfocused))
            .unwrap();
        let binding = first.platform().inventory()[0];
        let request = owner
            .coordinator
            .lock()
            .issue_effect(
                binding,
                NativeWindowEffect::RequestFocus,
                NativeEffectCorrelation::new(egui::UserData::new("focus-window")),
            )
            .unwrap();
        owner
            .coordinator
            .lock()
            .report_effect_dispatch(&request, NativeEffectDispatchOutcome::Dispatched)
            .unwrap();
        owner
            .dispatched_effects
            .insert((binding, request.property()), request);

        let contradictory_focus = WinitWindowFacts {
            focused: true,
            ..unfocused.clone()
        };
        owner
            .record_window_facts(&contradictory_focus, false)
            .unwrap();
        assert!(
            owner
                .dispatched_effects
                .contains_key(&(binding, NativeEffectProperty::Focus))
        );

        let still_unfocused = owner
            .freeze_facts(std::slice::from_ref(&unfocused))
            .unwrap();
        assert_eq!(
            still_unfocused.platform().windows()[0]
                .focus()
                .acknowledgement()
                .correlation(),
            None
        );
        assert!(
            owner
                .dispatched_effects
                .contains_key(&(binding, NativeEffectProperty::Focus))
        );

        let focused = WinitWindowFacts {
            focused: true,
            ..unfocused
        };
        let applied = owner.freeze_facts(&[focused]).unwrap();
        let observation = applied.platform().windows()[0].focus();
        assert_eq!(observation.value().value(), Some(&true));
        assert_eq!(
            observation
                .acknowledgement()
                .correlation()
                .and_then(|value| value.user_data().downcast_ref::<&str>()),
            Some(&"focus-window")
        );
        assert!(owner.dispatched_effects.is_empty());
    }

    #[test]
    fn later_same_property_dispatch_terminalizes_the_unobservable_predecessor() {
        let mut owner = NativePlatformIngressOwner::default();
        let viewport_id = viewport("pointer-input-lane");
        let initial = facts(window_id(), viewport_id, false);
        let first = owner.freeze_facts(std::slice::from_ref(&initial)).unwrap();
        let binding = first.platform().inventory()[0];
        let first_request = owner
            .coordinator
            .lock()
            .issue_effect(
                binding,
                NativeWindowEffect::SetPointerPassThrough(true),
                NativeEffectCorrelation::new(egui::UserData::new("enable-pass-through")),
            )
            .unwrap();
        owner
            .coordinator
            .lock()
            .report_effect_dispatch(&first_request, NativeEffectDispatchOutcome::Dispatched)
            .unwrap();
        owner
            .dispatched_effects
            .insert((binding, first_request.property()), first_request);

        let second_request = owner
            .coordinator
            .lock()
            .issue_effect(
                binding,
                NativeWindowEffect::SetPointerPassThrough(false),
                NativeEffectCorrelation::new(egui::UserData::new("restore-pointer-input")),
            )
            .unwrap();
        owner
            .supersede_dispatched_effect(&second_request)
            .expect("a later same-property dispatch must terminalize its predecessor");
        owner
            .coordinator
            .lock()
            .report_effect_dispatch(&second_request, NativeEffectDispatchOutcome::Dispatched)
            .unwrap();
        owner
            .dispatched_effects
            .insert((binding, second_request.property()), second_request);

        let restored = WinitWindowFacts {
            pointer_input: NativeAuthority::known(NativePointerInputState::ReceivesInput),
            ..initial
        };
        let second = owner.freeze_facts(&[restored]).unwrap();

        let terminal = second
            .effect_results()
            .iter()
            .filter(|result| result.outcome() == NativeEffectDispatchOutcome::Indeterminate)
            .collect::<Vec<_>>();
        assert_eq!(terminal.len(), 1);
        assert_eq!(
            terminal[0].outcome(),
            NativeEffectDispatchOutcome::Indeterminate
        );
        assert_eq!(
            terminal[0].correlation().user_data().downcast_ref::<&str>(),
            Some(&"enable-pass-through")
        );
        assert_eq!(
            second.platform().windows()[0]
                .pointer_input()
                .acknowledgement()
                .correlation()
                .and_then(|value| value.user_data().downcast_ref::<&str>()),
            Some(&"restore-pointer-input")
        );
        assert!(owner.dispatched_effects.is_empty());
    }

    #[test]
    fn exact_roster_retirement_acknowledges_dispatched_destroy() {
        let mut owner = NativePlatformIngressOwner::default();
        let viewport_id = viewport("destroy-ack");
        let initial = facts(window_id(), viewport_id, false);
        let first = owner.freeze_facts(std::slice::from_ref(&initial)).unwrap();
        let binding = first.platform().inventory()[0];
        let correlation = NativeEffectCorrelation::new(egui::UserData::new("destroy-window"));
        let request = owner
            .coordinator
            .lock()
            .issue_effect(binding, NativeWindowEffect::Destroy, correlation)
            .unwrap();
        owner
            .coordinator
            .lock()
            .report_effect_dispatch(&request, NativeEffectDispatchOutcome::Dispatched)
            .unwrap();
        owner
            .dispatched_effects
            .insert((binding, request.property()), request);

        let retired = owner.freeze_facts(&[]).unwrap();
        let tombstone = &retired.retirement_tombstones()[0];

        assert_eq!(tombstone.binding(), binding);
        assert_eq!(
            tombstone
                .acknowledgement()
                .correlation()
                .and_then(|value| value.user_data().downcast_ref::<&str>()),
            Some(&"destroy-window")
        );
        assert!(owner.dispatched_effects.is_empty());
    }

    #[test]
    fn destroy_dispatch_is_explicitly_limited_to_child_viewports() {
        let child = viewport("destroy-child");

        assert_eq!(
            destroy_dispatch(child, &NativeWindowEffect::Destroy),
            Some((NativeEffectDispatchOutcome::Dispatched, Some(child)))
        );
        assert_eq!(
            destroy_dispatch(ViewportId::ROOT, &NativeWindowEffect::Destroy),
            Some((NativeEffectDispatchOutcome::Unsupported, None))
        );
        assert_eq!(
            destroy_dispatch(child, &NativeWindowEffect::RequestFocus),
            None
        );
    }

    #[test]
    fn native_ingress_must_cover_every_hosted_input() {
        let mut owner = NativePlatformIngressOwner::default();
        let ingress = owner
            .freeze_facts(&[facts(window_id(), viewport("unrelated"), false)])
            .unwrap();
        let root_input = egui::RawInput {
            viewport_id: ViewportId::ROOT,
            ..Default::default()
        };

        let error = crate::HostedViewportCycle::with_native_ingress([root_input], ingress)
            .expect_err("native ingress without the hosted root must fail closed");

        assert!(matches!(
            error,
            crate::HostedViewportCycleError::NativeIngressMissingViewport {
                viewport_id: ViewportId::ROOT,
            }
        ));
    }

    #[test]
    fn frozen_ingress_reaches_begin_before_viewport_ui() {
        let mut owner = NativePlatformIngressOwner::default();
        let ingress = owner
            .freeze_facts(&[facts(window_id(), ViewportId::ROOT, true)])
            .unwrap();
        let expected_generation = ingress.platform().snapshot_generation();
        let cycle = crate::HostedViewportCycle::with_native_ingress(
            [egui::RawInput {
                viewport_id: ViewportId::ROOT,
                ..Default::default()
            }],
            ingress,
        )
        .unwrap();
        let phases = RefCell::new(Vec::new());

        cycle
            .run(
                [ViewportId::ROOT],
                |cycle| {
                    let ingress = cycle
                        .native_host_ingress()
                        .expect("a native cycle must expose its frozen ingress");
                    assert_eq!(
                        ingress.platform().snapshot_generation(),
                        expected_generation
                    );
                    assert_eq!(
                        ingress.platform().inventory()[0].viewport_id(),
                        ViewportId::ROOT
                    );
                    phases.borrow_mut().push("begin");
                    Ok(())
                },
                |input| {
                    assert_eq!(input.viewport_id(), ViewportId::ROOT);
                    assert_eq!(&*phases.borrow(), &["begin"]);
                    phases.borrow_mut().push("ui");
                    Ok(())
                },
                |_| {
                    assert_eq!(&*phases.borrow(), &["begin", "ui"]);
                    phases.borrow_mut().push("end");
                    Ok(())
                },
            )
            .expect("the frozen native cycle should complete");

        assert_eq!(phases.into_inner(), ["begin", "ui", "end"]);
    }

    #[test]
    fn exact_hidden_surface_authorizes_one_staging_presentation() {
        let mut owner = NativePlatformIngressOwner::default();
        let ingress = owner
            .freeze_facts(&[native_staging_facts(ViewportId::ROOT)])
            .unwrap();
        let binding = ingress.platform().inventory()[0];
        let cycle = crate::HostedViewportCycle::with_native_ingress_and_native_staging(
            [egui::RawInput {
                viewport_id: ViewportId::ROOT,
                ..Default::default()
            }],
            ingress,
            [binding],
        )
        .unwrap();
        let authorization = RefCell::new(None);

        let outputs = cycle
            .run(
                [ViewportId::ROOT],
                |cycle| {
                    authorization.replace(Some(
                        cycle
                            .take_native_staging_presentation(ViewportId::ROOT)
                            .expect("the exact hidden surface should authorize staging"),
                    ));
                    assert!(
                        cycle
                            .take_native_staging_presentation(ViewportId::ROOT)
                            .is_err(),
                        "one frozen surface may authorize only one staging presentation"
                    );
                    Ok(())
                },
                |_| Ok(egui::FullOutput::default()),
                |outputs| {
                    let authorization = authorization
                        .take()
                        .expect("the begin hook retained its affine authorization");
                    outputs[0]
                        .validate_native_staging_presentation(&authorization)
                        .expect("prepare accepts the exact output before token installation");
                    outputs[0].output_mut().platform_output.presentation_token =
                        Some(egui::UserData::new("hidden staging"));
                    outputs[0]
                        .authorize_native_staging_presentation(authorization)
                        .expect("the exact output should accept its authorization");
                    Ok(())
                },
            )
            .expect("the authorized hidden cycle should seal");

        assert!(outputs[0].is_native_staging_presentation());
    }

    #[test]
    fn ordinary_hidden_surface_cannot_bypass_not_visible_skip() {
        let mut owner = NativePlatformIngressOwner::default();
        let ingress = owner
            .freeze_facts(&[native_staging_facts(ViewportId::ROOT)])
            .unwrap();
        let cycle = crate::HostedViewportCycle::with_native_ingress(
            [egui::RawInput {
                viewport_id: ViewportId::ROOT,
                ..Default::default()
            }],
            ingress,
        )
        .unwrap();

        assert!(
            cycle
                .take_native_staging_presentation(ViewportId::ROOT)
                .is_err()
        );
    }

    #[test]
    fn native_staging_requires_a_terminal_presentation_token() {
        let mut owner = NativePlatformIngressOwner::default();
        let ingress = owner
            .freeze_facts(&[native_staging_facts(ViewportId::ROOT)])
            .unwrap();
        let binding = ingress.platform().inventory()[0];
        let cycle = crate::HostedViewportCycle::with_native_ingress_and_native_staging(
            [egui::RawInput {
                viewport_id: ViewportId::ROOT,
                ..Default::default()
            }],
            ingress,
            [binding],
        )
        .unwrap();
        let authorization = RefCell::new(None);

        let outputs = cycle
            .run(
                [ViewportId::ROOT],
                |cycle| {
                    authorization.replace(Some(
                        cycle
                            .take_native_staging_presentation(ViewportId::ROOT)
                            .unwrap(),
                    ));
                    Ok(())
                },
                |_| Ok(egui::FullOutput::default()),
                |outputs| {
                    assert!(matches!(
                        outputs[0]
                            .authorize_native_staging_presentation(authorization.take().unwrap()),
                        Err(
                            crate::HostedNativeStagingPresentationError::PresentationTokenMissing {
                                viewport_id: ViewportId::ROOT,
                            }
                        )
                    ));
                    Ok(())
                },
            )
            .expect("rejecting an uncorrelated staging marker keeps the cycle valid");

        assert!(!outputs[0].is_native_staging_presentation());
    }

    #[test]
    fn retired_incarnation_authorization_cannot_mark_recreated_output() {
        let mut owner = NativePlatformIngressOwner::default();
        let first_ingress = owner
            .freeze_facts(&[native_staging_facts(ViewportId::ROOT)])
            .unwrap();
        let first_binding = first_ingress.platform().inventory()[0];
        let first_cycle = crate::HostedViewportCycle::with_native_ingress_and_native_staging(
            [egui::RawInput {
                viewport_id: ViewportId::ROOT,
                ..Default::default()
            }],
            first_ingress,
            [first_binding],
        )
        .unwrap();
        let stale_authorization = RefCell::new(Some(
            first_cycle
                .take_native_staging_presentation(ViewportId::ROOT)
                .unwrap(),
        ));

        owner.freeze_facts(&[]).unwrap();
        let second_ingress = owner
            .freeze_facts(&[native_staging_facts(ViewportId::ROOT)])
            .unwrap();
        let second_binding = second_ingress.platform().inventory()[0];
        assert_ne!(first_binding, second_binding);
        let second_cycle = crate::HostedViewportCycle::with_native_ingress_and_native_staging(
            [egui::RawInput {
                viewport_id: ViewportId::ROOT,
                ..Default::default()
            }],
            second_ingress,
            [second_binding],
        )
        .unwrap();

        let outputs = second_cycle
            .run(
                [ViewportId::ROOT],
                |_| Ok(()),
                |_| {
                    let mut output = egui::FullOutput::default();
                    output.platform_output.presentation_token =
                        Some(egui::UserData::new("later output"));
                    Ok(output)
                },
                |outputs| {
                    assert!(matches!(
                        outputs[0].authorize_native_staging_presentation(
                            stale_authorization.take().unwrap()
                        ),
                        Err(
                            crate::HostedNativeStagingPresentationError::OutputAuthorityMismatch {
                                viewport_id: ViewportId::ROOT,
                            }
                        )
                    ));
                    Ok(())
                },
            )
            .expect("rejecting a stale authorization keeps the later cycle valid");

        assert!(!outputs[0].is_native_staging_presentation());
    }
}
