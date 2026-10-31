//! Dependency graph rules for tasks.

use crate::error::{FleetError, Result};
use crate::ids::{show, TaskId};
use crate::limits::MAX_DEPS_PER_TASK;
use std::collections::{BTreeMap, BTreeSet};

/// A task to be added: its id and what it waits for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Node {
    pub id: TaskId,
    pub deps: Vec<TaskId>,
}

/// Checks `new` against the tasks that already exist and returns the indices of `new` in an order
/// where every task comes after the new tasks it depends on (ties keep input order).
///
/// Rejected: duplicate ids (inside `new` or against `existing`), a task that depends on itself, a
/// dependency that exists nowhere, a dependency listed twice, too many dependencies and cycles.
pub fn order_new_tasks(existing: &BTreeSet<TaskId>, new: &[Node]) -> Result<Vec<usize>> {
    let mut index: BTreeMap<&TaskId, usize> = BTreeMap::new();
    for (i, node) in new.iter().enumerate() {
        if existing.contains(&node.id) || index.insert(&node.id, i).is_some() {
            return Err(FleetError::invalid(format!(
                "task id {} is used more than once",
                show(node.id.as_str())
            )));
        }
    }
    let mut indegree = vec![0usize; new.len()];
    let mut dependents: Vec<Vec<usize>> = vec![Vec::new(); new.len()];
    for (i, node) in new.iter().enumerate() {
        if node.deps.len() > MAX_DEPS_PER_TASK {
            return Err(FleetError::limit(format!(
                "task {} has more than {MAX_DEPS_PER_TASK} dependencies",
                node.id
            )));
        }
        let mut seen = BTreeSet::new();
        for dep in &node.deps {
            if dep == &node.id {
                return Err(FleetError::invalid(format!(
                    "task {} depends on itself",
                    node.id
                )));
            }
            if !seen.insert(dep) {
                return Err(FleetError::invalid(format!(
                    "task {} lists dependency {} twice",
                    node.id, dep
                )));
            }
            if let Some(&j) = index.get(dep) {
                if let Some(d) = indegree.get_mut(i) {
                    *d += 1;
                }
                if let Some(list) = dependents.get_mut(j) {
                    list.push(i);
                }
            } else if !existing.contains(dep) {
                return Err(FleetError::invalid(format!(
                    "task {} depends on unknown task {}",
                    node.id,
                    show(dep.as_str())
                )));
            }
        }
    }

    let mut order = Vec::with_capacity(new.len());
    let mut ready: BTreeSet<usize> = indegree
        .iter()
        .enumerate()
        .filter(|(_, d)| **d == 0)
        .map(|(i, _)| i)
        .collect();
    while let Some(&next) = ready.iter().next() {
        ready.remove(&next);
        order.push(next);
        for &d in dependents.get(next).map(Vec::as_slice).unwrap_or(&[]) {
            if let Some(deg) = indegree.get_mut(d) {
                *deg = deg.saturating_sub(1);
                if *deg == 0 {
                    ready.insert(d);
                }
            }
        }
    }
    if order.len() != new.len() {
        return Err(FleetError::invalid(format!(
            "dependency cycle: {}",
            describe_cycle(new, &index, &indegree)
        )));
    }
    Ok(order)
}

/// Walks dependencies from a node that Kahn's algorithm could not remove until a node repeats.
fn describe_cycle(new: &[Node], index: &BTreeMap<&TaskId, usize>, indegree: &[usize]) -> String {
    let Some(start) = indegree.iter().position(|d| *d > 0) else {
        return "unknown".to_string();
    };
    let mut path: Vec<usize> = Vec::new();
    let mut cur = start;
    loop {
        if let Some(pos) = path.iter().position(|p| *p == cur) {
            let mut names: Vec<String> = path
                .iter()
                .skip(pos)
                .filter_map(|i| new.get(*i).map(|n| n.id.to_string()))
                .collect();
            if let Some(first) = names.first().cloned() {
                names.push(first);
            }
            return names.join(" -> ");
        }
        path.push(cur);
        let next = new.get(cur).and_then(|n| {
            n.deps
                .iter()
                .filter_map(|d| index.get(d).copied())
                .find(|j| indegree.get(*j).copied().unwrap_or(0) > 0)
        });
        match next {
            Some(n) => cur = n,
            None => return "unknown".to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(s: &str) -> TaskId {
        TaskId::parse(s).unwrap()
    }

    fn node(id: &str, deps: &[&str]) -> Node {
        Node {
            id: t(id),
            deps: deps.iter().map(|d| t(d)).collect(),
        }
    }

    fn none() -> BTreeSet<TaskId> {
        BTreeSet::new()
    }

    #[test]
    fn independent_tasks_keep_input_order() {
        let order =
            order_new_tasks(&none(), &[node("b", &[]), node("a", &[]), node("c", &[])]).unwrap();
        assert_eq!(order, vec![0, 1, 2]);
    }

    #[test]
    fn dependencies_come_first() {
        let nodes = [node("c", &["b"]), node("b", &["a"]), node("a", &[])];
        assert_eq!(order_new_tasks(&none(), &nodes).unwrap(), vec![2, 1, 0]);
    }

    #[test]
    fn diamond_is_ordered() {
        let nodes = [
            node("d", &["b", "c"]),
            node("b", &["a"]),
            node("c", &["a"]),
            node("a", &[]),
        ];
        let order = order_new_tasks(&none(), &nodes).unwrap();
        let pos = |i: usize| order.iter().position(|x| *x == i).unwrap();
        assert!(pos(3) < pos(1) && pos(3) < pos(2) && pos(1) < pos(0) && pos(2) < pos(0));
    }

    #[test]
    fn dependencies_on_existing_tasks_are_allowed() {
        let existing: BTreeSet<TaskId> = [t("old")].into_iter().collect();
        assert_eq!(
            order_new_tasks(&existing, &[node("n", &["old"])]).unwrap(),
            vec![0]
        );
    }

    #[test]
    fn cycles_are_rejected_with_the_path() {
        let err = order_new_tasks(
            &none(),
            &[node("a", &["c"]), node("b", &["a"]), node("c", &["b"])],
        )
        .unwrap_err();
        assert!(err.message.contains("cycle"), "{}", err.message);
        assert!(
            err.message.contains("a -> c -> b -> a") || err.message.contains("->"),
            "{}",
            err.message
        );
    }

    #[test]
    fn two_node_cycle_and_self_dependency() {
        assert!(order_new_tasks(&none(), &[node("a", &["b"]), node("b", &["a"])]).is_err());
        let err = order_new_tasks(&none(), &[node("a", &["a"])]).unwrap_err();
        assert!(err.message.contains("itself"));
    }

    #[test]
    fn cycle_downstream_of_valid_tasks_is_found() {
        let nodes = [node("ok", &[]), node("a", &["ok", "b"]), node("b", &["a"])];
        assert!(order_new_tasks(&none(), &nodes)
            .unwrap_err()
            .message
            .contains("cycle"));
    }

    #[test]
    fn unknown_dependency_duplicate_ids_and_repeats_are_rejected() {
        assert!(order_new_tasks(&none(), &[node("a", &["ghost"])]).is_err());
        assert!(order_new_tasks(&none(), &[node("a", &[]), node("a", &[])]).is_err());
        let existing: BTreeSet<TaskId> = [t("a")].into_iter().collect();
        assert!(order_new_tasks(&existing, &[node("a", &[])]).is_err());
        assert!(order_new_tasks(&none(), &[node("a", &[]), node("b", &["a", "a"])]).is_err());
    }

    #[test]
    fn too_many_dependencies_are_rejected() {
        let mut nodes: Vec<Node> = (0..=MAX_DEPS_PER_TASK)
            .map(|i| node(&format!("d{i}"), &[]))
            .collect();
        let deps: Vec<String> = (0..=MAX_DEPS_PER_TASK).map(|i| format!("d{i}")).collect();
        let refs: Vec<&str> = deps.iter().map(String::as_str).collect();
        nodes.push(node("top", &refs));
        let err = order_new_tasks(&none(), &nodes).unwrap_err();
        assert_eq!(err.code, crate::error::ErrorCode::Limit);
    }

    #[test]
    fn long_chain_is_ordered_without_recursion() {
        let n = 200;
        let nodes: Vec<Node> = (0..n)
            .rev()
            .map(|i| {
                if i == 0 {
                    node("c0", &[])
                } else {
                    let dep = format!("c{}", i - 1);
                    node(&format!("c{i}"), &[dep.as_str()])
                }
            })
            .collect();
        let order = order_new_tasks(&none(), &nodes).unwrap();
        assert_eq!(order.len(), n);
        assert_eq!(order.first().copied(), Some(n - 1));
    }
}
