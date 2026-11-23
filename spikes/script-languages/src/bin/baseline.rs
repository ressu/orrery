//! The same program without a language: the size every candidate is measured against.
#[path = "../common.rs"]
mod common;

fn main() {
    let scenarios = [common::Scenario { name: "estop", on_press: Some("printer.emergency_stop"), on_release: None }];
    common::drive(&scenarios, || {
        Box::new(|script, _e| {
            common::call_method(script, serde_json::json!({}));
            Ok(())
        })
    });
}
