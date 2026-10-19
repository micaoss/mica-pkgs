//! One item as the bridge mirrors it: what `GetItems` and `ItemsChanged`
//! carry per path.

use serde_json::Value as Json;

/// The attributes of one item, converted out of the `a{sv}` the bus carries.
#[derive(Debug, Clone, PartialEq)]
pub struct Item {
    pub value: Json,
    pub writable: bool,
    /// `min`/`max` travel when the publishing application carries them.
    pub min: Option<Json>,
    pub max: Option<Json>,
}

impl Item {
    /// A plain read-only item with no bounds — the shape most of the tree has.
    pub fn new(value: Json) -> Self {
        Self {
            value,
            writable: false,
            min: None,
            max: None,
        }
    }

    /// The same item marked writable.
    pub fn writable(value: Json) -> Self {
        Self {
            writable: true,
            ..Self::new(value)
        }
    }
}
