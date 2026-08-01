//! Provider-owned stable monitor identities and event-time route selection.

use std::collections::{BTreeMap, BTreeSet};

use winit::window::Window;

use super::{
    native_work_area_probe::NativeMonitorWorkArea,
    platform_provider::{
        NativeAuthority, NativePhysicalPoint, NativePhysicalRect, NativePlatformError,
        NativeUnavailableReason, NativeWorkArea, NativeWorkAreaGeneration,
        NativeWorkAreaRosterObservation, NativeWorkAreaRoute, NativeWorkAreaToken,
    },
};

#[derive(Clone, Copy, PartialEq)]
struct RoutedWorkArea {
    token: NativeWorkAreaToken,
    monitor_bounds: NativePhysicalRect,
}

pub(super) struct NativeWorkAreaAuthority {
    tokens: BTreeMap<String, NativeWorkAreaToken>,
    current: NativeAuthority<Vec<NativeWorkArea>>,
    routes: Vec<RoutedWorkArea>,
    generation: u64,
    next_token: u64,
}

impl Default for NativeWorkAreaAuthority {
    fn default() -> Self {
        Self {
            tokens: BTreeMap::new(),
            current: NativeAuthority::unknown(NativeUnavailableReason::NotObserved),
            routes: Vec::new(),
            generation: 0,
            next_token: 0,
        }
    }
}

impl NativeWorkAreaAuthority {
    pub(super) fn observe_window(&mut self, window: &Window) -> Result<(), NativePlatformError> {
        self.observe(&super::native_work_area_probe::probe(window))
    }

    pub(super) fn observe_unavailable(
        &mut self,
        reason: NativeUnavailableReason,
    ) -> Result<(), NativePlatformError> {
        self.observe(&NativeAuthority::unknown(reason))
    }

    fn observe(
        &mut self,
        observation: &NativeAuthority<Vec<NativeMonitorWorkArea>>,
    ) -> Result<(), NativePlatformError> {
        let (next, routes) = match observation.value() {
            Some(candidates) => self.canonicalize(candidates)?,
            None => (
                NativeAuthority::unknown(
                    observation
                        .unavailable_reason()
                        .unwrap_or(NativeUnavailableReason::NotObserved),
                ),
                Vec::new(),
            ),
        };
        if next != self.current || routes != self.routes {
            self.generation = self
                .generation
                .checked_add(1)
                .ok_or(NativePlatformError::CounterExhausted)?;
            self.current = next;
            self.routes = routes;
        }
        Ok(())
    }

    fn canonicalize(
        &mut self,
        candidates: &[NativeMonitorWorkArea],
    ) -> Result<(NativeAuthority<Vec<NativeWorkArea>>, Vec<RoutedWorkArea>), NativePlatformError>
    {
        let mut candidates = candidates.to_vec();
        candidates.sort_by(|left, right| left.identity.cmp(&right.identity));
        let mut identities = BTreeSet::new();
        let mut roster = Vec::with_capacity(candidates.len());
        let mut routes = Vec::with_capacity(candidates.len());
        for candidate in candidates {
            if !identities.insert(candidate.identity.clone()) {
                return Ok((
                    NativeAuthority::unknown(NativeUnavailableReason::StaleSource),
                    Vec::new(),
                ));
            }
            let token = if let Some(token) = self.tokens.get(&candidate.identity).copied() {
                token
            } else {
                self.next_token = self
                    .next_token
                    .checked_add(1)
                    .ok_or(NativePlatformError::CounterExhausted)?;
                let token = NativeWorkAreaToken::new(self.next_token);
                self.tokens.insert(candidate.identity, token);
                token
            };
            roster.push(NativeWorkArea::new(
                token,
                candidate.work_area_bounds,
                candidate.scale_factor,
            ));
            routes.push(RoutedWorkArea {
                token,
                monitor_bounds: candidate.monitor_bounds,
            });
        }
        roster.sort_by_key(|work_area| work_area.token());
        routes.sort_by_key(|route| route.token);
        if roster.is_empty() {
            Ok((
                NativeAuthority::unknown(NativeUnavailableReason::NotObserved),
                Vec::new(),
            ))
        } else {
            Ok((NativeAuthority::known(roster), routes))
        }
    }

    pub(super) fn observation(&self) -> NativeWorkAreaRosterObservation {
        NativeWorkAreaRosterObservation::new(
            NativeWorkAreaGeneration::new(self.generation),
            self.current.clone(),
        )
    }

    pub(super) fn route(
        &self,
        position: &NativeAuthority<NativePhysicalPoint>,
    ) -> NativeAuthority<NativeWorkAreaRoute> {
        let Some(position) = position.value().copied() else {
            return NativeAuthority::unknown(
                position
                    .unavailable_reason()
                    .unwrap_or(NativeUnavailableReason::NotObserved),
            );
        };
        if let Some(reason) = self.current.unavailable_reason() {
            return NativeAuthority::unknown(reason);
        }
        let mut matches = self
            .routes
            .iter()
            .filter(|route| contains(route.monitor_bounds, position));
        let Some(route) = matches.next() else {
            return NativeAuthority::unknown(NativeUnavailableReason::NotObserved);
        };
        if matches.next().is_some() {
            return NativeAuthority::unknown(NativeUnavailableReason::StaleSource);
        }
        NativeAuthority::known(NativeWorkAreaRoute::new(
            NativeWorkAreaGeneration::new(self.generation),
            route.token,
        ))
    }
}

fn contains(rect: NativePhysicalRect, point: NativePhysicalPoint) -> bool {
    point.x() >= rect.min().x()
        && point.y() >= rect.min().y()
        && point.x() < rect.max().x()
        && point.y() < rect.max().y()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: i32, y: i32, width: i32, height: i32) -> NativePhysicalRect {
        NativePhysicalRect::new(
            NativePhysicalPoint::new(x, y),
            NativePhysicalPoint::new(x + width, y + height),
        )
    }

    fn monitor(identity: &str, bounds: NativePhysicalRect) -> NativeMonitorWorkArea {
        NativeMonitorWorkArea {
            identity: identity.to_owned(),
            monitor_bounds: bounds,
            work_area_bounds: bounds,
            scale_factor: 1.0,
        }
    }

    #[test]
    fn monitor_tokens_survive_reordering_and_geometry_changes() {
        let mut authority = NativeWorkAreaAuthority::default();
        authority
            .observe(&NativeAuthority::known(vec![
                monitor("left", rect(-100, 0, 100, 100)),
                monitor("right", rect(0, 0, 100, 100)),
            ]))
            .unwrap();
        let first = authority.observation();
        let first_roster = first.roster().value().unwrap();
        let left = first_roster[0].token();
        let right = first_roster[1].token();

        authority
            .observe(&NativeAuthority::known(vec![
                monitor("right", rect(0, 0, 100, 100)),
                monitor("left", rect(-100, 0, 100, 100)),
            ]))
            .unwrap();
        assert_eq!(authority.observation().generation(), first.generation());

        authority
            .observe(&NativeAuthority::known(vec![
                monitor("left", rect(-120, 0, 120, 100)),
                monitor("right", rect(0, 0, 120, 100)),
            ]))
            .unwrap();
        let changed = authority.observation();
        assert_ne!(changed.generation(), first.generation());
        let changed_roster = changed.roster().value().unwrap();
        assert_eq!(changed_roster[0].token(), left);
        assert_eq!(changed_roster[1].token(), right);
    }

    #[test]
    fn event_time_route_requires_one_unambiguous_monitor() {
        let mut authority = NativeWorkAreaAuthority::default();
        authority
            .observe(&NativeAuthority::known(vec![
                monitor("left", rect(-100, 0, 100, 100)),
                monitor("right", rect(0, 0, 100, 100)),
            ]))
            .unwrap();
        let route = authority
            .route(&NativeAuthority::known(NativePhysicalPoint::new(25, 50)))
            .value()
            .copied()
            .expect("one monitor owns the point");
        assert_eq!(route.generation(), authority.observation().generation());

        authority
            .observe(&NativeAuthority::known(vec![
                monitor("mirror-a", rect(0, 0, 100, 100)),
                monitor("mirror-b", rect(0, 0, 100, 100)),
            ]))
            .unwrap();
        assert_eq!(
            authority
                .route(&NativeAuthority::known(NativePhysicalPoint::new(25, 50)))
                .unavailable_reason(),
            Some(NativeUnavailableReason::StaleSource)
        );
    }

    #[test]
    fn route_generation_advances_when_only_monitor_selection_bounds_change() {
        let mut authority = NativeWorkAreaAuthority::default();
        let work_area = rect(0, 0, 90, 100);
        authority
            .observe(&NativeAuthority::known(vec![NativeMonitorWorkArea {
                identity: "primary".to_owned(),
                monitor_bounds: rect(0, 0, 100, 100),
                work_area_bounds: work_area,
                scale_factor: 1.0,
            }]))
            .unwrap();
        let first = authority.observation().generation();
        let stale_route = authority
            .route(&NativeAuthority::known(NativePhysicalPoint::new(95, 50)))
            .value()
            .copied()
            .expect("the original monitor contains the point");

        authority
            .observe(&NativeAuthority::known(vec![NativeMonitorWorkArea {
                identity: "primary".to_owned(),
                monitor_bounds: rect(0, 0, 90, 100),
                work_area_bounds: work_area,
                scale_factor: 1.0,
            }]))
            .unwrap();

        assert_ne!(authority.observation().generation(), first);
        assert_ne!(
            stale_route.generation(),
            authority.observation().generation()
        );
        assert_eq!(
            authority
                .route(&NativeAuthority::known(NativePhysicalPoint::new(95, 50)))
                .unavailable_reason(),
            Some(NativeUnavailableReason::NotObserved)
        );
    }
}
