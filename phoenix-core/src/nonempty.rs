//! A list guaranteed to hold at least one element by construction
//! (DESIGN.md §4: `[LAW:dataflow-not-control-flow]`) — every tmux collection
//! this crate models (a server always has ≥1 session, a session ≥1 window, a
//! window ≥1 pane) can be typed as `NonEmpty<T>` so downstream code never
//! branches on the impossible-empty case.

/// A non-empty `Vec<T>`. The head is split out as its own field so the empty
/// state literally cannot be constructed — there is no `Vec` to be empty.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NonEmpty<T> {
    head: T,
    tail: Vec<T>,
}

impl<T> NonEmpty<T> {
    /// Build from a known-present first element plus the rest.
    pub fn new(head: T, tail: Vec<T>) -> Self {
        Self { head, tail }
    }

    /// A `NonEmpty` with exactly one element.
    pub fn singleton(head: T) -> Self {
        Self {
            head,
            tail: Vec::new(),
        }
    }

    /// Fallible construction from a runtime-sized collection — the boundary
    /// where "tmux guarantees ≥1" gets checked once, at parse time.
    pub fn from_vec(v: Vec<T>) -> Option<Self> {
        let mut iter = v.into_iter();
        let head = iter.next()?;
        Some(Self {
            head,
            tail: iter.collect(),
        })
    }

    pub fn len(&self) -> usize {
        1 + self.tail.len()
    }

    /// Always `false` — kept alongside [`Self::len`] to satisfy the
    /// conventional `len`/`is_empty` pairing, not because the state is
    /// reachable.
    pub fn is_empty(&self) -> bool {
        false
    }

    pub fn first(&self) -> &T {
        &self.head
    }

    pub fn last(&self) -> &T {
        self.tail.last().unwrap_or(&self.head)
    }

    pub fn get(&self, index: usize) -> Option<&T> {
        if index == 0 {
            Some(&self.head)
        } else {
            self.tail.get(index - 1)
        }
    }

    pub fn iter(&self) -> <&Self as IntoIterator>::IntoIter {
        self.into_iter()
    }
}

impl<T> IntoIterator for NonEmpty<T> {
    type Item = T;
    type IntoIter = std::iter::Chain<std::iter::Once<T>, std::vec::IntoIter<T>>;

    fn into_iter(self) -> Self::IntoIter {
        std::iter::once(self.head).chain(self.tail)
    }
}

impl<'a, T> IntoIterator for &'a NonEmpty<T> {
    type Item = &'a T;
    type IntoIter = std::iter::Chain<std::iter::Once<&'a T>, std::slice::Iter<'a, T>>;

    fn into_iter(self) -> Self::IntoIter {
        std::iter::once(&self.head).chain(self.tail.iter())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn singleton_has_len_one() {
        let ne = NonEmpty::singleton(1);
        assert_eq!(ne.len(), 1);
        assert!(!ne.is_empty());
        assert_eq!(ne.first(), &1);
        assert_eq!(ne.last(), &1);
    }

    #[test]
    fn from_vec_rejects_empty() {
        assert_eq!(NonEmpty::<i32>::from_vec(vec![]), None);
    }

    #[test]
    fn from_vec_preserves_order() {
        let ne = NonEmpty::from_vec(vec![1, 2, 3]).unwrap();
        assert_eq!(ne.len(), 3);
        assert_eq!(ne.first(), &1);
        assert_eq!(ne.last(), &3);
        assert_eq!(ne.get(1), Some(&2));
        assert_eq!(ne.get(3), None);
    }

    #[test]
    fn iter_yields_head_then_tail() {
        let ne = NonEmpty::new(1, vec![2, 3]);
        assert_eq!(ne.iter().copied().collect::<Vec<_>>(), vec![1, 2, 3]);
    }

    #[test]
    fn into_iter_by_value_and_by_ref_agree() {
        let ne = NonEmpty::new("a", vec!["b", "c"]);
        let by_ref: Vec<&&str> = (&ne).into_iter().collect();
        assert_eq!(by_ref, vec![&"a", &"b", &"c"]);
        let by_value: Vec<&str> = ne.into_iter().collect();
        assert_eq!(by_value, vec!["a", "b", "c"]);
    }
}
