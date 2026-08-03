//! Incarnation-bound native accessibility actions.

use super::authority::{NativeAuthority, NativeUnavailableReason, NativeViewportBinding};

/// An accessibility action with docking semantics.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NativeAccessibilityAction {
    /// Activate the target as if it were clicked.
    Click,
    /// Move semantic focus to the target.
    Focus,
    /// Increase an adjustable target by one semantic step.
    Increment,
    /// Decrease an adjustable target by one semantic step.
    Decrement,
    /// Reveal the target inside its owning scroll viewport.
    ScrollIntoView,
}

/// One accessibility action delivered to an exact native viewport incarnation.
#[derive(Clone, Debug, PartialEq)]
pub struct NativeAccessibilityEdge {
    binding: NativeViewportBinding,
    target: egui::accesskit::NodeId,
    action: NativeAccessibilityAction,
    presentation: NativeAuthority<egui::PointerHitGraphSnapshot>,
}

impl NativeAccessibilityEdge {
    pub(crate) fn from_accesskit_request(
        binding: NativeViewportBinding,
        request: &egui::accesskit::ActionRequest,
        presentation: Option<egui::PointerHitGraphSnapshot>,
    ) -> Option<Self> {
        if request.target_tree != egui::accesskit::TreeId::ROOT {
            return None;
        }
        let action = match request.action {
            egui::accesskit::Action::Click => NativeAccessibilityAction::Click,
            egui::accesskit::Action::Focus => NativeAccessibilityAction::Focus,
            egui::accesskit::Action::Increment => NativeAccessibilityAction::Increment,
            egui::accesskit::Action::Decrement => NativeAccessibilityAction::Decrement,
            egui::accesskit::Action::ScrollIntoView => NativeAccessibilityAction::ScrollIntoView,
            _ => return None,
        };
        Some(Self {
            binding,
            target: request.target_node,
            action,
            presentation: presentation.map_or_else(
                || NativeAuthority::unknown(NativeUnavailableReason::NotObserved),
                NativeAuthority::known,
            ),
        })
    }

    /// Return the exact viewport incarnation that delivered this action.
    pub const fn binding(&self) -> NativeViewportBinding {
        self.binding
    }

    /// Return the AccessKit node targeted by the action.
    pub const fn target(&self) -> egui::accesskit::NodeId {
        self.target
    }

    /// Return the typed docking action.
    pub const fn action(&self) -> NativeAccessibilityAction {
        self.action
    }

    /// Return the exact presented widget graph visible when this action arrived.
    pub const fn presentation(&self) -> &NativeAuthority<egui::PointerHitGraphSnapshot> {
        &self.presentation
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::native::platform_provider::NativeViewportIncarnation;

    fn binding() -> NativeViewportBinding {
        NativeViewportBinding::new(
            egui::ViewportId::from_hash_of("native-accessibility"),
            NativeViewportIncarnation::new(1),
        )
    }

    fn request(action: egui::accesskit::Action) -> egui::accesskit::ActionRequest {
        egui::accesskit::ActionRequest {
            action,
            target_tree: egui::accesskit::TreeId::ROOT,
            target_node: egui::accesskit::NodeId(41),
            data: None,
        }
    }

    #[test]
    fn supported_actions_remain_typed_and_targeted() {
        let cases = [
            (
                egui::accesskit::Action::Click,
                NativeAccessibilityAction::Click,
            ),
            (
                egui::accesskit::Action::Focus,
                NativeAccessibilityAction::Focus,
            ),
            (
                egui::accesskit::Action::Increment,
                NativeAccessibilityAction::Increment,
            ),
            (
                egui::accesskit::Action::Decrement,
                NativeAccessibilityAction::Decrement,
            ),
            (
                egui::accesskit::Action::ScrollIntoView,
                NativeAccessibilityAction::ScrollIntoView,
            ),
        ];
        for (action, expected) in cases {
            let edge =
                NativeAccessibilityEdge::from_accesskit_request(binding(), &request(action), None)
                    .expect("the action has docking semantics");
            assert_eq!(edge.binding(), binding());
            assert_eq!(edge.target(), egui::accesskit::NodeId(41));
            assert_eq!(edge.action(), expected);
            assert!(edge.presentation().value().is_none());
        }
    }

    #[test]
    fn unrelated_actions_are_not_provider_facts() {
        assert!(
            NativeAccessibilityEdge::from_accesskit_request(
                binding(),
                &request(egui::accesskit::Action::Blur),
                None,
            )
            .is_none()
        );
    }
}
