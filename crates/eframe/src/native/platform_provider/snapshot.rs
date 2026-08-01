//! Complete native window and platform snapshots.

use super::{
    authority::{
        NativeAuthority, NativeCaptureOwner, NativeCloseState, NativeFocusedWindow,
        NativeHoveredWindow, NativePhysicalRect, NativePlatformGeneration, NativePointerInputState,
        NativePresentationState, NativeViewportBinding,
    },
    effect::NativePropertyObservation,
    work_area::NativeWorkAreaRosterObservation,
};

/// Whether the active native backend can authoritatively provide one capability.
///
/// `Unknown` is distinct from `Unsupported`: it means the event-loop integration could not
/// identify or validate the backend contract for the frozen snapshot. Both states require
/// consumers to fail closed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum NativeBackendCapability {
    /// The active backend has an exact implementation for this capability.
    Supported,
    /// The active backend is known not to implement this capability.
    Unsupported,
    /// The active backend contract could not be proved.
    #[default]
    Unknown,
}

impl NativeBackendCapability {
    pub(crate) const fn intersect(self, other: Self) -> Self {
        match (self, other) {
            (Self::Unsupported, _) | (_, Self::Unsupported) => Self::Unsupported,
            (Self::Unknown, _) | (_, Self::Unknown) => Self::Unknown,
            (Self::Supported, Self::Supported) => Self::Supported,
        }
    }
}

/// Complete native backend capability roster frozen with one platform snapshot.
///
/// Visibility is separate from object lifecycle because a compositor may permit creating and
/// destroying windows without exposing authoritative show/hide state. Global placement requires
/// both control and observation in one desktop-global physical coordinate space.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct NativeBackendCapabilities {
    window_lifecycle: NativeBackendCapability,
    window_visibility: NativeBackendCapability,
    authoritative_inventory: NativeBackendCapability,
    hovered_window: NativeBackendCapability,
    desktop_pointer_position: NativeBackendCapability,
    authoritative_button_state: NativeBackendCapability,
    global_window_placement: NativeBackendCapability,
    pointer_hit_test_observation: NativeBackendCapability,
    pointer_hit_test_control: NativeBackendCapability,
    global_focus_observation: NativeBackendCapability,
    window_activation_control: NativeBackendCapability,
    close_cancellation: NativeBackendCapability,
}

impl NativeBackendCapabilities {
    /// Create the provider roster for one identified native window backend.
    ///
    /// Inventory, pointer-button edges, and child-close cancellation are owned by the eframe
    /// coordinator and do not depend on an operating-system probe. The remaining capabilities
    /// stay unknown until the backend probe supplies their exact contracts.
    pub(crate) const fn new(
        window_lifecycle: NativeBackendCapability,
        window_visibility: NativeBackendCapability,
        global_window_placement: NativeBackendCapability,
    ) -> Self {
        Self {
            window_lifecycle,
            window_visibility,
            authoritative_inventory: NativeBackendCapability::Supported,
            hovered_window: NativeBackendCapability::Unknown,
            desktop_pointer_position: NativeBackendCapability::Unknown,
            authoritative_button_state: NativeBackendCapability::Supported,
            global_window_placement,
            pointer_hit_test_observation: NativeBackendCapability::Unknown,
            pointer_hit_test_control: NativeBackendCapability::Unknown,
            global_focus_observation: NativeBackendCapability::Unknown,
            window_activation_control: NativeBackendCapability::Unknown,
            close_cancellation: NativeBackendCapability::Supported,
        }
    }

    pub(crate) const fn with_pointer_routing(
        mut self,
        hovered_window: NativeBackendCapability,
        desktop_pointer_position: NativeBackendCapability,
    ) -> Self {
        self.hovered_window = hovered_window;
        self.desktop_pointer_position = desktop_pointer_position;
        self
    }

    pub(crate) const fn with_pointer_hit_test(
        mut self,
        observation: NativeBackendCapability,
        control: NativeBackendCapability,
    ) -> Self {
        self.pointer_hit_test_observation = observation;
        self.pointer_hit_test_control = control;
        self
    }

    pub(crate) const fn with_focus(
        mut self,
        observation: NativeBackendCapability,
        activation_control: NativeBackendCapability,
    ) -> Self {
        self.global_focus_observation = observation;
        self.window_activation_control = activation_control;
        self
    }

    pub(crate) const fn intersect(self, other: Self) -> Self {
        Self {
            window_lifecycle: self.window_lifecycle.intersect(other.window_lifecycle),
            window_visibility: self.window_visibility.intersect(other.window_visibility),
            authoritative_inventory: self
                .authoritative_inventory
                .intersect(other.authoritative_inventory),
            hovered_window: self.hovered_window.intersect(other.hovered_window),
            desktop_pointer_position: self
                .desktop_pointer_position
                .intersect(other.desktop_pointer_position),
            authoritative_button_state: self
                .authoritative_button_state
                .intersect(other.authoritative_button_state),
            global_window_placement: self
                .global_window_placement
                .intersect(other.global_window_placement),
            pointer_hit_test_observation: self
                .pointer_hit_test_observation
                .intersect(other.pointer_hit_test_observation),
            pointer_hit_test_control: self
                .pointer_hit_test_control
                .intersect(other.pointer_hit_test_control),
            global_focus_observation: self
                .global_focus_observation
                .intersect(other.global_focus_observation),
            window_activation_control: self
                .window_activation_control
                .intersect(other.window_activation_control),
            close_cancellation: self.close_cancellation.intersect(other.close_cancellation),
        }
    }

    /// Return native window create/destroy lifecycle support.
    pub const fn window_lifecycle(self) -> NativeBackendCapability {
        self.window_lifecycle
    }

    /// Return exact native show/hide control and observation support.
    pub const fn window_visibility(self) -> NativeBackendCapability {
        self.window_visibility
    }

    /// Return complete native viewport inventory support.
    pub const fn authoritative_inventory(self) -> NativeBackendCapability {
        self.authoritative_inventory
    }

    /// Return desktop-global hovered-window observation support.
    pub const fn hovered_window(self) -> NativeBackendCapability {
        self.hovered_window
    }

    /// Return desktop-global physical pointer-position support.
    pub const fn desktop_pointer_position(self) -> NativeBackendCapability {
        self.desktop_pointer_position
    }

    /// Return exact pointer-button edge support.
    pub const fn authoritative_button_state(self) -> NativeBackendCapability {
        self.authoritative_button_state
    }

    /// Return exact desktop-global placement control and observation support.
    pub const fn global_window_placement(self) -> NativeBackendCapability {
        self.global_window_placement
    }

    /// Return native pointer hit-test state observation support.
    pub const fn pointer_hit_test_observation(self) -> NativeBackendCapability {
        self.pointer_hit_test_observation
    }

    /// Return native pointer hit-test state control support.
    pub const fn pointer_hit_test_control(self) -> NativeBackendCapability {
        self.pointer_hit_test_control
    }

    /// Return complete desktop-global focus observation support.
    pub const fn global_focus_observation(self) -> NativeBackendCapability {
        self.global_focus_observation
    }

    /// Return native window activation request support.
    pub const fn window_activation_control(self) -> NativeBackendCapability {
        self.window_activation_control
    }

    /// Return child-window close cancellation support.
    pub const fn close_cancellation(self) -> NativeBackendCapability {
        self.close_cancellation
    }
}

/// The generation advanced only by a successful atomic platform snapshot freeze.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NativePlatformSnapshotGeneration(u64);

impl NativePlatformSnapshotGeneration {
    pub(crate) const fn new(value: u64) -> Self {
        Self(value)
    }

    /// Return the monotonically increasing numeric value.
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// One global Known/Unknown fact bound to an exact successful snapshot generation.
#[derive(Clone, Debug, PartialEq)]
pub struct NativeGlobalObservation<T> {
    generation: NativePlatformSnapshotGeneration,
    authority: NativeAuthority<T>,
}

impl<T> NativeGlobalObservation<T> {
    pub(super) const fn new(
        generation: NativePlatformSnapshotGeneration,
        authority: NativeAuthority<T>,
    ) -> Self {
        Self {
            generation,
            authority,
        }
    }

    /// Return the successful snapshot generation that observed this fact.
    pub const fn generation(&self) -> NativePlatformSnapshotGeneration {
        self.generation
    }

    /// Return the Known/Unknown authority observed in that generation.
    pub const fn authority(&self) -> &NativeAuthority<T> {
        &self.authority
    }
}

/// A complete inventory generation and its global native authority.
#[derive(Clone, Debug, PartialEq)]
pub struct NativePlatformFacts {
    pub(super) inventory_generation: NativePlatformGeneration,
    pub(super) snapshot_generation: NativePlatformSnapshotGeneration,
    pub(super) inventory: Vec<NativeViewportBinding>,
    pub(super) focused: NativeGlobalObservation<NativeFocusedWindow>,
    pub(super) hovered: NativeGlobalObservation<NativeHoveredWindow>,
    pub(super) capture: NativeGlobalObservation<NativeCaptureOwner>,
    pub(super) capabilities: NativeBackendCapabilities,
    pub(super) work_areas: NativeWorkAreaRosterObservation,
}

impl NativePlatformFacts {
    /// Return the complete-inventory generation.
    pub const fn generation(&self) -> NativePlatformGeneration {
        self.inventory_generation
    }

    /// Return the complete-inventory generation.
    pub const fn inventory_generation(&self) -> NativePlatformGeneration {
        self.inventory_generation
    }

    /// Return the generation advanced by the successful atomic freeze.
    pub const fn snapshot_generation(&self) -> NativePlatformSnapshotGeneration {
        self.snapshot_generation
    }

    /// Return the complete set of active native viewport bindings.
    pub fn inventory(&self) -> &[NativeViewportBinding] {
        &self.inventory
    }

    /// Return global keyboard-focus authority.
    pub const fn focused(&self) -> &NativeAuthority<NativeFocusedWindow> {
        self.focused.authority()
    }

    /// Return focus authority bound to the snapshot generation that observed it.
    pub const fn focused_observation(&self) -> &NativeGlobalObservation<NativeFocusedWindow> {
        &self.focused
    }

    /// Return global hover authority.
    pub const fn hovered(&self) -> &NativeAuthority<NativeHoveredWindow> {
        self.hovered.authority()
    }

    /// Return hover authority bound to the snapshot generation that observed it.
    pub const fn hovered_observation(&self) -> &NativeGlobalObservation<NativeHoveredWindow> {
        &self.hovered
    }

    /// Return global pointer-capture authority.
    pub const fn capture(&self) -> &NativeAuthority<NativeCaptureOwner> {
        self.capture.authority()
    }

    /// Return capture authority bound to the snapshot generation that observed it.
    pub const fn capture_observation(&self) -> &NativeGlobalObservation<NativeCaptureOwner> {
        &self.capture
    }

    /// Return the native backend capabilities frozen with this snapshot.
    pub const fn capabilities(&self) -> NativeBackendCapabilities {
        self.capabilities
    }

    /// Return the complete native monitor work-area roster.
    pub const fn work_areas(&self) -> &NativeWorkAreaRosterObservation {
        &self.work_areas
    }
}

/// The physical geometry facts for one native viewport.
#[derive(Clone, Debug, PartialEq)]
pub struct NativeWindowGeometry {
    pub(super) binding: NativeViewportBinding,
    pub(super) content_rect: NativeAuthority<NativePhysicalRect>,
    pub(super) outer_rect: NativeAuthority<NativePhysicalRect>,
    pub(super) native_scale_factor: NativeAuthority<f64>,
    pub(super) presentation_scale_factor: NativeAuthority<f64>,
    pub(super) work_area: NativeAuthority<NativePhysicalRect>,
}

impl NativeWindowGeometry {
    /// Return the exact viewport lifetime described.
    pub const fn binding(&self) -> NativeViewportBinding {
        self.binding
    }

    /// Return desktop-global content geometry.
    pub const fn content_rect(&self) -> &NativeAuthority<NativePhysicalRect> {
        &self.content_rect
    }

    /// Return desktop-global outer geometry.
    pub const fn outer_rect(&self) -> &NativeAuthority<NativePhysicalRect> {
        &self.outer_rect
    }

    /// Return the native scale factor.
    pub const fn native_scale_factor(&self) -> &NativeAuthority<f64> {
        &self.native_scale_factor
    }

    /// Return the physical-pixel scale of the presented egui coordinate system.
    pub const fn presentation_scale_factor(&self) -> &NativeAuthority<f64> {
        &self.presentation_scale_factor
    }

    /// Return the usable desktop work area, if the platform proves it.
    pub const fn work_area(&self) -> &NativeAuthority<NativePhysicalRect> {
        &self.work_area
    }
}

/// A complete set of native observations for one exact viewport lifetime.
#[derive(Clone, Debug, PartialEq)]
pub struct NativeWindowSnapshot {
    pub(super) binding: NativeViewportBinding,
    pub(super) platform_generation: NativePlatformGeneration,
    pub(super) geometry: NativePropertyObservation<NativeWindowGeometry>,
    pub(super) presentation: NativePropertyObservation<NativePresentationState>,
    pub(super) pointer_input: NativePropertyObservation<NativePointerInputState>,
    pub(super) focus: NativePropertyObservation<bool>,
    pub(super) close: NativePropertyObservation<NativeCloseState>,
}

impl NativeWindowSnapshot {
    /// Return the exact viewport lifetime described by every observation.
    pub const fn binding(&self) -> NativeViewportBinding {
        self.binding
    }

    /// Return the inventory generation against which this snapshot was measured.
    pub const fn platform_generation(&self) -> NativePlatformGeneration {
        self.platform_generation
    }

    /// Return the inventory generation against which this snapshot was measured.
    pub const fn inventory_generation(&self) -> NativePlatformGeneration {
        self.platform_generation
    }

    /// Return native geometry authority.
    pub const fn geometry(&self) -> &NativePropertyObservation<NativeWindowGeometry> {
        &self.geometry
    }

    /// Return native presentation authority.
    pub const fn presentation(&self) -> &NativePropertyObservation<NativePresentationState> {
        &self.presentation
    }

    /// Return native pointer-input authority.
    pub const fn pointer_input(&self) -> &NativePropertyObservation<NativePointerInputState> {
        &self.pointer_input
    }

    /// Return whether this exact native viewport owns keyboard focus.
    pub const fn focus(&self) -> &NativePropertyObservation<bool> {
        &self.focus
    }

    /// Return native close-state authority.
    pub const fn close(&self) -> &NativePropertyObservation<NativeCloseState> {
        &self.close
    }
}

/// A complete native platform snapshot for one inventory generation.
#[derive(Clone, Debug, PartialEq)]
pub struct NativePlatformSnapshot {
    pub(super) facts: NativePlatformFacts,
    pub(super) windows: Vec<NativeWindowSnapshot>,
}

impl NativePlatformSnapshot {
    /// Return the complete-inventory generation.
    pub const fn generation(&self) -> NativePlatformGeneration {
        self.facts.generation()
    }

    /// Return the complete-inventory generation.
    pub const fn inventory_generation(&self) -> NativePlatformGeneration {
        self.facts.inventory_generation()
    }

    /// Return the generation advanced by this successful atomic freeze.
    pub const fn snapshot_generation(&self) -> NativePlatformSnapshotGeneration {
        self.facts.snapshot_generation()
    }

    /// Return the exact active viewport roster.
    pub fn inventory(&self) -> &[NativeViewportBinding] {
        self.facts.inventory()
    }

    /// Return global keyboard-focus authority.
    pub const fn focused(&self) -> &NativeAuthority<NativeFocusedWindow> {
        self.facts.focused()
    }

    /// Return focus authority bound to the snapshot generation that observed it.
    pub const fn focused_observation(&self) -> &NativeGlobalObservation<NativeFocusedWindow> {
        self.facts.focused_observation()
    }

    /// Return global hover authority.
    pub const fn hovered(&self) -> &NativeAuthority<NativeHoveredWindow> {
        self.facts.hovered()
    }

    /// Return hover authority bound to the snapshot generation that observed it.
    pub const fn hovered_observation(&self) -> &NativeGlobalObservation<NativeHoveredWindow> {
        self.facts.hovered_observation()
    }

    /// Return global pointer-capture authority.
    pub const fn capture(&self) -> &NativeAuthority<NativeCaptureOwner> {
        self.facts.capture()
    }

    /// Return capture authority bound to the snapshot generation that observed it.
    pub const fn capture_observation(&self) -> &NativeGlobalObservation<NativeCaptureOwner> {
        self.facts.capture_observation()
    }

    /// Return the native backend capabilities frozen with this snapshot.
    pub const fn capabilities(&self) -> NativeBackendCapabilities {
        self.facts.capabilities()
    }

    /// Return the complete native monitor work-area roster frozen with this snapshot.
    pub const fn work_areas(&self) -> &NativeWorkAreaRosterObservation {
        self.facts.work_areas()
    }

    /// Return whether this snapshot can authorize the supplied global observation.
    pub const fn authorizes_global_observation<T>(
        &self,
        observation: &NativeGlobalObservation<T>,
    ) -> bool {
        self.snapshot_generation().get() == observation.generation().get()
    }

    /// Return one complete window snapshot per active binding, in roster order.
    pub fn windows(&self) -> &[NativeWindowSnapshot] {
        &self.windows
    }
}

#[derive(Debug, PartialEq)]
pub(super) struct PendingGlobalFacts {
    pub(super) focused: NativeAuthority<NativeFocusedWindow>,
    pub(super) hovered: NativeAuthority<NativeHoveredWindow>,
    pub(super) capture: NativeAuthority<NativeCaptureOwner>,
    pub(super) capabilities: NativeBackendCapabilities,
    pub(super) work_areas: NativeWorkAreaRosterObservation,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn intersection_covers_every_capability_field() {
        use NativeBackendCapability::{Supported, Unsupported};

        let complete = NativeBackendCapabilities::new(Supported, Supported, Supported)
            .with_pointer_routing(Supported, Supported)
            .with_pointer_hit_test(Supported, Supported)
            .with_focus(Supported, Supported);
        let degraded = NativeBackendCapabilities::new(Supported, Unsupported, Unsupported)
            .with_pointer_routing(Unsupported, Unsupported)
            .with_pointer_hit_test(Unsupported, Supported)
            .with_focus(Unsupported, Unsupported);
        let intersection = complete.intersect(degraded);

        assert_eq!(intersection.window_lifecycle(), Supported);
        assert_eq!(intersection.window_visibility(), Unsupported);
        assert_eq!(intersection.authoritative_inventory(), Supported);
        assert_eq!(intersection.hovered_window(), Unsupported);
        assert_eq!(intersection.desktop_pointer_position(), Unsupported);
        assert_eq!(intersection.authoritative_button_state(), Supported);
        assert_eq!(intersection.global_window_placement(), Unsupported);
        assert_eq!(intersection.pointer_hit_test_observation(), Unsupported);
        assert_eq!(intersection.pointer_hit_test_control(), Supported);
        assert_eq!(intersection.global_focus_observation(), Unsupported);
        assert_eq!(intersection.window_activation_control(), Unsupported);
        assert_eq!(intersection.close_cancellation(), Supported);
    }
}
