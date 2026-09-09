//! Snapshot-based undo/redo. The project is small (hundreds of entities at most), so a
//! clone per committed action is simpler and safer than command objects.

use ttg_core::Project;

pub struct History {
    undo: Vec<Project>,
    redo: Vec<Project>,
    limit: usize,
}

impl Default for History {
    fn default() -> Self {
        Self {
            undo: Vec::new(),
            redo: Vec::new(),
            limit: 200,
        }
    }
}

impl History {
    /// Record the state *before* a mutation.
    pub fn push(&mut self, before: Project) {
        if self.undo.last() == Some(&before) {
            return;
        }
        self.undo.push(before);
        if self.undo.len() > self.limit {
            self.undo.remove(0);
        }
        self.redo.clear();
    }

    pub fn undo(&mut self, current: &mut Project) -> bool {
        match self.undo.pop() {
            Some(prev) => {
                self.redo.push(std::mem::replace(current, prev));
                true
            }
            None => false,
        }
    }

    pub fn redo(&mut self, current: &mut Project) -> bool {
        match self.redo.pop() {
            Some(next) => {
                self.undo.push(std::mem::replace(current, next));
                true
            }
            None => false,
        }
    }

    /// Drop the most recent undo step (used by the MCP executor to merge two steps).
    #[cfg_attr(not(feature = "mcp"), allow(dead_code))]
    pub fn pop_last(&mut self) {
        self.undo.pop();
    }

    /// Number of undo steps (used to collapse a batch into one).
    #[cfg_attr(not(feature = "mcp"), allow(dead_code))]
    pub fn len(&self) -> usize {
        self.undo.len()
    }

    /// Drop every undo step recorded after the first `n`.
    #[cfg_attr(not(feature = "mcp"), allow(dead_code))]
    pub fn truncate(&mut self, n: usize) {
        self.undo.truncate(n);
    }

    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }
    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }
    pub fn clear(&mut self) {
        self.undo.clear();
        self.redo.clear();
    }
}
