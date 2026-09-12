use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use eyre::{Result, ensure, eyre};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Specification {
    pub tree: Value,
    pub nodes: BTreeMap<String, Value>,
    pub sha256: String,
}

#[derive(Debug, Serialize, Deserialize, PartialEq)]
pub struct Delta {
    pub added: Vec<String>,
    pub changed: Vec<String>,
    pub unchanged: Vec<String>,
    pub removed: Vec<String>,
    pub affected: Vec<String>,
}

pub fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

impl Specification {
    pub fn read(path: &Path) -> Result<Self> {
        let file = if path.is_dir() {
            ["requirements.yaml", "requirements.yml", "requirements.json"]
                .into_iter()
                .map(|name| path.join(name))
                .find(|candidate| candidate.is_file())
                .ok_or_else(|| {
                    eyre!("No requirements.yaml, requirements.yml or requirements.json")
                })?
        } else {
            path.to_owned()
        };
        ensure!(
            std::fs::metadata(&file)?.len() <= 131_072,
            "Requirements exceed 128 KiB"
        );
        let value: Value = serde_yml::from_slice(&std::fs::read(file)?)?;
        Self::from_tree(value)
    }

    pub fn from_tree(value: Value) -> Result<Self> {
        let tree = value
            .get("root")
            .or_else(|| value.get("requirement"))
            .unwrap_or(&value)
            .clone();
        let mut nodes = BTreeMap::new();
        let mut seen = BTreeSet::new();
        collect(&tree, &mut nodes, &mut seen, 0)?;
        ensure!(!nodes.is_empty(), "No ATOMIC requirement nodes");
        for node in nodes.values() {
            for dependency in dependencies(node)? {
                ensure!(
                    nodes.contains_key(dependency),
                    "Unknown dependency: {dependency}"
                );
            }
        }
        let mut resolved = BTreeSet::new();
        while resolved.len() < nodes.len() {
            let previous = resolved.len();
            for (id, node) in &nodes {
                if dependencies(node)?
                    .iter()
                    .all(|dependency| resolved.contains(*dependency))
                {
                    resolved.insert(id.as_str());
                }
            }
            ensure!(resolved.len() > previous, "Cyclic requirement dependencies");
        }
        let sha256 = digest(&serde_json::to_vec(&tree)?);
        Ok(Self {
            tree,
            nodes,
            sha256,
        })
    }

    pub fn delta(&self, previous: Option<&Self>) -> Delta {
        let empty = BTreeMap::new();
        let old = previous.map(|spec| &spec.nodes).unwrap_or(&empty);
        let mut delta = Delta {
            added: vec![],
            changed: vec![],
            unchanged: vec![],
            removed: vec![],
            affected: vec![],
        };
        for (id, node) in &self.nodes {
            match old.get(id) {
                None => delta.added.push(id.clone()),
                Some(prior) if prior == node => delta.unchanged.push(id.clone()),
                Some(_) => delta.changed.push(id.clone()),
            }
        }
        delta.removed = old
            .keys()
            .filter(|id| !self.nodes.contains_key(*id))
            .cloned()
            .collect();
        let mut affected: BTreeSet<String> =
            delta.added.iter().chain(&delta.changed).cloned().collect();
        loop {
            let count = affected.len();
            for (id, node) in &self.nodes {
                if dependencies(node)
                    .unwrap_or_default()
                    .iter()
                    .any(|dependency| affected.contains(*dependency))
                {
                    affected.insert(id.clone());
                }
            }
            if affected.len() == count {
                break;
            }
        }
        delta.affected = affected.into_iter().collect();
        delta
    }
}

fn dependencies(node: &Value) -> Result<Vec<&str>> {
    match node.get("dependencies") {
        None | Some(Value::Null) => Ok(vec![]),
        Some(Value::Array(values)) => values
            .iter()
            .map(|value| {
                value
                    .as_str()
                    .ok_or_else(|| eyre!("Dependency must be a string"))
            })
            .collect(),
        _ => Err(eyre!("Dependencies must be an array")),
    }
}

fn collect(
    node: &Value,
    nodes: &mut BTreeMap<String, Value>,
    seen: &mut BTreeSet<String>,
    depth: usize,
) -> Result<()> {
    ensure!(depth <= 64, "Requirement tree is too deep");
    let id = node
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| !id.trim().is_empty())
        .ok_or_else(|| eyre!("Requirement node needs an id"))?;
    ensure!(seen.insert(id.to_owned()), "Duplicate requirement id: {id}");
    let kind = node
        .get("type")
        .and_then(Value::as_str)
        .ok_or_else(|| eyre!("Requirement node needs a type"))?;
    ensure!(
        matches!(kind, "ATOMIC" | "FOLDER"),
        "Unsupported requirement type: {kind}"
    );
    if kind == "ATOMIC" {
        nodes.insert(id.to_owned(), node.clone());
    }
    if let Some(children) = node.get("children") {
        let children = children
            .as_array()
            .ok_or_else(|| eyre!("Children must be an array"))?;
        ensure!(
            kind != "ATOMIC" || children.is_empty(),
            "ATOMIC node has children"
        );
        for child in children {
            collect(child, nodes, seen, depth + 1)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn node(id: &str, description: &str, dependencies: Vec<&str>) -> Value {
        json!({"id":id,"type":"ATOMIC","description":description,"dependencies":dependencies})
    }

    fn spec(children: Vec<Value>) -> Result<Specification> {
        Specification::from_tree(json!({"id":"ROOT","type":"FOLDER","children":children}))
    }

    #[test]
    fn addition_preserves_old_requirements() {
        let old = spec(vec![node("REQ-1", "controls", vec![])]).unwrap();
        let new = spec(vec![
            node("REQ-1", "controls", vec![]),
            node("REQ-2", "reset", vec!["REQ-1"]),
        ])
        .unwrap();
        assert_eq!(
            new.delta(Some(&old)),
            Delta {
                added: vec!["REQ-2".into()],
                changed: vec![],
                unchanged: vec!["REQ-1".into()],
                removed: vec![],
                affected: vec!["REQ-2".into()]
            }
        );
    }

    #[test]
    fn changes_propagate_to_dependents() {
        let old = spec(vec![
            node("first", "old", vec![]),
            node("second", "stable", vec!["first"]),
        ])
        .unwrap();
        let new = spec(vec![
            node("first", "new", vec![]),
            node("second", "stable", vec!["first"]),
        ])
        .unwrap();
        assert_eq!(new.delta(Some(&old)).affected, vec!["first", "second"]);
    }

    #[test]
    fn rejects_invalid_dependency_graphs() {
        assert!(spec(vec![node("same", "", vec![]), node("same", "", vec![])]).is_err());
        assert!(spec(vec![node("one", "", vec!["missing"])]).is_err());
        assert!(
            spec(vec![
                node("one", "", vec!["two"]),
                node("two", "", vec!["one"])
            ])
            .is_err()
        );
    }

    #[test]
    fn removal_is_not_an_implementation_task() {
        let old = spec(vec![node("one", "", vec![]), node("two", "", vec![])]).unwrap();
        let new = spec(vec![node("one", "", vec![])]).unwrap();
        assert_eq!(new.delta(Some(&old)).removed, vec!["two"]);
        assert!(new.delta(Some(&old)).affected.is_empty());
    }
}
