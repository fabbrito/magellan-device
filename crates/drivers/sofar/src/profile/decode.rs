//! Register words to named values.

use crate::profile::{Entry, Kind, Profile};

/// A decoded value. Every measurement is an integer; its entry's exponent says what it means.
/// A float is never formed, so nothing here can round a register on the way to the wire.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value {
    Int(i64),
    /// A version or a chip code. Identity, not a measurement, so it is no metric.
    Text(String),
}

impl Value {
    /// The number, if it is one.
    #[must_use]
    pub const fn as_int(&self) -> Option<i64> {
        match self {
            Self::Int(v) => Some(*v),
            Self::Text(_) => None,
        }
    }
}

/// One named value. The name is owned: a value outlives the read that made it,
/// and a handful of small clones per sweep costs nothing next to the wire.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NamedValue {
    pub name: String,
    pub value: Value,
}

/// A read's values by name, in address order. Serialises as one JSON object.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct NamedValues(Vec<NamedValue>);

impl NamedValues {
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&Value> {
        self.0.iter().find(|r| r.name == name).map(|r| &r.value)
    }

    pub fn iter(&self) -> impl Iterator<Item = &NamedValue> {
        self.0.iter()
    }

    #[must_use]
    pub const fn len(&self) -> usize {
        self.0.len()
    }

    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl FromIterator<NamedValue> for NamedValues {
    fn from_iter<I: IntoIterator<Item = NamedValue>>(iter: I) -> Self {
        Self(iter.into_iter().collect())
    }
}

/// A read, named: what passed its entry's bounds, and what did not.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Decoded {
    pub values: NamedValues,
    /// What the profile's bounds say the source cannot physically report. Never a metric value.
    pub implausible: NamedValues,
}

impl Profile {
    /// Name the registers of one read, `values` starting at `addr`.
    ///
    /// Drops what the read cannot vouch for: an entry its partition mask marks
    /// invalid (an unpopulated sensor reads a plausible 120), and an entry the
    /// reply stopped short of. Sets apart what falls outside its bounds.
    #[must_use]
    pub fn decode(&self, addr: u16, values: &[u16]) -> Decoded {
        let (plausible, implausible): (Vec<_>, Vec<_>) = self
            .entries()
            .iter()
            .filter_map(|entry| {
                let at = usize::from(entry.addr.checked_sub(addr)?);
                let words = values.get(at..at + usize::from(entry.kind.width()))?;
                let valid = (entry.addr..entry.addr + entry.kind.width())
                    .all(|reg| self.valid(addr, values, reg));
                valid.then(|| (entry, value(entry, words)))
            })
            .map(|(entry, value)| {
                let within_bounds = within(entry, &value);
                let named = NamedValue {
                    name: entry.name.clone(),
                    value,
                };
                (named, within_bounds)
            })
            .partition(|(_, within_bounds)| *within_bounds);
        Decoded {
            values: plausible.into_iter().map(|(v, _)| v).collect(),
            implausible: implausible.into_iter().map(|(v, _)| v).collect(),
        }
    }
}

/// Inside the entry's bounds, or it has none. Compared raw against raw: the profile's bounds were
/// converted to register units at parse, so nothing here has to know the exponent.
fn within(entry: &Entry, value: &Value) -> bool {
    value.as_int().is_none_or(|v| {
        entry.min.is_none_or(|min| v >= min) && entry.max.is_none_or(|max| v <= max)
    })
}

/// One entry's words as its value. `words` is exactly the entry's width.
fn value(entry: &Entry, words: &[u16]) -> Value {
    let word = |i: usize| words.get(i).copied().unwrap_or_default();
    let raw: i64 = match entry.kind {
        Kind::U16 => i64::from(word(0)),
        Kind::I16 => i64::from(word(0).cast_signed()),
        Kind::U32 => i64::from(u32::from(word(0)) << 16 | u32::from(word(1))),
        Kind::I32 => i64::from((u32::from(word(0)) << 16 | u32::from(word(1))).cast_signed()),
        Kind::Bcd16 => {
            let [major, minor] = word(0).to_be_bytes();
            return Value::Text(format!(
                "{}.{minor:02x}",
                u32::from(major >> 4) * 10 + u32::from(major & 0xF)
            ));
        }
        Kind::Ascii => {
            let text: String = word(0)
                .to_be_bytes()
                .iter()
                .filter(|b| b.is_ascii_graphic() || **b == b' ')
                .map(|&b| char::from(b))
                .collect();
            return Value::Text(text.trim().to_owned());
        }
    };
    // The register is the value. Its entry's exponent says what it means, and travels with the
    // metric rather than being multiplied in here.
    Value::Int(raw)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile::tests::checked;

    /// Mask words for the minimal profile's read at 0x0580: bit 4 is `pv1_voltage`, bit
    /// 5 `vendor_code`.
    const BOTH_VALID: [u16; 4] = [0, 0, 0, 0b11_0000];

    fn names(decoded: &Decoded) -> Vec<String> {
        decoded.values.iter().map(|r| r.name.clone()).collect()
    }

    #[test]
    fn an_entry_past_the_end_of_the_reply_is_dropped() {
        let profile = checked(|_| ()).unwrap();
        let mut values = BOTH_VALID.to_vec();
        values.push(2545);
        let decoded = profile.decode(0x0580, &values);
        assert_eq!(decoded.values.get("pv1_voltage"), Some(&Value::Int(2545)));
        assert_eq!(names(&decoded), ["pv1_voltage"]);
    }

    #[test]
    fn the_mask_decides_what_is_valid() {
        let profile = checked(|_| ()).unwrap();
        let values = [0, 0, 0, 0b10_0000, 2545, 7];
        assert_eq!(names(&profile.decode(0x0580, &values)), ["vendor_code"]);
    }

    #[test]
    fn a_read_that_misses_its_mask_vouches_for_nothing() {
        let profile = checked(|_| ()).unwrap();
        assert!(profile.decode(0x0584, &[2545, 7]).values.is_empty());
    }

    #[test]
    fn a_register_no_mask_covers_is_valid() {
        let profile = checked(|raw| raw.mask.clear()).unwrap();
        let decoded = profile.decode(0x0580, &[0, 0, 0, 0, 2545, 7]);
        assert_eq!(names(&decoded), ["pv1_voltage", "vendor_code"]);
    }

    #[test]
    fn a_value_outside_its_bounds_is_implausible_not_dropped() {
        let profile = checked(|raw| {
            let field = raw.field.first_mut().expect("minimal has a field");
            field.min = Some(0.0);
            field.max = Some(600.0);
        })
        .unwrap();
        let mut values = BOTH_VALID.to_vec();
        values.extend([6001, 7]);
        let decoded = profile.decode(0x0580, &values);
        // The unbounded neighbour is kept whatever it reads.
        assert_eq!(names(&decoded), ["vendor_code"]);
        assert_eq!(
            decoded.implausible.get("pv1_voltage"),
            Some(&Value::Int(6001))
        );

        values[4] = 6000;
        let decoded = profile.decode(0x0580, &values);
        assert_eq!(names(&decoded), ["pv1_voltage", "vendor_code"]);
        assert!(decoded.implausible.is_empty());
    }

    #[test]
    fn a_scaled_register_travels_as_the_integer_it_already_is() {
        let profile = checked(|_| ()).unwrap();
        let mut values = BOTH_VALID.to_vec();
        values.extend([2545, 7]);
        let decoded = profile.decode(0x0580, &values);
        // The same register decoded to the float 254.5 before the port. Now the integer travels
        // untouched and the exponent beside it says what it means: 2545 x 10^-1. No multiply, so
        // nothing to round, and the value the cloud stores is the value the inverter reported.
        assert_eq!(decoded.values.get("pv1_voltage"), Some(&Value::Int(2545)));
        let entry = profile
            .entries()
            .iter()
            .find(|e| e.name == "pv1_voltage")
            .expect("the entry is there");
        assert_eq!(entry.exponent, -1);
    }
}
