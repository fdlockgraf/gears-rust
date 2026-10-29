//! The record-acceptance request (S4-08b; 05 §2.2 *Record acceptance of the current version*;
//! Stage 4 DESIGN §5 "Standalone consent"; D-255).
//!
//! The customer's consent to the order's current committed version after submit. The body has no
//! member (`models.json` `Acceptance`): the accepted version is `If-Match`
//! (`CallMeta.expected_version`), and the instant, the recording actor, the recording path and
//! the requirement source are server facts. A caller-supplied timestamp or actor is therefore an
//! unknown member the wire refuses (`request-invalid`), never assent.
use serde::de::{self, Deserializer, MapAccess, Visitor};
use serde::ser::{SerializeMap, Serializer};
use serde::{Deserialize, Serialize};

/// A record-acceptance request: exactly an empty JSON object on the wire (an array, `null` or
/// any member is refused).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Acceptance;

impl Serialize for Acceptance {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_map(Some(0))?.end()
    }
}

impl<'de> Deserialize<'de> for Acceptance {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Empty;
        impl<'de> Visitor<'de> for Empty {
            type Value = Acceptance;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("an empty object")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Acceptance, A::Error> {
                match map.next_key::<String>()? {
                    None => Ok(Acceptance),
                    Some(member) => Err(de::Error::unknown_field(&member, &[])),
                }
            }
        }
        deserializer.deserialize_map(Empty)
    }
}
