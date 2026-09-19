use super::{MissingSpawnerNode, NestedSpawner, SpawnGraph};

fn spawner(child_id: &str, session_id: &str) -> NestedSpawner {
    NestedSpawner {
        child_id: child_id.to_owned(),
        session_id: session_id.to_owned(),
        surface_completion: true,
    }
}

fn nested_graph() -> SpawnGraph {
    let mut graph = SpawnGraph::default();
    graph.insert_root_child("c", "r");
    graph
        .insert_nested("g", "r", spawner("c", "c"))
        .expect("spawner node inserted");
    graph
}

#[test]
fn direct_child_reachable_from_root_only() {
    let mut graph = SpawnGraph::default();
    graph.insert_root_child("c", "r");
    assert!(graph.is_reachable_from("c", "r"));
    assert!(!graph.is_reachable_from("c", "c"));
    assert!(!graph.is_reachable_from("c", "f"));
    assert_eq!(graph.direct_spawner("c"), None);
    assert_eq!(graph.advertise_target("c"), None);
}

#[test]
fn nested_child_reachable_from_root_and_chain() {
    let graph = nested_graph();
    assert!(graph.is_reachable_from("g", "r"));
    assert!(graph.is_reachable_from("g", "c"));
    assert!(!graph.is_reachable_from("g", "f"));
    assert_eq!(graph.direct_spawner("g"), Some("c"));
    assert_eq!(graph.advertise_target("g"), Some("c"));
}

#[test]
fn nested_chain_is_prefix_consistent() {
    let mut graph = nested_graph();
    // Chain entries are the spawner's session, not its child id.
    graph
        .insert_nested("gg", "r", spawner("g", "g-session"))
        .expect("spawner node inserted");
    assert!(graph.is_reachable_from("gg", "r"));
    assert!(graph.is_reachable_from("gg", "c"));
    assert!(graph.is_reachable_from("gg", "g-session"));
    assert!(!graph.is_reachable_from("gg", "g"));
    assert_eq!(graph.direct_spawner("gg"), Some("g-session"));
}

#[test]
fn drop_advertise_keeps_lineage() {
    let mut graph = nested_graph();
    graph.drop_advertise("g");
    assert_eq!(graph.advertise_target("g"), None);
    assert_eq!(graph.direct_spawner("g"), Some("c"));
    assert!(graph.is_reachable_from("g", "c"));
    assert!(graph.is_reachable_from("g", "r"));
}

#[test]
fn missing_node_is_unreachable() {
    let graph = SpawnGraph::default();
    assert!(!graph.is_reachable_from("nope", "r"));
    assert_eq!(graph.direct_spawner("nope"), None);
    assert_eq!(graph.advertise_target("nope"), None);
}

#[test]
fn remove_evicts_node() {
    let mut graph = nested_graph();
    graph.remove("g");
    assert!(!graph.is_reachable_from("g", "r"));
    assert!(!graph.is_reachable_from("g", "c"));
    assert_eq!(graph.direct_spawner("g"), None);
}

#[test]
fn insert_nested_without_spawner_node_is_rejected() {
    let mut graph = SpawnGraph::default();
    assert!(matches!(
        graph.insert_nested("g", "r", spawner("absent", "absent")),
        Err(MissingSpawnerNode)
    ));
    assert!(!graph.is_reachable_from("g", "r"));
    assert!(!graph.is_reachable_from("g", "absent"));
    assert_eq!(graph.direct_spawner("g"), None);
}

#[test]
fn surface_target_reflects_ingress_flag() {
    let mut graph = nested_graph();
    graph
        .insert_nested(
            "quiet",
            "r",
            NestedSpawner {
                surface_completion: false,
                ..spawner("c", "c")
            },
        )
        .expect("spawner node inserted");
    graph.insert_root_child("direct", "r");
    assert_eq!(graph.surface_target("g"), Some("c"));
    assert_eq!(graph.surface_target("quiet"), None);
    assert_eq!(graph.surface_target("direct"), None);
    assert_eq!(graph.surface_target("missing"), None);
}
