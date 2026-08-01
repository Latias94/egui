//! Stable native monitor work-area identities and observations.

use super::authority::{NativeAuthority, NativePhysicalRect};

macro_rules! work_area_id {
    ($name:ident, $doc:literal) => {
        #[doc = $doc]
        #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(u64);

        impl $name {
            pub(crate) const fn new(value: u64) -> Self {
                Self(value)
            }

            /// Return the provider-local protocol value.
            pub const fn get(self) -> u64 {
                self.0
            }
        }
    };
}

work_area_id!(
    NativeWorkAreaToken,
    "A provider-minted stable identity for one native monitor work area."
);
work_area_id!(
    NativeWorkAreaGeneration,
    "The semantic generation of the complete native monitor work-area roster."
);

/// One selectable native monitor work area in desktop-global physical pixels.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct NativeWorkArea {
    pub(super) token: NativeWorkAreaToken,
    pub(super) bounds: NativePhysicalRect,
    pub(super) scale_factor: f64,
}

impl NativeWorkArea {
    pub(crate) const fn new(
        token: NativeWorkAreaToken,
        bounds: NativePhysicalRect,
        scale_factor: f64,
    ) -> Self {
        Self {
            token,
            bounds,
            scale_factor,
        }
    }

    /// Return the stable provider-local monitor identity.
    pub const fn token(self) -> NativeWorkAreaToken {
        self.token
    }

    /// Return the usable monitor rectangle in desktop-global physical pixels.
    pub const fn bounds(self) -> NativePhysicalRect {
        self.bounds
    }

    /// Return the monitor's native physical-pixel scale.
    pub const fn scale_factor(self) -> f64 {
        self.scale_factor
    }
}

/// One complete Known/Unknown native work-area roster.
#[derive(Clone, Debug, PartialEq)]
pub struct NativeWorkAreaRosterObservation {
    pub(super) generation: NativeWorkAreaGeneration,
    pub(super) roster: NativeAuthority<Vec<NativeWorkArea>>,
}

impl NativeWorkAreaRosterObservation {
    pub(crate) const fn new(
        generation: NativeWorkAreaGeneration,
        roster: NativeAuthority<Vec<NativeWorkArea>>,
    ) -> Self {
        Self { generation, roster }
    }

    /// Return the semantic roster generation sampled by this observation.
    pub const fn generation(&self) -> NativeWorkAreaGeneration {
        self.generation
    }

    /// Return the complete Known/Unknown roster authority.
    pub const fn roster(&self) -> &NativeAuthority<Vec<NativeWorkArea>> {
        &self.roster
    }
}

/// Event-time selection of one work area from an exact native roster generation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NativeWorkAreaRoute {
    generation: NativeWorkAreaGeneration,
    token: NativeWorkAreaToken,
}

impl NativeWorkAreaRoute {
    pub(crate) const fn new(
        generation: NativeWorkAreaGeneration,
        token: NativeWorkAreaToken,
    ) -> Self {
        Self { generation, token }
    }

    /// Return the exact native roster generation used for selection.
    pub const fn generation(self) -> NativeWorkAreaGeneration {
        self.generation
    }

    /// Return the selected monitor identity.
    pub const fn token(self) -> NativeWorkAreaToken {
        self.token
    }
}
