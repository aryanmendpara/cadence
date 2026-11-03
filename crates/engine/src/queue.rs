//! The play queue: canonical track ids in order, plus a cursor.
//!
//! Pure data with no I/O so the ordering rules can be tested without an audio device.

/// Ordered track ids and where playback currently is.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Queue {
    ids: Vec<i64>,
    cursor: Option<usize>,
}

impl Queue {
    /// An empty queue.
    pub fn new() -> Self {
        Self::default()
    }

    /// Replace the contents. The cursor resets to the first item, or `None` when empty.
    pub fn set(&mut self, ids: &[i64]) {
        self.ids = ids.to_vec();
        self.cursor = (!self.ids.is_empty()).then_some(0);
    }

    /// Number of items.
    pub fn len(&self) -> usize {
        self.ids.len()
    }

    /// True when there is nothing to play.
    pub fn is_empty(&self) -> bool {
        self.ids.is_empty()
    }

    /// All track ids, in order.
    pub fn ids(&self) -> &[i64] {
        &self.ids
    }

    /// Index of the current item, if any.
    pub fn cursor(&self) -> Option<usize> {
        self.cursor
    }

    /// Move the cursor to `index`. Returns `false` when out of range.
    pub fn set_cursor(&mut self, index: usize) -> bool {
        if index >= self.ids.len() {
            return false;
        }
        self.cursor = Some(index);
        true
    }

    /// The current track id.
    pub fn current(&self) -> Option<i64> {
        self.cursor.and_then(|index| self.ids.get(index).copied())
    }

    /// The item at `index`.
    pub fn at(&self, index: usize) -> Option<i64> {
        self.ids.get(index).copied()
    }

    /// The item after the cursor, without moving it.
    pub fn peek_next(&self) -> Option<(usize, i64)> {
        let index = match self.cursor {
            Some(cursor) => cursor + 1,
            None => 0,
        };
        self.at(index).map(|id| (index, id))
    }

    /// Move to the next item and return it, or `None` at the end of the queue.
    pub fn advance(&mut self) -> Option<(usize, i64)> {
        let (index, id) = self.peek_next()?;
        self.cursor = Some(index);
        Some((index, id))
    }

    /// Move to the previous item. Stays put at the start.
    pub fn back(&mut self) -> Option<(usize, i64)> {
        let index = self.cursor.map_or(0, |cursor| cursor.saturating_sub(1));
        let id = self.at(index)?;
        self.cursor = Some(index);
        Some((index, id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fresh_queue_points_at_the_first_item() {
        let mut queue = Queue::new();
        assert!(queue.is_empty());
        assert_eq!(queue.current(), None);
        assert_eq!(queue.peek_next(), None);

        queue.set(&[7, 8, 9]);
        assert_eq!(queue.cursor(), Some(0));
        assert_eq!(queue.current(), Some(7));
    }

    #[test]
    fn advance_walks_forward_then_stops() {
        let mut queue = Queue::new();
        queue.set(&[7, 8]);

        assert_eq!(queue.advance(), Some((1, 8)));
        assert_eq!(queue.advance(), None);
        // Failure does not move the cursor.
        assert_eq!(queue.cursor(), Some(1));
        assert_eq!(queue.current(), Some(8));
    }

    #[test]
    fn back_clamps_at_the_first_item() {
        let mut queue = Queue::new();
        queue.set(&[7, 8, 9]);

        assert_eq!(queue.back(), Some((0, 7)));
        assert_eq!(queue.back(), Some((0, 7)));
        assert!(queue.set_cursor(2));
        assert_eq!(queue.back(), Some((1, 8)));
        assert!(!queue.set_cursor(3));
    }

    #[test]
    fn peek_does_not_move_the_cursor() {
        let mut queue = Queue::new();
        queue.set(&[7, 8]);
        assert_eq!(queue.peek_next(), Some((1, 8)));
        assert_eq!(queue.cursor(), Some(0));
    }

    #[test]
    fn replacing_the_contents_resets_the_cursor() {
        let mut queue = Queue::new();
        queue.set(&[7, 8, 9]);
        queue.set_cursor(2);
        queue.set(&[1]);
        assert_eq!(queue.cursor(), Some(0));

        queue.set(&[]);
        assert_eq!(queue.cursor(), None);
        assert_eq!(queue.advance(), None);
    }
}
