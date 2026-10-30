use serde::{Deserialize, Serialize};

/// A recorded package (or `docker`) a recreate could not restore.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Skipped {
    pub name: String,
    /// One line of at most 256 bytes: whitespace runs become one space,
    /// other control characters are escaped.
    pub reason: String,
}
impl Skipped {
    pub fn new(name: &str, reason: &str) -> Self {
        let text = |s: &str| {
            let mut bounded = String::new();
            let line = s.split_whitespace().collect::<Vec<_>>().join(" ");
            for c in goblins_protocol::plain_text(&line).chars() {
                if bounded.len() + c.len_utf8() > 256 {
                    break;
                }
                bounded.push(c);
            }
            bounded
        };
        Self {
            name: text(name),
            reason: text(reason),
        }
    }
}
