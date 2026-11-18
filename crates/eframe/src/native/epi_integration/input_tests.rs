//! Deferred input must retain its originating viewport through logic-only updates.

use super::close_tests::integration;
use super::*;

#[derive(Default)]
struct InputApp {
    seen: Vec<(ViewportId, Vec<egui::Event>)>,
    child: Option<ViewportId>,
}

impl crate::App for InputApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut crate::Frame) {
        if let Some(child) = self.child {
            ui.ctx()
                .show_viewport_deferred(child, egui::ViewportBuilder::default(), |_, _| {});
        }
    }

    fn raw_input_hook(&mut self, _ctx: &egui::Context, input: &mut egui::RawInput) {
        self.seen.push((input.viewport_id, input.events.clone()));
    }
}

fn input(viewport: ViewportId, text: Option<&str>) -> egui::RawInput {
    let mut input = egui::RawInput {
        viewport_id: viewport,
        ..Default::default()
    };
    input.viewports.entry(viewport).or_default();
    if let Some(text) = text {
        input.events.push(egui::Event::Text(text.into()));
    }
    input
}

fn full_update(
    integration: &mut EpiIntegration,
    app: &mut InputApp,
    viewport: ViewportId,
    text: Option<&str>,
    native_host: &super::super::host_seam::NativeHostState,
) {
    let callback: Arc<egui::DeferredViewportUiCallback> = Arc::new(|ui| {
        ui.label("child editor");
    });
    let output = integration.update(
        app,
        (viewport != ViewportId::ROOT).then_some(callback.as_ref()),
        input(viewport, text),
        native_host,
    );
    output.drop_without_applying_deltas();
}

#[test]
fn logic_only_input_is_delivered_once_to_its_original_viewport() {
    let child = ViewportId::from_hash_of("child-editor");
    for (source, other) in [(ViewportId::ROOT, child), (child, ViewportId::ROOT)] {
        let mut integration = integration();
        let mut app = InputApp {
            child: Some(child),
            ..Default::default()
        };
        let native_host = super::super::host_seam::NativeHostState::new(None);
        full_update(
            &mut integration,
            &mut app,
            ViewportId::ROOT,
            None,
            &native_host,
        );
        app.seen.clear();
        integration.update_logic_only(&mut app, input(source, Some("owned")), &native_host);
        assert!(integration.has_pending_ui_input(source));
        assert!(!integration.has_pending_ui_input(other));
        full_update(
            &mut integration,
            &mut app,
            other,
            Some("other"),
            &native_host,
        );
        assert_eq!(
            app.seen[1],
            (other, vec![egui::Event::Text("other".into())])
        );

        full_update(
            &mut integration,
            &mut app,
            source,
            Some("later"),
            &native_host,
        );
        assert_eq!(
            app.seen[2].1,
            [
                egui::Event::Text("owned".into()),
                egui::Event::Text("later".into())
            ]
        );
        assert!(!integration.has_pending_ui_input(source));
        full_update(&mut integration, &mut app, source, None, &native_host);
        assert!(app.seen[3].1.is_empty());
    }
}

#[test]
fn a_hidden_childs_extracted_input_survives_until_its_ui_pass() {
    let child = ViewportId::from_hash_of("hidden-child-editor");
    let mut integration = integration();
    let mut app = InputApp {
        child: Some(child),
        ..Default::default()
    };
    let native_host = super::super::host_seam::NativeHostState::new(None);
    full_update(
        &mut integration,
        &mut app,
        ViewportId::ROOT,
        None,
        &native_host,
    );
    app.seen.clear();
    // Both native renderers use this same handoff when a child skips its UI pass.
    integration.retain_raw_input(input(child, Some("first")));
    integration.retain_raw_input(input(child, Some("second")));
    full_update(
        &mut integration,
        &mut app,
        ViewportId::ROOT,
        None,
        &native_host,
    );
    assert!(app.seen[0].1.is_empty());
    assert!(integration.has_pending_ui_input(child));
    full_update(&mut integration, &mut app, child, None, &native_host);
    assert_eq!(
        app.seen[1].1,
        [
            egui::Event::Text("first".into()),
            egui::Event::Text("second".into())
        ]
    );
    assert!(!integration.has_pending_ui_input(child));
}
