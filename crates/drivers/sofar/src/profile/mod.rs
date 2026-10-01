//! Profile schema, parsed from TOML and checked once, at load.

pub(crate) mod common;
pub(crate) mod decode;
mod error;

pub(crate) use crate::profile::error::ProfileError;

use contract::limits::{EXPONENT_MAX, EXPONENT_MIN};
use serde::Deserialize;

use crate::profile::common::{CommonName, canonical_unit, count_of};

/// Ceiling on a single read: Modbus caps FC3 here, and the short shape counts
/// its body in one byte, so nothing larger can come back whole.
const QTY_MAX: u16 = 125;
/// A partition mask is a U64: four registers, one bit per address above it.
const MASK_WIDTH: u16 = 4;
const MASK_SPAN: u32 = 64;

/// One inverter family: its read plan and what every register in it means.
#[derive(Debug)]
pub struct Profile {
    name: String,
    source: String,
    ranges: Vec<Range>,
    masks: Vec<u16>,
    entries: Vec<Entry>,
}

/// One contiguous read. Order in the file is the order polled each sweep.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Range {
    /// Labels the range in the log; unique within a profile.
    pub name: String,
    pub addr: u16,
    pub qty: u16,
}

impl Range {
    fn contains(&self, addr: u16, width: u16) -> bool {
        let (start, end) = (
            u32::from(self.addr),
            u32::from(self.addr) + u32::from(self.qty),
        );
        u32::from(addr) >= start && u32::from(addr) + u32::from(width) <= end
    }
}

/// How a register's words become a value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    U16,
    I16,
    /// Two registers, high word first.
    U32,
    I32,
    /// A version, `0x0123` = 1.23.
    Bcd16,
    /// Two characters, high byte first.
    Ascii,
}

impl Kind {
    /// Registers this kind spans.
    #[must_use]
    pub const fn width(self) -> u16 {
        match self {
            Self::U32 | Self::I32 => 2,
            Self::U16 | Self::I16 | Self::Bcd16 | Self::Ascii => 1,
        }
    }

    pub(crate) const fn numeric(self) -> bool {
        !matches!(self, Self::Bcd16 | Self::Ascii)
    }
}

/// A named register.
#[derive(Debug)]
pub struct Entry {
    pub addr: u16,
    pub name: String,
    pub kind: Kind,
    /// The physical value is `raw × 10^exponent`. The contract carries this beside the metric,
    /// so the register travels as the integer it already is and no float is ever formed.
    pub exponent: i8,
    pub unit: Option<String>,
    /// Plausible bounds as raw register values. A value outside them is garbage on the wire, not
    /// a measurement: decode rejects it. The profile writes them in the physical unit and they
    /// are converted once, here, so decode only ever compares integers.
    pub min: Option<i64>,
    pub max: Option<i64>,
    /// Listed under `[[field]]`: a name every inverter shares.
    pub common: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Raw {
    profile: Meta,
    range: Vec<Range>,
    #[serde(default)]
    mask: Vec<RawMask>,
    #[serde(default)]
    field: Vec<RawEntry>,
    #[serde(default)]
    extra: Vec<RawEntry>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Meta {
    name: String,
    source: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawMask {
    addr: u16,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawEntry {
    addr: u16,
    name: String,
    #[serde(rename = "type")]
    kind: Kind,
    #[serde(default)]
    exponent: i8,
    unit: Option<String>,
    min: Option<f64>,
    max: Option<f64>,
}

macro_rules! require {
    ($cond:expr, $($why:tt)+) => {
        if !$cond {
            return Err(ProfileError::Invalid(format!($($why)+)));
        }
    };
}

/// A bound written in the physical unit, as a raw register value: `physical × 10^-exponent`.
///
/// Rounded outward — down for a minimum, up for a maximum — so a bound that is not a whole number
/// of raw units never rejects a reading the profile meant to allow.
fn raw_bound(physical: Option<f64>, exponent: i8, up: bool) -> Option<i64> {
    let scaled = physical? * 10f64.powi(-i32::from(exponent));
    Some(if up { scaled.ceil() } else { scaled.floor() } as i64)
}

impl Entry {
    /// Check what has to be right before the bounds can be converted, then convert them.
    fn from_raw(e: RawEntry, common: bool) -> Result<Self, ProfileError> {
        for bound in [e.min, e.max].into_iter().flatten() {
            require!(bound.is_finite(), "{:?} has bound {bound}", e.name);
        }
        require!(
            (EXPONENT_MIN..=EXPONENT_MAX).contains(&e.exponent),
            "{:?} has exponent {}, outside the contract's {EXPONENT_MIN}..={EXPONENT_MAX}",
            e.name,
            e.exponent
        );
        Ok(Self {
            min: raw_bound(e.min, e.exponent, false),
            max: raw_bound(e.max, e.exponent, true),
            addr: e.addr,
            name: e.name,
            kind: e.kind,
            exponent: e.exponent,
            unit: e.unit,
            common,
        })
    }
}

impl Profile {
    /// Parse and check a profile.
    ///
    /// # Errors
    ///
    /// [`ProfileError::Parse`] on bad TOML or an unknown key; [`ProfileError::Invalid`] on
    /// anything decode would get wrong quietly: a read Modbus cannot answer,
    /// an entry outside every read, two entries on one register, a common name
    /// misspelled or in the wrong unit.
    pub fn parse(text: &str) -> Result<Self, ProfileError> {
        let raw: Raw = toml::from_str(text).map_err(ProfileError::Parse)?;
        let mut entries: Vec<Entry> = raw
            .field
            .into_iter()
            .map(|e| (e, true))
            .chain(raw.extra.into_iter().map(|e| (e, false)))
            .map(|(e, common)| Entry::from_raw(e, common))
            .collect::<Result<Vec<_>, ProfileError>>()?;
        // Wire order, so a decode reads in the order the registers arrive.
        entries.sort_by_key(|e| e.addr);
        let profile = Self {
            name: raw.profile.name,
            source: raw.profile.source,
            ranges: raw.range,
            masks: raw.mask.into_iter().map(|m| m.addr).collect(),
            entries,
        };
        profile.check()?;
        Ok(profile)
    }

    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Where the profile came from.
    #[must_use]
    pub fn source(&self) -> &str {
        &self.source
    }

    /// The read plan, in polling order.
    #[must_use]
    pub fn ranges(&self) -> &[Range] {
        &self.ranges
    }

    /// Every named register, in address order.
    #[must_use]
    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    /// Addresses of the partition masks.
    #[must_use]
    pub fn masks(&self) -> &[u16] {
        &self.masks
    }

    fn check(&self) -> Result<(), ProfileError> {
        require!(!self.name.is_empty(), "profile has no name");
        self.check_ranges()?;
        self.check_claims()?;
        self.check_masks_read()?;
        self.check_entries()
    }

    fn check_ranges(&self) -> Result<(), ProfileError> {
        require!(!self.ranges.is_empty(), "no ranges to read");
        for (i, range) in self.ranges.iter().enumerate() {
            require!(!range.name.is_empty(), "a range has an empty name");
            require!(
                (1..=QTY_MAX).contains(&range.qty),
                "range {:?} asks for {} registers, outside 1..={QTY_MAX}",
                range.name,
                range.qty
            );
            require!(
                range.addr.checked_add(range.qty).is_some(),
                "range {:?} runs past the end of the address space",
                range.name
            );
            for other in self.ranges.iter().skip(i + 1) {
                require!(
                    other.name != range.name,
                    "duplicate range name {:?} — names label the log",
                    range.name
                );
                // One register, one range: otherwise a sweep reads it twice
                // and the log carries two values for one name.
                require!(
                    !other.contains(range.addr, 1) && !range.contains(other.addr, 1),
                    "ranges {:?} and {:?} overlap",
                    range.name,
                    other.name
                );
            }
        }
        Ok(())
    }

    /// Every register a mask or entry spans is read, and claimed once.
    fn check_claims(&self) -> Result<(), ProfileError> {
        let spans = self.masks.iter().map(|&m| (m, MASK_WIDTH, "a mask")).chain(
            self.entries
                .iter()
                .map(|e| (e.addr, e.kind.width(), "an entry")),
        );
        let mut claimed: Vec<u16> = Vec::new();
        for (addr, width, what) in spans {
            require!(
                self.ranges.iter().any(|r| r.contains(addr, width)),
                "{what} at {addr:#06x} is not inside any range — it would never be read"
            );
            for reg in addr..addr.saturating_add(width) {
                require!(
                    !claimed.contains(&reg),
                    "register {reg:#06x} is claimed twice"
                );
                claimed.push(reg);
            }
        }
        Ok(())
    }

    /// Every masked entry shares a read with its mask: decode trusts no value
    /// its read cannot vouch for, so an entry read without one never appears.
    fn check_masks_read(&self) -> Result<(), ProfileError> {
        for entry in &self.entries {
            let Some(mask) = self.mask_of(entry.addr) else {
                continue;
            };
            require!(
                self.ranges
                    .iter()
                    .any(|r| r.contains(entry.addr, entry.kind.width())
                        && r.contains(mask, MASK_WIDTH)),
                "{:?} is read without its mask at {mask:#06x}",
                entry.name
            );
        }
        Ok(())
    }

    /// Names are unique, common ones spelled and scaled right.
    fn check_entries(&self) -> Result<(), ProfileError> {
        for (i, entry) in self.entries.iter().enumerate() {
            require!(
                !entry.name.is_empty(),
                "entry at {:#06x} has no name",
                entry.addr
            );
            require!(
                !self
                    .entries
                    .iter()
                    .skip(i + 1)
                    .any(|e| e.name == entry.name),
                "duplicate name {:?}",
                entry.name
            );
            match (entry.common, canonical_unit(&entry.name)) {
                (true, None) => {
                    return Err(ProfileError::Invalid(format!(
                        "{:?} is not a common name — misspelled, or it belongs under [[extra]]",
                        entry.name
                    )));
                }
                (true, Some(CommonName { unit })) => require!(
                    entry.unit.as_deref() == unit,
                    "{:?} is in {:?}, but its common unit is {:?} — scale into it",
                    entry.name,
                    entry.unit,
                    unit
                ),
                (false, Some(_)) => {
                    return Err(ProfileError::Invalid(format!(
                        "{:?} is a common name — list it under [[field]]",
                        entry.name
                    )));
                }
                (false, None) => require!(
                    count_of(&entry.name).is_none(),
                    "{:?} is named like a counter, and only a common name may be one — rename it, \
                     or add it to the common vocabulary",
                    entry.name
                ),
            }
            if entry.exponent != 0 {
                require!(
                    entry.kind.numeric(),
                    "{:?} scales a {:?}, which is not a number",
                    entry.name,
                    entry.kind
                );
            }
            if entry.min.is_some() || entry.max.is_some() {
                require!(
                    entry.kind.numeric(),
                    "{:?} bounds a {:?}, which is not a number",
                    entry.name,
                    entry.kind
                );
            }
            if let (Some(min), Some(max)) = (entry.min, entry.max)
                && min > max
            {
                return Err(ProfileError::Invalid(format!(
                    "{:?} has min {min} above max {max}",
                    entry.name
                )));
            }
        }
        Ok(())
    }

    /// The mask whose bits cover `reg`, if any.
    fn mask_of(&self, reg: u16) -> Option<u16> {
        self.masks
            .iter()
            .copied()
            .find(|&m| m <= reg && u32::from(reg) < u32::from(m) + MASK_SPAN)
    }

    /// Is `reg` valid by the mask covering it? `values` starts at `base`. A
    /// register no mask covers is valid: no mask, no claim. One whose mask is
    /// not in `values` is not: nothing vouches for it.
    pub(crate) fn valid(&self, base: u16, values: &[u16], reg: u16) -> bool {
        let Some(mask) = self.mask_of(reg) else {
            return true;
        };
        let Some(words) = mask
            .checked_sub(base)
            .and_then(|at| values.get(usize::from(at)..usize::from(at) + usize::from(MASK_WIDTH)))
        else {
            return false;
        };
        let bits = words
            .iter()
            .fold(0u64, |acc, &w| (acc << 16) | u64::from(w));
        bits >> (reg - mask) & 1 == 1
    }
}

/// Profiles shipped in the binary, by name. Another inverter family is another file.
const BUILTIN: &[(&str, &str)] = &[("sofar-g3", include_str!("../../profiles/sofar-g3.toml"))];

/// Load a shipped profile.
///
/// # Errors
///
/// [`ProfileError::Unknown`] if no profile has that name; otherwise whatever [`Profile::parse`]
/// rejects.
pub(crate) fn builtin(name: &str) -> Result<Profile, ProfileError> {
    let (_, text) = BUILTIN
        .iter()
        .find(|(n, _)| *n == name)
        .ok_or_else(|| ProfileError::Unknown(name.to_owned()))?;
    Profile::parse(text)
}

#[cfg(test)]
pub mod tests {
    use super::*;

    /// One masked range; the mask marks nothing valid until a test sets bits.
    pub const MINIMAL: &str = r#"
        [profile]
        name = "test"
        source = "test"

        [[range]]
        name = "pv-input"
        addr = 0x0580
        qty = 10

        [[mask]]
        addr = 0x0580

        [[field]]
        addr = 0x0584
        name = "pv1_voltage"
        type = "u16"
        exponent = -1
        unit = "V"

        [[extra]]
        addr = 0x0585
        name = "vendor_code"
        type = "u16"
    "#;

    fn rejects(text: &str, why: &str) {
        let err = Profile::parse(text).expect_err("should be rejected");
        assert!(err.to_string().contains(why), "{err}");
    }

    #[test]
    fn a_minimal_profile_parses() {
        let profile = Profile::parse(MINIMAL).expect("parses");
        assert_eq!(profile.ranges().len(), 1);
        assert_eq!(profile.entries().len(), 2);
    }

    #[test]
    fn an_entry_no_range_reads_is_rejected() {
        rejects(&MINIMAL.replace("0x0585", "0x0600"), "not inside any range");
        // Straddling the end counts: half a U32 is not a value.
        rejects(
            &MINIMAL.replace(
                "addr = 0x0585\n        name = \"vendor_code\"\n        type = \"u16\"",
                "addr = 0x0589\n        name = \"vendor_code\"\n        type = \"u32\"",
            ),
            "not inside any range",
        );
    }

    #[test]
    fn two_entries_on_one_register_are_rejected() {
        rejects(&MINIMAL.replace("0x0585", "0x0584"), "claimed twice");
        // A mask spans four registers; an entry inside it is a transcription slip.
        rejects(&MINIMAL.replace("0x0585", "0x0583"), "claimed twice");
    }

    #[test]
    fn a_misspelled_common_name_is_rejected() {
        rejects(
            &MINIMAL.replace("pv1_voltage", "pv1_volts"),
            "not a common name",
        );
    }

    #[test]
    fn a_common_name_in_the_wrong_unit_is_rejected() {
        rejects(
            &MINIMAL.replace("unit = \"V\"", "unit = \"mV\""),
            "common unit",
        );
    }

    #[test]
    fn a_common_name_filed_as_extra_is_rejected() {
        rejects(&MINIMAL.replace("vendor_code", "pv2_voltage"), "[[field]]");
    }

    #[test]
    fn a_read_modbus_cannot_answer_is_rejected() {
        rejects(&MINIMAL.replace("qty = 10", "qty = 126"), "outside 1..=125");
        rejects(&MINIMAL.replace("qty = 10", "qty = 0"), "outside 1..=125");
    }

    #[test]
    fn overlapping_ranges_are_rejected() {
        let text = format!("{MINIMAL}\n[[range]]\nname = \"again\"\naddr = 0x0589\nqty = 2\n");
        rejects(&text, "overlap");
    }

    #[test]
    fn an_entry_read_without_its_mask_is_rejected() {
        // The mask moves into a range of its own; the entries' read loses it.
        let text = MINIMAL.replace(
            "addr = 0x0580\n        qty = 10",
            "addr = 0x0584\n        qty = 6\n\n        [[range]]\n        name = \"head\"\n        addr = 0x0580\n        qty = 4",
        );
        rejects(&text, "without its mask");
    }

    #[test]
    fn bounds_that_cannot_hold_are_rejected() {
        let bounded = |bounds: &str| {
            MINIMAL.replace("unit = \"V\"", &format!("unit = \"V\"\n        {bounds}"))
        };
        assert!(Profile::parse(&bounded("min = 0.0\n        max = 1000.0")).is_ok());
        rejects(&bounded("min = 10.0\n        max = 1.0"), "above max");
        rejects(&bounded("max = nan"), "has bound");
        let text = MINIMAL.replace(
            "name = \"vendor_code\"\n        type = \"u16\"",
            "name = \"vendor_code\"\n        type = \"ascii\"\n        max = 1.0",
        );
        rejects(&text, "not a number");
    }

    #[test]
    fn a_bound_that_is_not_a_whole_register_rounds_outward() {
        // 0.05 V at exponent -1 is half a register. Rounding the minimum up would reject 0.1 V,
        // which the profile plainly means to allow; rounding the maximum down would do the same
        // at the other end.
        let text = MINIMAL.replace(
            "unit = \"V\"",
            "unit = \"V\"\n        min = 0.05\n        max = 600.05",
        );
        let profile = Profile::parse(&text).expect("parses");
        let entry = profile
            .entries()
            .iter()
            .find(|e| e.name == "pv1_voltage")
            .expect("the entry is there");
        assert_eq!((entry.min, entry.max), (Some(0), Some(6001)));
    }

    #[test]
    fn an_extra_named_like_a_counter_is_rejected() {
        // The suffix makes a counter, and only the common vocabulary vouches for one: an extra
        // would be charted as a running total on its name alone.
        for suffix in ["_total", "_today"] {
            let text = MINIMAL.replace("\"vendor_code\"", &format!("\"vendor_code{suffix}\""));
            rejects(&text, "counter");
        }
    }

    #[test]
    fn an_exponent_the_contract_cannot_carry_is_rejected() {
        let text = MINIMAL.replace("exponent = -1", "exponent = -13");
        rejects(&text, "outside the contract's");
    }

    #[test]
    fn an_unknown_key_is_rejected() {
        let text = MINIMAL.replace(
            "type = \"u16\"\n        exponent",
            "type = \"u16\"\n        factor = 1\n        exponent",
        );
        assert!(matches!(Profile::parse(&text), Err(ProfileError::Parse(_))));
    }
}
