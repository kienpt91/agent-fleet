//! A layer while it is being assembled: groups of items with priorities, from which whole items
//! can be removed, lowest priority first, until the layer fits.

use crate::layer::LayerId;
use crate::limits::MAX_MARKED_DROPS;
use crate::pack::escape;

#[derive(Clone, Debug)]
pub(crate) struct Item {
    /// Stable name of the item, listed in the manifest when the item is dropped.
    pub key: String,
    /// Higher is kept longer. Items of equal priority are dropped last one first.
    pub priority: u32,
    /// A required item is never dropped.
    pub required: bool,
    /// Already cleaned, framed and labelled text.
    pub text: String,
    pub unverified: bool,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct Group {
    pub heading: Option<String>,
    pub items: Vec<Item>,
}

#[derive(Clone, Debug)]
pub(crate) struct Draft {
    pub id: LayerId,
    pub groups: Vec<Group>,
    /// Shown when the layer has nothing at all (and nothing was dropped).
    pub empty_note: Option<String>,
    pub dropped: Vec<String>,
    /// Unverified items left out by policy.
    pub excluded: u32,
    pub clipped: bool,
}

impl Draft {
    pub fn new(id: LayerId) -> Draft {
        Draft {
            id,
            groups: Vec::new(),
            empty_note: None,
            dropped: Vec::new(),
            excluded: 0,
            clipped: false,
        }
    }

    pub fn item_count(&self) -> usize {
        self.groups.iter().map(|g| g.items.len()).sum()
    }

    pub fn unverified_count(&self) -> usize {
        self.groups
            .iter()
            .flat_map(|g| g.items.iter())
            .filter(|i| i.unverified)
            .count()
    }

    pub fn trust_label(&self) -> &'static str {
        if self.unverified_count() > 0 {
            "contains-unverified"
        } else {
            "trusted"
        }
    }

    fn marker(&self) -> Option<String> {
        if self.dropped.is_empty() {
            return None;
        }
        let shown: Vec<&str> = self
            .dropped
            .iter()
            .take(MAX_MARKED_DROPS)
            .map(String::as_str)
            .collect();
        let more = self.dropped.len().saturating_sub(shown.len());
        let mut text = format!("(left out to fit the token budget: {}", shown.join(", "));
        if more > 0 {
            text.push_str(&format!(" and {more} more"));
        }
        text.push(')');
        Some(text)
    }

    /// The body before line escaping.
    pub fn body(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        for g in &self.groups {
            if g.items.is_empty() {
                continue;
            }
            let mut part = String::new();
            if let Some(h) = &g.heading {
                part.push_str(h);
                part.push('\n');
            }
            let texts: Vec<&str> = g.items.iter().map(|i| i.text.as_str()).collect();
            part.push_str(&texts.join("\n"));
            parts.push(part);
        }
        if parts.is_empty() && self.dropped.is_empty() {
            if let Some(note) = &self.empty_note {
                parts.push(note.clone());
            }
        }
        if let Some(marker) = self.marker() {
            parts.push(marker);
        }
        parts.join("\n\n")
    }

    /// The body as it is written into the pack.
    pub fn escaped_body(&self) -> String {
        escape(&self.body())
    }

    /// Removes the item that matters least and is allowed to go: the lowest priority, and among
    /// equals the last one. Returns false when nothing may be removed.
    pub fn shed_one(&mut self) -> bool {
        let mut pick: Option<(usize, usize, u32)> = None;
        for (gi, g) in self.groups.iter().enumerate() {
            for (ii, item) in g.items.iter().enumerate() {
                if item.required {
                    continue;
                }
                let better = match pick {
                    None => true,
                    // `<=`: a later item of the same priority goes first
                    Some((_, _, p)) => item.priority <= p,
                };
                if better {
                    pick = Some((gi, ii, item.priority));
                }
            }
        }
        let Some((gi, ii, _)) = pick else {
            return false;
        };
        let Some(group) = self.groups.get_mut(gi) else {
            return false;
        };
        let removed = group.items.remove(ii);
        self.dropped.push(removed.key);
        true
    }
}

pub(crate) fn item(key: impl Into<String>, priority: u32, required: bool, text: String) -> Item {
    Item {
        key: key.into(),
        priority,
        required,
        text,
        unverified: false,
    }
}

pub(crate) fn unverified_item(key: impl Into<String>, priority: u32, text: String) -> Item {
    Item {
        key: key.into(),
        priority,
        required: false,
        text,
        unverified: true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn draft() -> Draft {
        let mut d = Draft::new(LayerId::RecentEvents);
        d.groups.push(Group {
            heading: Some("## A".to_string()),
            items: vec![
                item("a1", 5, false, "a1 text".to_string()),
                item("a2", 5, false, "a2 text".to_string()),
                item("a3", 9, false, "a3 text".to_string()),
            ],
        });
        d.groups.push(Group {
            heading: Some("## B".to_string()),
            items: vec![
                item("b1", 1, false, "b1 text".to_string()),
                item("keep", 0, true, "kept".to_string()),
            ],
        });
        d
    }

    #[test]
    fn items_go_lowest_priority_first_and_the_last_of_equals_first() {
        let mut d = draft();
        assert_eq!(d.item_count(), 5);
        assert!(d.shed_one());
        assert_eq!(d.dropped, vec!["b1"]);
        assert!(d.shed_one());
        assert_eq!(d.dropped, vec!["b1", "a2"]);
        assert!(d.shed_one());
        assert!(d.shed_one());
        assert_eq!(d.dropped, vec!["b1", "a2", "a1", "a3"]);
        assert!(!d.shed_one(), "a required item is never dropped");
        assert_eq!(d.item_count(), 1);
    }

    #[test]
    fn the_body_has_headings_for_non_empty_groups_and_names_what_was_dropped() {
        let mut d = draft();
        assert_eq!(
            d.body(),
            "## A\na1 text\na2 text\na3 text\n\n## B\nb1 text\nkept"
        );
        d.shed_one();
        d.shed_one();
        d.shed_one();
        d.shed_one();
        assert_eq!(
            d.body(),
            "## B\nkept\n\n(left out to fit the token budget: b1, a2, a1, a3)"
        );
        let mut many = Draft::new(LayerId::WorkingSet);
        many.dropped = (0..9).map(|i| format!("k{i}")).collect();
        assert_eq!(
            many.body(),
            "(left out to fit the token budget: k0, k1, k2, k3, k4, k5 and 3 more)"
        );
    }

    #[test]
    fn an_empty_layer_says_so_unless_something_was_dropped() {
        let mut d = Draft::new(LayerId::Knowledge);
        d.empty_note = Some("nothing here".to_string());
        assert_eq!(d.body(), "nothing here");
        d.dropped.push("x".to_string());
        assert!(!d.body().contains("nothing here"));
    }

    #[test]
    fn trust_label_follows_the_items() {
        let mut d = Draft::new(LayerId::RecentEvents);
        assert_eq!(d.trust_label(), "trusted");
        d.groups.push(Group {
            heading: None,
            items: vec![unverified_item("n", 1, "UNVERIFIED".to_string())],
        });
        assert_eq!(d.trust_label(), "contains-unverified");
        assert_eq!(d.unverified_count(), 1);
        d.shed_one();
        assert_eq!(d.trust_label(), "trusted");
    }

    #[test]
    fn escaping_applies_to_the_whole_body() {
        let mut d = Draft::new(LayerId::TaskSpec);
        d.groups.push(Group {
            heading: None,
            items: vec![item("x", 1, true, "@@neo-end 4 0\n\\x".to_string())],
        });
        assert_eq!(d.escaped_body(), "\\@@neo-end 4 0\n\\\\x");
    }
}
