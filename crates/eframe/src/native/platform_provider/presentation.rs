//! Incarnation-bound presentation and retirement terminal facts.

use super::{
    authority::{NativePlatformError, NativePlatformGeneration, NativeViewportBinding},
    effect::NativeEffectAcknowledgement,
};

/// A coordinator-minted identity for one native presentation attempt.
///
/// Serials are process-local and monotonically increasing. Together with the
/// exact [`NativeViewportBinding`] they prevent a renderer result for an old
/// viewport incarnation from affecting a replacement incarnation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NativePresentationSerial(u64);

impl NativePresentationSerial {
    pub(super) const fn new(value: u64) -> Self {
        Self(value)
    }

    /// Return the monotonically increasing numeric value.
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// A terminal native viewport retirement record.
#[derive(Clone, Debug, PartialEq)]
pub struct NativeRetirementTombstone {
    pub(super) binding: NativeViewportBinding,
    pub(super) generation: NativePlatformGeneration,
    pub(super) acknowledgement: NativeEffectAcknowledgement,
}

impl NativeRetirementTombstone {
    /// Return the exact retired viewport lifetime.
    pub const fn binding(&self) -> NativeViewportBinding {
        self.binding
    }

    /// Return the inventory generation that retired it.
    pub const fn generation(&self) -> NativePlatformGeneration {
        self.generation
    }

    /// Return the lifecycle effect acknowledged by this retirement.
    pub const fn acknowledgement(&self) -> &NativeEffectAcknowledgement {
        &self.acknowledgement
    }
}

/// A renderer result bound to the exact viewport lifetime that requested presentation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NativePresentationResult {
    pub(super) binding: NativeViewportBinding,
    pub(super) serial: NativePresentationSerial,
    pub(super) result: egui::PresentationResult,
}

impl NativePresentationResult {
    /// Return the exact viewport lifetime that issued the presentation.
    pub const fn binding(&self) -> NativeViewportBinding {
        self.binding
    }

    /// Return the presentation attempt settled by this renderer result.
    pub const fn serial(&self) -> NativePresentationSerial {
        self.serial
    }

    /// Return the original renderer result.
    pub const fn result(&self) -> &egui::PresentationResult {
        &self.result
    }
}

/// Proof that every ingress route for one retired viewport is quiescent.
///
/// This fact does not retire the native viewport. [`NativeRetirementTombstone`]
/// does that. It proves that the Winit owner no longer retains pointer, scroll,
/// effect, close, or window routes for the exact binding, and that every
/// coordinator-owned effect and presentation lane is terminal. The provider
/// may therefore reclaim the binding's retained ABA guards after the host
/// commits the enclosing ingress batch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NativeBindingIngressQuiesced {
    pub(super) binding: NativeViewportBinding,
    pub(super) retirement_generation: NativePlatformGeneration,
    pub(super) last_started_presentation: Option<NativePresentationSerial>,
}

impl NativeBindingIngressQuiesced {
    /// Return the exact retired viewport lifetime whose ingress is quiescent.
    pub const fn binding(self) -> NativeViewportBinding {
        self.binding
    }

    /// Return the inventory generation that retired the viewport.
    pub const fn retirement_generation(self) -> NativePlatformGeneration {
        self.retirement_generation
    }

    /// Return the final presentation serial ever minted for this binding.
    ///
    /// `None` means the viewport retired without starting a tracked
    /// presentation.
    pub const fn last_started_presentation(self) -> Option<NativePresentationSerial> {
        self.last_started_presentation
    }
}

#[derive(Debug)]
pub(crate) struct NativePresentationTicket {
    pub(super) binding: NativeViewportBinding,
    pub(super) serial: NativePresentationSerial,
}

impl NativePresentationTicket {
    pub(super) fn complete(
        self,
        result: egui::PresentationResult,
    ) -> Result<NativePresentationResult, NativePlatformError> {
        if result.viewport_id() != self.binding.viewport_id {
            return Err(NativePlatformError::PresentationViewportMismatch);
        }

        Ok(NativePresentationResult {
            binding: self.binding,
            serial: self.serial,
            result,
        })
    }
}
