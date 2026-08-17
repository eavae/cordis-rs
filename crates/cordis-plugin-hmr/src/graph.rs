//! Artifact-level dependency graph analysis.
//!
//! The TS implementation walks Node's module graph at source level; the Rust
//! port uses the declarative `deps` metadata each `.so` plugin exports and
//! classifies changed artifacts into `accepted` (reload) and `declined`
//! (skip) with the same fixed-point rules as `analyzeChanges`.

use std::collections::{HashMap, HashSet};

/// One plugin artifact and its declared dependencies.
#[derive(Clone, Debug, Default)]
pub struct Artifact {
    /// Stable plugin name (the loader registry key).
    pub name: String,
    /// Declared dependencies (host crates/services).
    pub deps: Vec<String>,
}

/// A dependency graph built from plugin metadata.
#[derive(Clone, Debug, Default)]
pub struct DependencyGraph {
    /// Artifact name → declared dependencies.
    artifacts: HashMap<String, Vec<String>>,
    /// Dependency name → artifacts that depend on it.
    dependents: HashMap<String, HashSet<String>>,
}

impl DependencyGraph {
    /// Adds an artifact and its declared dependencies.
    pub fn add(&mut self, artifact: Artifact) {
        let name = artifact.name;
        let deps = artifact.deps;
        self.artifacts.insert(name.clone(), deps.clone());
        for dep in deps {
            self.dependents.entry(dep).or_default().insert(name.clone());
        }
    }
}

/// The classification result (mirrors `analyzeChanges`).
#[derive(Clone, Debug, Default)]
pub struct Classification {
    /// Directly changed artifacts that should reload.
    pub accepted: HashSet<String>,
    /// Artifacts that must not reload (externals or fully declined).
    pub declined: HashSet<String>,
}

/// Classifies changed artifacts using the TS fixed-point rules.
///
/// - `stashed`: directly changed artifacts (seeds for `accepted`).
/// - `externals`: framework artifacts (seeds for `declined`).
pub fn analyze_changes(
    graph: &DependencyGraph,
    stashed: &[String],
    externals: &[String],
) -> Classification {
    let mut accepted: HashSet<String> = stashed.iter().cloned().collect();
    let mut declined: HashSet<String> = externals.iter().cloned().collect();
    let mut pending: Vec<String> = Vec::new();

    // Seed pending with every artifact; the fixed-point loop classifies each
    // one from its dependency set (accepted/declined seeds included).
    pending.extend(graph.artifacts.keys().cloned());
    pending.retain(|artifact| !accepted.contains(artifact) && !declined.contains(artifact));

    loop {
        let mut has_update = false;
        let mut index = 0;
        while index < pending.len() {
            let artifact = pending[index].clone();
            let deps = graph.artifacts.get(&artifact).cloned().unwrap_or_default();
            let mut is_declined = true;
            let mut is_accepted = false;
            for dep in &deps {
                if declined.contains(dep) {
                    continue;
                }
                if accepted.contains(dep) {
                    is_accepted = true;
                    break;
                }
                is_declined = false;
                if !pending.contains(dep) {
                    has_update = true;
                    pending.push(dep.clone());
                }
            }
            if is_accepted || is_declined {
                has_update = true;
                pending.remove(index);
                if is_accepted {
                    accepted.insert(artifact);
                } else {
                    declined.insert(artifact);
                }
            } else {
                index += 1;
            }
        }
        if !has_update {
            break;
        }
    }

    for artifact in pending {
        declined.insert(artifact);
    }
    Classification { accepted, declined }
}
