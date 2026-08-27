//! One row's worth of commit facts, and the walk that produces them.

/// Anything that went wrong reading the repository. The message is shown to the
/// user, so it carries gix's text rather than a code.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct ReadError(pub String);

impl ReadError {
    pub(crate) fn from_err(context: &str, e: impl std::fmt::Display) -> Self {
        Self(format!("{context}: {e}"))
    }
}

/// The facts a graph row needs. Owned, so a row survives the gix objects it
/// came from and the layout can be held across loads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitInfo {
    pub id: gix::ObjectId,
    /// Every parent, first parent first. A merge has two or more.
    pub parents: Vec<gix::ObjectId>,
    pub summary: String,
    pub author: String,
    /// Commit time, seconds since the Unix epoch.
    pub time: i64,
}

#[cfg(test)]
mod tests {
    use crate::git::fixture::Fixture;
    use crate::git::repo::Repo;

    #[test]
    fn walks_a_linear_history_newest_first() {
        let fx = Fixture::new("linear");
        fx.commit("first");
        fx.commit("second");
        fx.commit("third");

        let repo = Repo::open(fx.path()).unwrap();
        let commits: Vec<_> = repo.walk().unwrap().collect::<Result<Vec<_>, _>>().unwrap();

        let summaries: Vec<_> = commits.iter().map(|c| c.summary.as_str()).collect();
        assert_eq!(summaries, ["third", "second", "first"]);
        assert_eq!(commits[0].parents.len(), 1);
        assert_eq!(commits[0].parents[0], commits[1].id);
        // The root commit has no parents.
        assert!(commits[2].parents.is_empty());
        assert_eq!(commits[0].author, "asd test");
        assert!(commits[0].time > commits[2].time);
    }

    #[test]
    fn a_merge_commit_reports_both_parents() {
        let fx = Fixture::new("merge-parents");
        fx.commit("base");
        fx.branch("side");
        fx.commit("on side");
        fx.checkout("main");
        fx.commit("on main");
        let merge = fx.merge("side", "merge side");

        let repo = Repo::open(fx.path()).unwrap();
        let commits: Vec<_> = repo.walk().unwrap().collect::<Result<Vec<_>, _>>().unwrap();
        let head = commits
            .iter()
            .find(|c| c.id == merge)
            .expect("merge commit is in the walk");
        assert_eq!(head.parents.len(), 2, "merge has two parents");
    }
}
