//! Bounded document history shared by typing and structured native edits.
#[derive(Clone)]
pub struct History<T> {
    current: T,
    undo: Vec<T>,
    redo: Vec<T>,
    weight: fn(&T) -> usize,
}

impl<T: Clone + PartialEq> History<T> {
    pub fn new(current: T, weight: fn(&T) -> usize) -> Self {
        Self {
            current,
            undo: Vec::new(),
            redo: Vec::new(),
            weight,
        }
    }
    pub fn record(&mut self, next: T) {
        if next == self.current {
            return;
        }
        self.undo.push(std::mem::replace(&mut self.current, next));
        self.redo.clear();
        self.trim();
    }
    pub fn undo(&mut self) -> Option<T> {
        let previous = self.undo.pop()?;
        self.redo
            .push(std::mem::replace(&mut self.current, previous));
        Some(self.current.clone())
    }
    pub fn redo(&mut self) -> Option<T> {
        let next = self.redo.pop()?;
        self.undo.push(std::mem::replace(&mut self.current, next));
        Some(self.current.clone())
    }
    fn trim(&mut self) {
        while self.undo.len() > 64
            || self.undo.iter().map(self.weight).sum::<usize>() > 4 * 1024 * 1024
        {
            self.undo.remove(0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::History;
    #[test]
    fn history_unifies_edits_and_clears_redo_on_branch() {
        let mut h = History::new("initial".to_owned(), String::len);
        h.record("typed".into());
        h.record("structured".into());
        assert_eq!(h.undo().as_deref(), Some("typed"));
        assert_eq!(h.undo().as_deref(), Some("initial"));
        assert_eq!(h.redo().as_deref(), Some("typed"));
        h.record("other edit".into());
        assert!(h.redo().is_none());
    }
    #[test]
    fn unchanged_snapshots_do_not_add_history_and_memory_is_bounded() {
        let mut h = History::new(String::new(), String::len);
        h.record(String::new());
        assert!(h.undo().is_none());
        for i in 0..100 {
            h.record(format!("{i}{}", "x".repeat(100_000)));
        }
        let mut count = 0;
        while h.undo().is_some() {
            count += 1;
        }
        assert!(count <= 42 && count > 0);
    }
}
