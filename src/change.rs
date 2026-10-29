//! A mutation reported by the WebDAV filesystem to the committer.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    Put,
    Delete,
    Mkcol,
    Move,
    Copy,
}

impl Op {
    pub fn word(self) -> &'static str {
        match self {
            Op::Put => "put",
            Op::Delete => "delete",
            Op::Mkcol => "mkcol",
            Op::Move => "move",
            Op::Copy => "copy",
        }
    }
}

/// Paths are repo-relative, use `/` separators and have no leading slash.
/// For `Move` and `Copy`, `path` is the source and `to` is the destination.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    pub op: Op,
    pub path: String,
    pub to: Option<String>,
}

impl Change {
    pub fn new(op: Op, path: impl Into<String>) -> Self {
        Change {
            op,
            path: path.into(),
            to: None,
        }
    }

    pub fn with_target(op: Op, path: impl Into<String>, to: impl Into<String>) -> Self {
        Change {
            op,
            path: path.into(),
            to: Some(to.into()),
        }
    }

    /// Paths that must be staged to record this change.
    pub fn staged_paths(&self) -> Vec<&str> {
        match (self.op, &self.to) {
            (Op::Move, Some(to)) => vec![self.path.as_str(), to.as_str()],
            (Op::Copy, Some(to)) => vec![to.as_str()],
            _ => vec![self.path.as_str()],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn staged_paths_per_op() {
        assert_eq!(Change::new(Op::Put, "a").staged_paths(), vec!["a"]);
        assert_eq!(Change::new(Op::Delete, "a").staged_paths(), vec!["a"]);
        assert_eq!(Change::new(Op::Mkcol, "a").staged_paths(), vec!["a"]);
        assert_eq!(
            Change::with_target(Op::Move, "a", "b").staged_paths(),
            vec!["a", "b"]
        );
        // a copy leaves the source untouched
        assert_eq!(
            Change::with_target(Op::Copy, "a", "b").staged_paths(),
            vec!["b"]
        );
    }

    #[test]
    fn op_words() {
        assert_eq!(Op::Put.word(), "put");
        assert_eq!(Op::Move.word(), "move");
    }
}
