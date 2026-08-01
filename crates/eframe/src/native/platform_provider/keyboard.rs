//! Incarnation-bound native keyboard facts.

use super::authority::{NativeAuthority, NativeUnavailableReason, NativeViewportBinding};

/// A keyboard key with native docking semantics.
///
/// The provider deliberately exposes only keys consumed by the native host
/// protocol. Text entry and application shortcuts continue through egui's raw
/// input path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NativeKey {
    /// Cancel the currently active docking interaction.
    Escape,
    /// Move semantic focus or adjust a control toward the left.
    ArrowLeft,
    /// Move semantic focus or adjust a control toward the right.
    ArrowRight,
    /// Move semantic focus or adjust a control upward.
    ArrowUp,
    /// Move semantic focus or adjust a control downward.
    ArrowDown,
    /// Move semantic focus to the first ordered receiver.
    Home,
    /// Move semantic focus to the last ordered receiver.
    End,
    /// Activate the focused receiver.
    Enter,
    /// Activate the focused receiver.
    Space,
}

/// The physical transition represented by one native key edge.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NativeKeyEdgeKind {
    /// The key transitioned from released to pressed.
    Pressed,
    /// The platform repeated a key that remains pressed.
    Repeated,
    /// The key transitioned from pressed to released.
    Released,
}

/// One keyboard transition delivered to an exact native viewport incarnation.
#[derive(Clone, Debug, PartialEq)]
pub struct NativeKeyEdge {
    binding: NativeViewportBinding,
    key: NativeKey,
    kind: NativeKeyEdgeKind,
    presentation: NativeAuthority<egui::PointerHitGraphSnapshot>,
}

impl NativeKeyEdge {
    pub(super) fn new(
        binding: NativeViewportBinding,
        key: NativeKey,
        kind: NativeKeyEdgeKind,
    ) -> Self {
        Self {
            binding,
            key,
            kind,
            presentation: NativeAuthority::unknown(NativeUnavailableReason::NotObserved),
        }
    }

    pub(crate) fn from_winit_event(
        binding: NativeViewportBinding,
        event: &winit::event::KeyEvent,
        is_synthetic: bool,
        presentation: Option<egui::PointerHitGraphSnapshot>,
    ) -> Option<Self> {
        let mut edge = classify_winit_key_edge(
            binding,
            &event.logical_key,
            event.physical_key,
            event.state,
            event.repeat,
            is_synthetic,
        )?;
        edge.presentation = presentation.map_or_else(
            || NativeAuthority::unknown(NativeUnavailableReason::NotObserved),
            NativeAuthority::known,
        );
        Some(edge)
    }

    /// Return the exact viewport incarnation that delivered this edge.
    pub const fn binding(&self) -> NativeViewportBinding {
        self.binding
    }

    /// Return the key represented by this edge.
    pub const fn key(&self) -> NativeKey {
        self.key
    }

    /// Return the physical transition represented by this edge.
    pub const fn kind(&self) -> NativeKeyEdgeKind {
        self.kind
    }

    /// Return the exact presented widget graph visible when this edge arrived.
    pub const fn presentation(&self) -> &NativeAuthority<egui::PointerHitGraphSnapshot> {
        &self.presentation
    }
}

fn classify_winit_key_edge(
    binding: NativeViewportBinding,
    logical_key: &winit::keyboard::Key,
    physical_key: winit::keyboard::PhysicalKey,
    state: winit::event::ElementState,
    repeat: bool,
    is_synthetic: bool,
) -> Option<NativeKeyEdge> {
    if is_synthetic {
        return None;
    }

    let key = classify_native_key(logical_key, physical_key)?;

    let kind = match (state, repeat) {
        (winit::event::ElementState::Pressed, false) => NativeKeyEdgeKind::Pressed,
        (winit::event::ElementState::Pressed, true) => NativeKeyEdgeKind::Repeated,
        (winit::event::ElementState::Released, _) => NativeKeyEdgeKind::Released,
    };
    Some(NativeKeyEdge::new(binding, key, kind))
}

fn classify_native_key(
    logical_key: &winit::keyboard::Key,
    physical_key: winit::keyboard::PhysicalKey,
) -> Option<NativeKey> {
    use winit::keyboard::{Key, KeyCode, NamedKey, PhysicalKey};

    let logical = match logical_key {
        Key::Named(NamedKey::Escape) => Some(NativeKey::Escape),
        Key::Named(NamedKey::ArrowLeft) => Some(NativeKey::ArrowLeft),
        Key::Named(NamedKey::ArrowRight) => Some(NativeKey::ArrowRight),
        Key::Named(NamedKey::ArrowUp) => Some(NativeKey::ArrowUp),
        Key::Named(NamedKey::ArrowDown) => Some(NativeKey::ArrowDown),
        Key::Named(NamedKey::Home) => Some(NativeKey::Home),
        Key::Named(NamedKey::End) => Some(NativeKey::End),
        Key::Named(NamedKey::Enter) => Some(NativeKey::Enter),
        Key::Named(NamedKey::Space) => Some(NativeKey::Space),
        _ => None,
    };
    logical.or_else(|| match physical_key {
        PhysicalKey::Code(KeyCode::Escape) => Some(NativeKey::Escape),
        PhysicalKey::Code(KeyCode::ArrowLeft) => Some(NativeKey::ArrowLeft),
        PhysicalKey::Code(KeyCode::ArrowRight) => Some(NativeKey::ArrowRight),
        PhysicalKey::Code(KeyCode::ArrowUp) => Some(NativeKey::ArrowUp),
        PhysicalKey::Code(KeyCode::ArrowDown) => Some(NativeKey::ArrowDown),
        PhysicalKey::Code(KeyCode::Home) => Some(NativeKey::Home),
        PhysicalKey::Code(KeyCode::End) => Some(NativeKey::End),
        PhysicalKey::Code(KeyCode::Enter | KeyCode::NumpadEnter) => Some(NativeKey::Enter),
        PhysicalKey::Code(KeyCode::Space) => Some(NativeKey::Space),
        _ => None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::native::platform_provider::NativeViewportIncarnation;

    fn binding() -> NativeViewportBinding {
        NativeViewportBinding::new(
            egui::ViewportId::from_hash_of("native-key"),
            NativeViewportIncarnation::new(1),
        )
    }

    fn classify(
        state: winit::event::ElementState,
        repeat: bool,
        is_synthetic: bool,
    ) -> Option<NativeKeyEdge> {
        classify_winit_key_edge(
            binding(),
            &winit::keyboard::Key::Named(winit::keyboard::NamedKey::Escape),
            winit::keyboard::PhysicalKey::Code(winit::keyboard::KeyCode::Escape),
            state,
            repeat,
            is_synthetic,
        )
    }

    #[test]
    fn escape_press_repeat_and_release_remain_distinct() {
        assert_eq!(
            classify(winit::event::ElementState::Pressed, false, false).map(|edge| edge.kind()),
            Some(NativeKeyEdgeKind::Pressed)
        );
        assert_eq!(
            classify(winit::event::ElementState::Pressed, true, false).map(|edge| edge.kind()),
            Some(NativeKeyEdgeKind::Repeated)
        );
        assert_eq!(
            classify(winit::event::ElementState::Released, false, false).map(|edge| edge.kind()),
            Some(NativeKeyEdgeKind::Released)
        );
    }

    #[test]
    fn physical_escape_is_a_fallback_for_an_unmapped_logical_key() {
        let edge = classify_winit_key_edge(
            binding(),
            &winit::keyboard::Key::Unidentified(winit::keyboard::NativeKey::Unidentified),
            winit::keyboard::PhysicalKey::Code(winit::keyboard::KeyCode::Escape),
            winit::event::ElementState::Pressed,
            false,
            false,
        )
        .expect("physical Escape remains authoritative when the logical key is unavailable");

        assert_eq!(edge.key(), NativeKey::Escape);
        assert_eq!(edge.binding(), binding());
    }

    #[test]
    fn synthetic_and_unrelated_keys_do_not_mint_physical_edges() {
        assert_eq!(
            classify(winit::event::ElementState::Pressed, false, true),
            None
        );
        assert_eq!(
            classify_winit_key_edge(
                binding(),
                &winit::keyboard::Key::Named(winit::keyboard::NamedKey::F1),
                winit::keyboard::PhysicalKey::Code(winit::keyboard::KeyCode::F1),
                winit::event::ElementState::Pressed,
                false,
                false,
            ),
            None
        );
    }

    #[test]
    fn docking_semantic_keys_are_preserved_as_typed_edges() {
        use winit::keyboard::{Key, KeyCode, NamedKey, PhysicalKey};

        let cases = [
            (
                NamedKey::ArrowLeft,
                KeyCode::ArrowLeft,
                NativeKey::ArrowLeft,
            ),
            (
                NamedKey::ArrowRight,
                KeyCode::ArrowRight,
                NativeKey::ArrowRight,
            ),
            (NamedKey::ArrowUp, KeyCode::ArrowUp, NativeKey::ArrowUp),
            (
                NamedKey::ArrowDown,
                KeyCode::ArrowDown,
                NativeKey::ArrowDown,
            ),
            (NamedKey::Home, KeyCode::Home, NativeKey::Home),
            (NamedKey::End, KeyCode::End, NativeKey::End),
            (NamedKey::Enter, KeyCode::Enter, NativeKey::Enter),
            (NamedKey::Space, KeyCode::Space, NativeKey::Space),
        ];
        for (logical, physical, expected) in cases {
            assert_eq!(
                classify_native_key(&Key::Named(logical), PhysicalKey::Code(physical)),
                Some(expected)
            );
        }
    }
}
