//! Branch and tag decorations, resolved to the commits they label.

use crate::git::commit::ReadError;
use crate::git::repo::Repo;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefKind {
    LocalBranch,
    RemoteBranch,
    Tag,
}

impl RefKind {
    /// Whether a ref of this kind is drawn under the `o`/`t` toggles.
    ///
    /// Local branches are always shown; `o` hides remote branches and `t`
    /// hides tags. Shared between the graph row renderer and
    /// `GitGraph::decorations_at` so the two never disagree about what
    /// counts as "currently visible".
    pub fn visible(self, show_remotes: bool, show_tags: bool) -> bool {
        match self {
            RefKind::LocalBranch => true,
            RefKind::RemoteBranch => show_remotes,
            RefKind::Tag => show_tags,
        }
    }
}

/// One decoration. `name` is the short form (`main`, `origin/main`, `v1.2`),
/// which is what a graph row has room to print.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefInfo {
    pub name: String,
    pub target: gix::ObjectId,
    pub kind: RefKind,
}

impl Repo {
    /// Every local branch, remote branch, and tag, each resolved to a commit.
    ///
    /// References that cannot be peeled are skipped rather than failing the
    /// whole read: one broken ref should not blank the graph.
    pub fn refs(&self) -> Result<Vec<RefInfo>, ReadError> {
        let mut out = Vec::new();

        // A separate `references()` platform per group, each bound to its own
        // local: the platform is consumed by each listing call in gix's
        // builder style, and cannot be shared across three groups held in one
        // array, nor chained in a single expression (the platform would be a
        // dropped temporary while the iterator still borrows it).
        let local_platform = self
            .gix()
            .references()
            .map_err(|e| ReadError::from_err("opening references", e))?;
        let local = local_platform
            .local_branches()
            .map_err(|e| ReadError::from_err("listing local branches", e))?;
        Self::collect_refs(local, RefKind::LocalBranch, &mut out);

        let remote_platform = self
            .gix()
            .references()
            .map_err(|e| ReadError::from_err("opening references", e))?;
        let remote = remote_platform
            .remote_branches()
            .map_err(|e| ReadError::from_err("listing remote branches", e))?;
        Self::collect_refs(remote, RefKind::RemoteBranch, &mut out);

        let tags_platform = self
            .gix()
            .references()
            .map_err(|e| ReadError::from_err("opening references", e))?;
        let tags = tags_platform
            .tags()
            .map_err(|e| ReadError::from_err("listing tags", e))?;
        Self::collect_refs(tags, RefKind::Tag, &mut out);

        Ok(out)
    }

    /// Peel every reference in `iter` to its commit and push it onto `out`,
    /// silently skipping references that fail to peel.
    fn collect_refs<'repo>(
        iter: impl Iterator<
            Item = Result<gix::Reference<'repo>, Box<dyn std::error::Error + Send + Sync>>,
        >,
        kind: RefKind,
        out: &mut Vec<RefInfo>,
    ) {
        for reference in iter {
            let Ok(mut reference) = reference else {
                continue;
            };
            let name = reference.name().shorten().to_string();
            // peel_to_id, never id(): id() panics on a symbolic ref
            // (refs/remotes/origin/HEAD), and an annotated tag points at a
            // tag object that has to be peeled to reach the commit.
            let Ok(id) = reference.peel_to_id() else {
                continue;
            };
            out.push(RefInfo {
                name,
                target: id.detach(),
                kind,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::fixture::Fixture;
    use crate::git::repo::Repo;

    #[test]
    fn reports_branches_and_tags_by_short_name() {
        let fx = Fixture::new("refs");
        fx.commit("first");
        fx.tag("v1");
        fx.branch("feature");
        fx.commit("second");

        let repo = Repo::open(fx.path()).unwrap();
        let refs = repo.refs().unwrap();

        let mut branches: Vec<_> = refs
            .iter()
            .filter(|r| r.kind == RefKind::LocalBranch)
            .map(|r| r.name.as_str())
            .collect();
        branches.sort_unstable();
        assert_eq!(branches, ["feature", "main"]);

        let tags: Vec<_> = refs
            .iter()
            .filter(|r| r.kind == RefKind::Tag)
            .map(|r| r.name.as_str())
            .collect();
        assert_eq!(tags, ["v1"]);
    }

    #[test]
    fn a_symbolic_ref_does_not_panic() {
        // refs/remotes/origin/HEAD is symbolic in any clone. `Reference::id()`
        // panics on it ("BUG: tries to obtain object id from symbolic
        // target"), which is why refs.rs peels instead.
        let fx = Fixture::new("symbolic");
        let first = fx.commit("first");
        fx.git(&["update-ref", "refs/remotes/origin/main", &first]);
        fx.git(&[
            "symbolic-ref",
            "refs/remotes/origin/HEAD",
            "refs/remotes/origin/main",
        ]);

        let repo = Repo::open(fx.path()).unwrap();
        let refs = repo.refs().expect("peeling a symbolic ref must not panic");
        let remotes: Vec<_> = refs
            .iter()
            .filter(|r| r.kind == RefKind::RemoteBranch)
            .map(|r| r.name.as_str())
            .collect();
        assert!(remotes.contains(&"origin/main"), "{remotes:?}");
    }

    #[test]
    fn an_annotated_tag_peels_to_its_commit() {
        let fx = Fixture::new("annotated");
        let first = fx.commit("first");
        fx.git(&["tag", "-a", "v2", "-m", "release two"]);

        let repo = Repo::open(fx.path()).unwrap();
        let refs = repo.refs().unwrap();
        let tag = refs.iter().find(|r| r.name == "v2").expect("tag is listed");
        assert_eq!(
            tag.target.to_string(),
            first,
            "an annotated tag must resolve to the commit, not the tag object"
        );
    }

    #[test]
    fn local_branches_are_never_hidden_by_the_toggles() {
        assert!(RefKind::LocalBranch.visible(false, false));
        assert!(RefKind::LocalBranch.visible(true, true));
    }

    #[test]
    fn remote_branches_and_tags_follow_their_own_toggle() {
        assert!(RefKind::RemoteBranch.visible(true, false));
        assert!(!RefKind::RemoteBranch.visible(false, true));
        assert!(RefKind::Tag.visible(false, true));
        assert!(!RefKind::Tag.visible(true, false));
    }
}
