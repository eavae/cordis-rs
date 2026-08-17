//! Dependency graph classification (accepted/declined).

use std::collections::HashSet;

use cordis_plugin_hmr::graph::{Artifact, DependencyGraph, analyze_changes};

fn set(items: &[&str]) -> HashSet<String> {
    items.iter().map(|s| s.to_string()).collect()
}

/// A shared dependency makes all dependent plugins accepted; the dependency
/// itself and unrelated artifacts behave per the TS rules.
#[test]
fn shared_dependency_classification() {
    let mut graph = DependencyGraph::default();
    graph.add(Artifact {
        name: "dep".to_string(),
        deps: vec![],
    });
    graph.add(Artifact {
        name: "plugin-a".to_string(),
        deps: vec!["dep".to_string()],
    });
    graph.add(Artifact {
        name: "plugin-b".to_string(),
        deps: vec!["dep".to_string()],
    });
    graph.add(Artifact {
        name: "unrelated".to_string(),
        deps: vec![],
    });

    // Changing `dep` accepts both dependents.
    let result = analyze_changes(&graph, &["dep".to_string()], &[]);
    assert!(result.accepted.contains("dep"));
    assert!(result.accepted.contains("plugin-a"));
    assert!(result.accepted.contains("plugin-b"));
    assert!(!result.accepted.contains("unrelated"));
    // An artifact with no dependency path to the change is declined (TS
    // rule: all its dependents are declined → itself declined).
    assert!(result.declined.contains("unrelated"));

    // Changing an unrelated artifact accepts only itself.
    let result = analyze_changes(&graph, &["unrelated".to_string()], &[]);
    assert_eq!(result.accepted, set(&["unrelated"]));
}

/// A declined dependency cascades: dependents of a declined artifact are
/// declined, not accepted.
#[test]
fn declined_dependency_cascades() {
    let mut graph = DependencyGraph::default();
    graph.add(Artifact {
        name: "framework".to_string(),
        deps: vec![],
    });
    graph.add(Artifact {
        name: "plugin-x".to_string(),
        deps: vec!["framework".to_string()],
    });
    graph.add(Artifact {
        name: "plugin-y".to_string(),
        deps: vec!["plugin-x".to_string()],
    });

    // The framework change is an external → full restart; everything that
    // depends on it is declined (not reloaded in place).
    let result = analyze_changes(&graph, &[], &["framework".to_string()]);
    assert!(result.declined.contains("framework"));
    assert!(result.declined.contains("plugin-x"));
    assert!(result.declined.contains("plugin-y"));
    assert!(result.accepted.is_empty());
}

/// A dependency accepted through one branch keeps its dependent accepted
/// even when another branch is unresolved.
#[test]
fn accepted_branch_wins() {
    let mut graph = DependencyGraph::default();
    graph.add(Artifact {
        name: "shared".to_string(),
        deps: vec![],
    });
    graph.add(Artifact {
        name: "plugin".to_string(),
        deps: vec!["shared".to_string(), "missing".to_string()],
    });

    let result = analyze_changes(&graph, &["shared".to_string()], &[]);
    assert!(result.accepted.contains("shared"));
    assert!(result.accepted.contains("plugin"));
}
