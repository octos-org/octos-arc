//! Requirement tree helpers (`arc/requirement_order.py` and the tree parts of
//! `arc/main.py`): ATOMIC flattening, dependency-ordered traversal, ancestor
//! lookup, content fingerprints and folder → leaf maps.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use eyre::{Result, bail};
use serde_json::Value;
use sha2::{Digest, Sha256};

/// `requirements.yaml` / `.yml` / `.json` from a directory or file; unwraps a
/// `root:` / `requirement:` envelope (`main.load_requirement_tree`).
pub fn load(path: &Path) -> Result<Value> {
    let file = if path.is_dir() {
        ["requirements.yaml", "requirements.yml", "requirements.json"]
            .into_iter()
            .map(|name| path.join(name))
            .find(|candidate| candidate.is_file())
            .ok_or_else(|| eyre::eyre!("no requirements.yaml in {}", path.display()))?
    } else {
        path.to_path_buf()
    };
    let raw: Value = serde_yml::from_slice(&std::fs::read(&file)?)?;
    let mut tree = raw;
    if tree.get("id").is_none() {
        for wrapper in ["root", "requirement"] {
            if tree.get(wrapper).is_some_and(Value::is_object) {
                tree = tree[wrapper].clone();
                break;
            }
        }
    }
    if !tree.is_object() || tree.get("id").is_none() {
        bail!("invalid requirements tree in {}", file.display());
    }
    Ok(tree)
}

pub fn node_id(node: &Value) -> String {
    match node.get("id") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Null) | None => String::new(),
        Some(other) => other.to_string(),
    }
}

fn node_type(node: &Value) -> String {
    node.get("type")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_ascii_uppercase()
}

fn children(node: &Value) -> Vec<&Value> {
    node.get("children")
        .and_then(Value::as_array)
        .map(|items| items.iter().filter(|c| c.is_object()).collect())
        .unwrap_or_default()
}

fn dependencies(node: &Value) -> Vec<String> {
    node.get("dependencies")
        .and_then(Value::as_array)
        .map(|deps| {
            deps.iter()
                .map(|d| match d {
                    Value::String(s) => s.clone(),
                    other => other.to_string(),
                })
                .collect()
        })
        .unwrap_or_default()
}

fn is_atomic(node: &Value) -> bool {
    let kind = node_type(node);
    kind == "ATOMIC" || (children(node).is_empty() && kind != "FOLDER")
}

/// Leaves in document order. Nodes typed ATOMIC, or untyped leaves, count.
pub fn flatten_atomic(node: &Value) -> Vec<Value> {
    let mut out = Vec::new();
    fn walk(node: &Value, out: &mut Vec<Value>) {
        if is_atomic(node) {
            out.push(node.clone());
        }
        for child in children(node) {
            walk(child, out);
        }
    }
    walk(node, &mut out);
    out
}

/// Every node id (folders included) → ids of its ATOMIC descendants (a leaf
/// maps to itself).
fn descendant_atomic_ids(tree: &Value) -> BTreeMap<String, Vec<String>> {
    let mut mapping = BTreeMap::new();
    fn walk(node: &Value, mapping: &mut BTreeMap<String, Vec<String>>) -> Vec<String> {
        let id = node_id(node);
        let ids = if is_atomic(node) {
            vec![id.clone()]
        } else {
            children(node)
                .into_iter()
                .flat_map(|child| walk(child, mapping))
                .collect()
        };
        if !id.is_empty() {
            mapping.insert(id, ids.clone());
        }
        ids
    }
    walk(tree, &mut mapping);
    mapping
}

/// ATOMIC nodes, dependencies first; among ready nodes keep document order.
/// Folder dependencies expand to the folder's atomic descendants. Unknown ids
/// and self references are ignored. A cycle is broken by taking the earliest
/// remaining node in document order.
pub fn topo_order(tree: &Value) -> Vec<Value> {
    let nodes = flatten_atomic(tree);
    let descendants = descendant_atomic_ids(tree);
    let ids: Vec<String> = nodes.iter().map(node_id).collect();
    let known: BTreeSet<&str> = ids.iter().map(String::as_str).collect();
    let mut deps: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (node, id) in nodes.iter().zip(&ids) {
        let mut expanded: Vec<String> = Vec::new();
        for dep in dependencies(node) {
            let atoms = descendants
                .get(&dep)
                .cloned()
                .unwrap_or_else(|| vec![dep.clone()]);
            for atom in atoms {
                if known.contains(atom.as_str()) && &atom != id && !expanded.contains(&atom) {
                    expanded.push(atom);
                }
            }
        }
        deps.insert(id.clone(), expanded);
    }
    let mut done: BTreeSet<String> = BTreeSet::new();
    let mut remaining: Vec<(String, Value)> = ids.into_iter().zip(nodes).collect();
    let mut ordered = Vec::new();
    while !remaining.is_empty() {
        let index = remaining
            .iter()
            .position(|(id, _)| deps[id].iter().all(|d| done.contains(d)))
            .unwrap_or(0);
        let (id, node) = remaining.remove(index);
        done.insert(id);
        ordered.push(node);
    }
    ordered
}

/// Transitive dependencies of `id`, in the given (topological) order.
pub fn ancestors_of(id: &str, ordered: &[Value]) -> Vec<String> {
    let by_id: BTreeMap<String, &Value> = ordered.iter().map(|n| (node_id(n), n)).collect();
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut stack: Vec<String> = by_id.get(id).map(|n| dependencies(n)).unwrap_or_default();
    while let Some(dep) = stack.pop() {
        if seen.contains(&dep) || dep == id {
            continue;
        }
        let Some(node) = by_id.get(&dep) else {
            continue;
        };
        seen.insert(dep.clone());
        stack.extend(dependencies(node));
    }
    ordered
        .iter()
        .map(node_id)
        .filter(|id| seen.contains(id))
        .collect()
}

/// Stable hash of the requirement content (not of child nodes). The payload is
/// serialised like Python's `json.dumps(sort_keys=True, separators=(",", ":"))`
/// so fingerprints stored by the Python adapter keep matching.
pub fn node_fingerprint(node: &Value) -> String {
    fn text(node: &Value, key: &str) -> String {
        match node.get(key) {
            Some(Value::String(s)) => s.clone(),
            Some(Value::Null) | None => String::new(),
            Some(other) => other.to_string(),
        }
    }
    let payload = serde_json::json!({
        "dependencies": dependencies(node),
        "description": text(node, "description"),
        "id": text(node, "id"),
        "name": text(node, "name"),
        "scenarios": node.get("scenarios").cloned().unwrap_or(Value::Array(vec![])),
    });
    let blob = compact_sorted_json(&payload);
    let digest = Sha256::digest(blob.as_bytes());
    format!("{digest:x}")[..16].to_string()
}

/// `json.dumps(obj, sort_keys=True, ensure_ascii=False, separators=(",", ":"))`.
fn compact_sorted_json(value: &Value) -> String {
    match value {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            let body: Vec<String> = keys
                .into_iter()
                .map(|k| {
                    format!(
                        "{}:{}",
                        serde_json::to_string(k).unwrap(),
                        compact_sorted_json(&map[k])
                    )
                })
                .collect();
            format!("{{{}}}", body.join(","))
        }
        Value::Array(items) => {
            let body: Vec<String> = items.iter().map(compact_sorted_json).collect();
            format!("[{}]", body.join(","))
        }
        Value::Number(n) => {
            // Python prints floats like 1.0; integers stay integers.
            if let Some(i) = n.as_i64() {
                i.to_string()
            } else if let Some(f) = n.as_f64() {
                if f.fract() == 0.0 {
                    format!("{f:.1}")
                } else {
                    f.to_string()
                }
            } else {
                n.to_string()
            }
        }
        Value::Bool(true) => "true".into(),
        Value::Bool(false) => "false".into(),
        Value::Null => "null".into(),
        Value::String(_) => serde_json::to_string(value).unwrap(),
    }
}

/// Non-atomic node id → ids of its ATOMIC descendants (document order).
pub fn folder_descendants(tree: &Value) -> BTreeMap<String, Vec<String>> {
    let mut out = BTreeMap::new();
    fn walk(node: &Value, out: &mut BTreeMap<String, Vec<String>>) -> Vec<String> {
        if is_atomic(node) {
            return vec![node_id(node)];
        }
        let ids: Vec<String> = children(node)
            .into_iter()
            .flat_map(|c| walk(c, out))
            .collect();
        let id = node_id(node);
        if !id.is_empty() {
            out.insert(id, ids.clone());
        }
        ids
    }
    walk(tree, &mut out);
    out
}

/// Prose description of one node for prompts (`main.describe_node`).
pub fn describe_node(node: &Value) -> String {
    let text = |key: &str| -> String {
        match node.get(key) {
            Some(Value::String(s)) => s.clone(),
            Some(Value::Null) | None => String::new(),
            Some(other) => other.to_string(),
        }
    };
    let mut lines = vec![
        format!("ID: {}", node_id(node)),
        format!("Name: {}", text("name")),
    ];
    if !text("description").is_empty() {
        lines.push(format!("Description: {}", text("description")));
    }
    if let Some(scenarios) = node.get("scenarios").and_then(Value::as_array)
        && !scenarios.is_empty()
    {
        lines.push("Scenarios:".into());
        for scenario in scenarios {
            let name = scenario
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("scenario");
            lines.push(format!("  - {name}"));
            for step in scenario
                .get("steps")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                if step.is_object() {
                    let keyword = step.get("keyword").and_then(Value::as_str).unwrap_or("");
                    let content = match step.get("content") {
                        Some(Value::String(s)) => s.clone(),
                        Some(Value::Null) | None => String::new(),
                        Some(other) => other.to_string(),
                    };
                    lines.push(format!("      {keyword} {}", content.trim()));
                }
            }
        }
    }
    let deps = dependencies(node);
    if !deps.is_empty() {
        lines.push(format!("Depends on: {}", deps.join(", ")));
    }
    lines.join("\n")
}

/// Nodes whose fingerprint matches the previous run's requirement table
/// (`main.unchanged_node_ids`; rows are the `.arc/traceability/requirements.json`
/// records the Python adapter writes).
pub fn unchanged_node_ids(nodes: &[Value], previous: &BTreeMap<String, Value>) -> BTreeSet<String> {
    nodes
        .iter()
        .filter(|node| {
            previous
                .get(&node_id(node))
                .is_some_and(|prev| node_fingerprint(prev) == node_fingerprint(node))
        })
        .map(node_id)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn node(id: &str, deps: &[&str]) -> Value {
        json!({"id": id, "type": "ATOMIC", "name": id, "description": "d", "dependencies": deps})
    }

    #[test]
    fn should_order_dependencies_first_and_keep_document_order_among_ready_nodes() {
        let tree = json!({"id": "ROOT", "type": "FOLDER", "children": [
            node("REQ-2", &["REQ-1"]), node("REQ-1", &[]), node("REQ-3", &[])]});
        let ids: Vec<String> = topo_order(&tree).iter().map(node_id).collect();
        assert_eq!(ids, ["REQ-1", "REQ-2", "REQ-3"]);
    }

    #[test]
    fn should_expand_folder_dependencies_and_ignore_unknown_or_self() {
        let tree = json!({"id": "ROOT", "type": "FOLDER", "children": [
            node("REQ-3", &["F-1", "REQ-3", "ghost"]),
            {"id": "F-1", "type": "FOLDER", "children": [node("REQ-1", &[]), node("REQ-2", &[])]}]});
        let ids: Vec<String> = topo_order(&tree).iter().map(node_id).collect();
        assert_eq!(ids, ["REQ-1", "REQ-2", "REQ-3"]);
    }

    #[test]
    fn should_break_cycles_by_document_order_and_return_every_node_once() {
        let tree = json!({"id": "ROOT", "type": "FOLDER", "children": [
            node("A", &["B"]), node("B", &["A"]), node("C", &["A"])]});
        let ids: Vec<String> = topo_order(&tree).iter().map(node_id).collect();
        assert_eq!(ids, ["A", "B", "C"]);
    }

    #[test]
    fn should_treat_untyped_leaves_as_atomic_and_skip_empty_folders() {
        let tree = json!({"id": "ROOT", "type": "FOLDER", "children": [
            {"id": "L", "name": "leaf"}, {"id": "F", "type": "FOLDER", "children": []}]});
        let ids: Vec<String> = flatten_atomic(&tree).iter().map(node_id).collect();
        assert_eq!(ids, ["L"]);
    }

    #[test]
    fn should_list_transitive_ancestors_in_topological_order() {
        let ordered = vec![
            node("REQ-1", &[]),
            node("REQ-2", &["REQ-1"]),
            node("REQ-3", &["REQ-2"]),
        ];
        assert_eq!(ancestors_of("REQ-3", &ordered), ["REQ-1", "REQ-2"]);
        assert!(ancestors_of("REQ-1", &ordered).is_empty());
    }

    #[test]
    fn should_fingerprint_content_like_the_python_adapter() {
        // sha256 of {"dependencies":[],"description":"same","id":"REQ-1","name":"REQ-1","scenarios":[{"name":"s","steps":[{"content":"x","keyword":"GIVEN"}]}]}
        let n = json!({"id": "REQ-1", "type": "ATOMIC", "name": "REQ-1", "description": "same",
            "dependencies": [], "scenarios": [{"name": "s", "steps": [{"keyword": "GIVEN", "content": "x"}]}]});
        let row = json!({"req_id": "REQ-1", "id": "REQ-1", "name": "REQ-1", "description": "same", "dependencies": [],
            "scenarios": [{"name": "s", "steps": [{"keyword": "GIVEN", "content": "x"}]}], "children_ids": []});
        assert_eq!(node_fingerprint(&n), node_fingerprint(&row));
        assert_eq!(node_fingerprint(&n).len(), 16);
        assert_ne!(node_fingerprint(&n), node_fingerprint(&node("REQ-1", &[])));
        let previous: BTreeMap<String, Value> = [("REQ-1".to_string(), row)].into_iter().collect();
        assert_eq!(
            unchanged_node_ids(&[n, node("REQ-2", &[])], &previous),
            BTreeSet::from(["REQ-1".to_string()])
        );
    }

    #[test]
    fn should_map_folders_to_atomic_leaves() {
        let tree = json!({"id": "ROOT", "type": "FOLDER", "children": [
            {"id": "F-1", "type": "FOLDER", "children": [node("REQ-1", &[]), node("REQ-2", &[])]},
            node("REQ-3", &[])]});
        let folders = folder_descendants(&tree);
        assert_eq!(folders["F-1"], ["REQ-1", "REQ-2"]);
        assert_eq!(folders["ROOT"], ["REQ-1", "REQ-2", "REQ-3"]);
        assert!(!folders.contains_key("REQ-1"));
    }

    #[test]
    fn should_describe_scenarios_and_dependencies() {
        let n = json!({"id": "REQ-2", "type": "ATOMIC", "name": "REQ-2", "description": "desc", "dependencies": ["REQ-1"],
            "scenarios": [{"name": "s", "steps": [{"keyword": "GIVEN", "content": " x "}]}]});
        let text = describe_node(&n);
        assert!(text.contains("ID: REQ-2"));
        assert!(text.contains("      GIVEN x"));
        assert!(text.contains("Depends on: REQ-1"));
    }

    #[test]
    fn should_load_yaml_with_wrapper_and_reject_missing_id() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("requirements.yaml"),
            "root:\n  id: ROOT\n  type: FOLDER\n  children: []\n",
        )
        .unwrap();
        assert_eq!(node_id(&load(dir.path()).unwrap()), "ROOT");
        std::fs::write(dir.path().join("requirements.yaml"), "name: nope\n").unwrap();
        assert!(load(dir.path()).is_err());
    }
}
