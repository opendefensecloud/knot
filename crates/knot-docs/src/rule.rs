//! The pure access rule for restricted documents.
//!
//! Access is a top-down fold over the root-to-doc path. Outside a restricted
//! subtree a doc's role is the max of the workspace role, the user's grants
//! on the doc, and their inherit=true grants on ancestors. A restricted doc
//! is a hard boundary: from it downwards the workspace role stops applying
//! and grants above it stop flowing in, so only grants on the boundary and
//! below count. The innermost boundary wins.
//!
//! Two evaluators share the one `step` function, so they cannot drift:
//! [`access_along_path`] folds a single ancestor chain (for `acl::resolve`)
//! and [`compute_access`] folds a whole workspace tree (for listings).

use std::collections::{HashMap, HashSet};

use knot_storage::WorkspaceRole;
use uuid::Uuid;

use crate::acl::{DocAccess, max};

/// One document as the rule sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AclNode {
    pub id: Uuid,
    pub parent_id: Option<Uuid>,
    pub restricted: bool,
    pub archived: bool,
}

/// One grant of the user being evaluated. Callers pass only that user's
/// grants; the rule does not look at principals.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PrincipalGrant {
    pub doc_id: Uuid,
    pub role: WorkspaceRole,
    pub inherit: bool,
}

/// What flows from a doc down to its children.
#[derive(Debug, Clone, Copy)]
struct FoldState {
    /// False from the nearest restricted ancestor-or-self downwards.
    workspace_role_applies: bool,
    /// Max role of the inherit=true grants since the last boundary.
    inherited: Option<WorkspaceRole>,
    /// The nearest restricted ancestor-or-self.
    boundary: Option<Uuid>,
}

impl FoldState {
    const ROOT: FoldState = FoldState {
        workspace_role_applies: true,
        inherited: None,
        boundary: None,
    };
}

/// The user's grants on one doc, folded to the two maxima the rule needs.
#[derive(Debug, Clone, Copy, Default)]
struct DocGrants {
    /// Max role of every grant on the doc, inheriting or not.
    any: Option<WorkspaceRole>,
    /// Max role of the inherit=true grants on the doc.
    inheriting: Option<WorkspaceRole>,
}

fn max_option(a: Option<WorkspaceRole>, b: Option<WorkspaceRole>) -> Option<WorkspaceRole> {
    match (a, b) {
        (Some(a), Some(b)) => Some(max(a, b)),
        (a, None) => a,
        (None, b) => b,
    }
}

fn grants_by_doc(grants: &[PrincipalGrant]) -> HashMap<Uuid, DocGrants> {
    let mut by_doc: HashMap<Uuid, DocGrants> = HashMap::new();
    for grant in grants {
        let doc_grants = by_doc.entry(grant.doc_id).or_default();
        doc_grants.any = max_option(doc_grants.any, Some(grant.role));
        if grant.inherit {
            doc_grants.inheriting = max_option(doc_grants.inheriting, Some(grant.role));
        }
    }
    by_doc
}

/// Evaluates one node given the state its parent passed down. Returns the
/// node's access and the state to pass on to its children.
fn step(
    parent_state: FoldState,
    node: &AclNode,
    grants: &HashMap<Uuid, DocGrants>,
    workspace_role: WorkspaceRole,
) -> (Option<DocAccess>, FoldState) {
    let mut state = if node.restricted {
        FoldState {
            workspace_role_applies: false,
            inherited: None,
            boundary: Some(node.id),
        }
    } else {
        parent_state
    };
    let node_grants = grants.get(&node.id).copied().unwrap_or_default();
    let workspace_contribution = state.workspace_role_applies.then_some(workspace_role);
    let role = max_option(
        max_option(node_grants.any, state.inherited),
        workspace_contribution,
    );
    state.inherited = max_option(state.inherited, node_grants.inheriting);
    let access = role.map(|role| DocAccess {
        role,
        archived: node.archived,
        restriction_root: state.boundary,
    });
    (access, state)
}

/// Access to the LAST node of `path` (root first). None = no access.
///
/// `grants` are the user's grants; ones on docs outside `path` are ignored.
/// A `workspace_role` of `None` (not a member) gives no access, whatever the
/// grants say.
pub fn access_along_path(
    path: &[AclNode],
    grants: &[PrincipalGrant],
    workspace_role: Option<WorkspaceRole>,
) -> Option<DocAccess> {
    let workspace_role = workspace_role?;
    let grants = grants_by_doc(grants);
    let mut state = FoldState::ROOT;
    let mut access = None;
    for node in path {
        (access, state) = step(state, node, &grants, workspace_role);
    }
    access
}

/// Access for every node of a workspace tree; nodes with no access are absent.
///
/// A node whose `parent_id` is not among `nodes` is treated as a root. Nodes
/// on a parent cycle, and everything below them, are unreachable from any
/// root and so get no access (fail closed).
pub fn compute_access(
    nodes: &[AclNode],
    grants: &[PrincipalGrant],
    workspace_role: Option<WorkspaceRole>,
) -> HashMap<Uuid, DocAccess> {
    let mut access_by_doc = HashMap::new();
    let Some(workspace_role) = workspace_role else {
        return access_by_doc;
    };
    let grants = grants_by_doc(grants);
    let node_ids: HashSet<Uuid> = nodes.iter().map(|node| node.id).collect();
    let mut children_by_parent: HashMap<Uuid, Vec<&AclNode>> = HashMap::new();
    let mut stack: Vec<(&AclNode, FoldState)> = Vec::new();
    for node in nodes {
        match node.parent_id {
            Some(parent_id) if node_ids.contains(&parent_id) => {
                children_by_parent.entry(parent_id).or_default().push(node);
            }
            _ => stack.push((node, FoldState::ROOT)),
        }
    }
    // Guards against revisiting a node, which duplicate ids could otherwise
    // turn into an endless walk.
    let mut visited: HashSet<Uuid> = HashSet::with_capacity(nodes.len());
    while let Some((node, parent_state)) = stack.pop() {
        if !visited.insert(node.id) {
            continue;
        }
        let (access, state) = step(parent_state, node, &grants, workspace_role);
        if let Some(access) = access {
            access_by_doc.insert(node.id, access);
        }
        if let Some(children) = children_by_parent.get(&node.id) {
            stack.extend(children.iter().map(|child| (*child, state)));
        }
    }
    access_by_doc
}

#[cfg(test)]
mod tests {
    use super::*;
    use knot_storage::WorkspaceRole::{self, Editor, Owner, Viewer};
    use uuid::Uuid;

    fn node(id: Uuid, parent_id: Option<Uuid>, restricted: bool) -> AclNode {
        AclNode {
            id,
            parent_id,
            restricted,
            archived: false,
        }
    }

    fn grant(doc_id: Uuid, role: WorkspaceRole, inherit: bool) -> PrincipalGrant {
        PrincipalGrant {
            doc_id,
            role,
            inherit,
        }
    }

    fn id(n: u128) -> Uuid {
        Uuid::from_u128(n)
    }

    #[test]
    fn unrestricted_uses_max_of_workspace_role_and_grants() {
        let root = node(id(1), None, false);
        let child = node(id(2), Some(root.id), false);
        let grants = [grant(root.id, Editor, true)];

        let child_access = access_along_path(&[root, child], &grants, Some(Viewer)).unwrap();

        assert_eq!(child_access.role, Editor);
        assert_eq!(child_access.restriction_root, None);
    }

    #[test]
    fn restricted_boundary_ignores_workspace_role() {
        let root = node(id(1), None, true);

        assert_eq!(access_along_path(&[root], &[], Some(Owner)), None);
    }

    #[test]
    fn grant_on_boundary_inherits_down() {
        let boundary = node(id(1), None, true);
        let child = node(id(2), Some(boundary.id), false);
        let grants = [grant(boundary.id, Owner, true)];

        let child_access = access_along_path(&[boundary, child], &grants, Some(Viewer)).unwrap();

        assert_eq!(child_access.role, Owner);
        assert_eq!(child_access.restriction_root, Some(boundary.id));
    }

    #[test]
    fn grant_above_boundary_does_not_cross() {
        let root = node(id(1), None, false);
        let boundary = node(id(2), Some(root.id), true);
        let grants = [grant(root.id, Editor, true)];

        let boundary_access = access_along_path(&[root, boundary], &grants, Some(Viewer));
        let root_access = access_along_path(&[root], &grants, Some(Viewer)).unwrap();

        assert_eq!(boundary_access, None);
        assert_eq!(root_access.role, Editor);
        assert_eq!(root_access.restriction_root, None);
    }

    #[test]
    fn nested_boundary_resets_inheritance() {
        let outer = node(id(1), None, true);
        let inner = node(id(2), Some(outer.id), true);
        let plain_child = node(id(3), Some(outer.id), false);
        let grants = [grant(outer.id, Owner, true)];

        let inner_access = access_along_path(&[outer, inner], &grants, Some(Editor));
        let plain_child_access =
            access_along_path(&[outer, plain_child], &grants, Some(Editor)).unwrap();

        assert_eq!(inner_access, None);
        assert_eq!(plain_child_access.role, Owner);
        assert_eq!(plain_child_access.restriction_root, Some(outer.id));
    }

    #[test]
    fn non_inheriting_grant_covers_only_its_doc() {
        let boundary = node(id(1), None, true);
        let child = node(id(2), Some(boundary.id), false);
        let grants = [grant(boundary.id, Editor, false)];

        let boundary_access = access_along_path(&[boundary], &grants, Some(Viewer)).unwrap();
        let child_access = access_along_path(&[boundary, child], &grants, Some(Viewer));

        assert_eq!(boundary_access.role, Editor);
        assert_eq!(boundary_access.restriction_root, Some(boundary.id));
        assert_eq!(child_access, None);
    }

    #[test]
    fn non_member_gets_nothing_even_with_grant() {
        let root = node(id(1), None, false);
        let grants = [grant(root.id, Owner, true)];

        assert_eq!(access_along_path(&[root], &grants, None), None);
        assert!(compute_access(&[root], &grants, None).is_empty());
    }

    #[test]
    fn archived_flag_is_carried() {
        let root = node(id(1), None, false);
        let archived_child = AclNode {
            archived: true,
            ..node(id(2), Some(root.id), false)
        };

        let child_access = access_along_path(&[root, archived_child], &[], Some(Editor)).unwrap();
        let batched = compute_access(&[root, archived_child], &[], Some(Editor));

        assert!(child_access.archived);
        assert_eq!(child_access.role, Editor);
        assert_eq!(child_access.restriction_root, None);
        assert!(batched[&archived_child.id].archived);
        assert!(!batched[&root.id].archived);
    }

    #[test]
    fn orphan_is_treated_as_root() {
        let missing_parent = id(99);
        let orphan = node(id(1), Some(missing_parent), false);
        let orphan_child = node(id(2), Some(orphan.id), false);

        let access = compute_access(&[orphan_child, orphan], &[], Some(Viewer));

        assert_eq!(access[&orphan.id].role, Viewer);
        assert_eq!(access[&orphan.id].restriction_root, None);
        assert_eq!(access[&orphan_child.id].role, Viewer);
        assert_eq!(access[&orphan_child.id].restriction_root, None);
    }

    #[test]
    fn cycle_fails_closed() {
        let root = node(id(1), None, false);
        let cycle_a = node(id(2), Some(id(3)), false);
        let cycle_b = node(id(3), Some(id(2)), false);
        let below_cycle = node(id(4), Some(cycle_a.id), false);
        let grants = [grant(cycle_a.id, Owner, true)];

        let access = compute_access(&[root, cycle_a, cycle_b, below_cycle], &grants, Some(Owner));

        assert_eq!(access[&root.id].role, Owner);
        assert!(!access.contains_key(&cycle_a.id));
        assert!(!access.contains_key(&cycle_b.id));
        assert!(!access.contains_key(&below_cycle.id));
    }

    /// A small linear congruential generator, so the parity test is
    /// deterministic without pulling in a random-number crate.
    struct Lcg(u64);

    impl Lcg {
        fn next(&mut self) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            self.0 >> 33
        }

        fn below(&mut self, bound: u64) -> u64 {
            self.next() % bound
        }
    }

    fn random_role(generator: &mut Lcg) -> WorkspaceRole {
        [Owner, Editor, Viewer][generator.below(3) as usize]
    }

    /// Root-first path from the tree's root down to `doc_id`.
    fn path_to(nodes: &[AclNode], doc_id: Uuid) -> Vec<AclNode> {
        let by_id: std::collections::HashMap<Uuid, AclNode> = nodes
            .iter()
            .map(|tree_node| (tree_node.id, *tree_node))
            .collect();
        let mut path = vec![by_id[&doc_id]];
        while let Some(parent_id) = path.last().unwrap().parent_id {
            path.push(by_id[&parent_id]);
        }
        path.reverse();
        path
    }

    #[test]
    fn compute_access_matches_access_along_path() {
        for tree_index in 0..500u64 {
            let mut generator = Lcg(tree_index.wrapping_mul(0x9E37_79B9_7F4A_7C15) + 1);
            let node_count = 1 + generator.below(30) as usize;
            let mut nodes: Vec<AclNode> = Vec::with_capacity(node_count);
            for node_index in 0..node_count {
                // Parents always come earlier in the list, so the tree is acyclic.
                let parent_id = if node_index == 0 || generator.below(4) == 0 {
                    None
                } else {
                    Some(nodes[generator.below(node_index as u64) as usize].id)
                };
                nodes.push(AclNode {
                    id: Uuid::from_u128(node_index as u128 + 1),
                    parent_id,
                    restricted: generator.below(4) == 0,
                    archived: generator.below(6) == 0,
                });
            }
            let grant_count = generator.below(8) as usize;
            let grants: Vec<PrincipalGrant> = (0..grant_count)
                .map(|_| PrincipalGrant {
                    doc_id: nodes[generator.below(node_count as u64) as usize].id,
                    role: random_role(&mut generator),
                    inherit: generator.below(2) == 0,
                })
                .collect();
            let workspace_role = if generator.below(8) == 0 {
                None
            } else {
                Some(random_role(&mut generator))
            };

            let batched = compute_access(&nodes, &grants, workspace_role);

            for tree_node in &nodes {
                let along_path =
                    access_along_path(&path_to(&nodes, tree_node.id), &grants, workspace_role);
                assert_eq!(
                    batched.get(&tree_node.id).copied(),
                    along_path,
                    "tree {tree_index}, node {}",
                    tree_node.id
                );
            }
        }
    }
}
