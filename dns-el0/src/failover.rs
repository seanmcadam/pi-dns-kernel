//! Which upstream to try next, and when the list is exhausted.
//!
//! The first attempt uses the server that answered last. A timeout walks
//! every other server once, then the query fails. The caller remembers the
//! index that actually answered.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Choice {
    pub index: usize,
    pub tries: u8,
}

pub fn first(len: usize, preferred: usize) -> Option<Choice> {
    if len == 0 {
        return None;
    }
    let index = if preferred < len { preferred } else { 0 };
    Some(Choice { index, tries: 1 })
}

/// `start` is the index of the first attempt. `tries` is how many sends
/// have already been made. `None` means each upstream has been tried.
pub fn after_timeout(len: usize, start: usize, tries: u8) -> Option<Choice> {
    if len == 0 || tries as usize >= len {
        return None;
    }
    let start = if start < len { start } else { 0 };
    Some(Choice {
        index: (start + tries as usize) % len,
        tries: tries + 1,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starts_at_the_preferred_server_and_walks_the_rest_once() {
        let first = first(5, 2).unwrap();
        assert_eq!(first, Choice { index: 2, tries: 1 });
        let mut choice = first;
        let mut seen = vec![choice.index];
        while let Some(next) = after_timeout(5, first.index, choice.tries) {
            seen.push(next.index);
            choice = next;
        }
        assert_eq!(seen, vec![2, 3, 4, 0, 1]);
        assert!(after_timeout(5, first.index, choice.tries).is_none());
    }

    #[test]
    fn empty_list_does_not_start() {
        assert!(first(0, 0).is_none());
        assert!(after_timeout(0, 0, 1).is_none());
    }

    #[test]
    fn preferred_past_the_end_starts_at_zero() {
        assert_eq!(first(3, 9).unwrap().index, 0);
    }
}
