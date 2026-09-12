use std::path::Path;

use octos_arc::spec::Specification;

#[test]
fn official_counter_evolution_retains_controls_and_adds_reset() {
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let counter = Specification::read(&fixtures.join("smoke--counter.yaml")).unwrap();
    let evolution = Specification::read(&fixtures.join("smoke-evolution--counter.yaml")).unwrap();
    let delta = evolution.delta(Some(&counter));
    assert_eq!(delta.added, ["REQ-2"]);
    assert_eq!(delta.unchanged, ["REQ-1"]);
    assert_eq!(delta.affected, ["REQ-2"]);
    assert!(delta.changed.is_empty());
    assert!(delta.removed.is_empty());
    assert_eq!(counter.nodes["REQ-1"], evolution.nodes["REQ-1"]);
}
