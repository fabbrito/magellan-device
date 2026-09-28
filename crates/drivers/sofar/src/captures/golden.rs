//! The shipped profile against replies captured off this installation.
//!
//! Every assertion is something the wire itself vouches for: a relation that holds exactly on
//! this unit, or a register its own mask marks invalid. A wrong exponent, a word order swapped,
//! an address one off — each breaks one. Values come from the fixture manifest, which
//! `codec_vectors` proves equal to the captured hex.

use crate::decode::{NamedValues, Value};
use crate::profile::Profile;
use serde::Deserialize;

const MANIFEST: &str = include_str!("fixtures/manifest.toml");

type BoxError = Box<dyn std::error::Error>;

#[derive(Deserialize)]
struct Manifest {
    vector: Vec<Vector>,
}

#[derive(Deserialize)]
struct Vector {
    name: String,
    range: Option<String>,
    values: Option<Vec<u16>>,
    source_ts: String,
}

fn profile() -> Result<Profile, BoxError> {
    Ok(crate::builtin("sofar-g3")?)
}

fn manifest() -> Result<Manifest, BoxError> {
    Ok(toml::from_str(MANIFEST)?)
}

/// One decoded range, with the profile that named it.
struct Read<'a> {
    profile: &'a Profile,
    values: NamedValues,
}

impl Read<'_> {
    /// A reading in its physical unit.
    ///
    /// The driver never forms this number: the integer and its exponent travel to the cloud
    /// separately, and multiplying them is the cloud's business. A relation between three
    /// registers can only be checked in the physical unit, so the test does here what the
    /// production path deliberately does not.
    fn num(&self, name: &str) -> f64 {
        let exponent = self
            .profile
            .entries()
            .iter()
            .find(|e| e.name == name)
            .unwrap_or_else(|| panic!("{name} is not in the profile"))
            .exponent;
        match self.values.get(name) {
            Some(Value::Int(v)) => *v as f64 * 10f64.powi(i32::from(exponent)),
            other => panic!("{name}: expected a number, got {other:?}"),
        }
    }

    fn text(&self, name: &str) -> &str {
        match self.values.get(name) {
            Some(Value::Text(v)) => v,
            other => panic!("{name}: expected text, got {other:?}"),
        }
    }

    fn get(&self, name: &str) -> Option<&Value> {
        self.values.get(name)
    }
}

/// Decode a captured range through the profile.
fn decode<'a>(profile: &'a Profile, name: &str) -> Result<(Read<'a>, Vector), BoxError> {
    let v = manifest()?
        .vector
        .into_iter()
        .find(|v| v.name == name)
        .ok_or_else(|| format!("no vector {name}"))?;
    let values = v.values.clone().ok_or("not a reply vector")?;
    let addr = u16::from_str_radix(name.rsplit('-').next().unwrap_or(name), 16)?;
    let decoded = profile.decode(addr, &values);
    // A captured reply is real: a bound that rejects it is wrong.
    if !decoded.implausible.is_empty() {
        return Err(format!("{name} rejects {:?}", decoded.implausible).into());
    }
    Ok((
        Read {
            profile,
            values: decoded.values,
        },
        v,
    ))
}

/// Equal to the register's own resolution, which is all it can promise.
fn assert_close(got: f64, want: f64, what: &str) {
    assert!((got - want).abs() < 1e-9, "{what}: {got}, want {want}");
}

/// V × I in kW against a power register, within what the three registers' own resolutions allow:
/// voltage ±0.05 V, current ±0.005 A, power ±0.005 kW.
fn assert_ohms_law(v: f64, i: f64, p_kw: f64, what: &str) {
    let tolerance = (v * 0.005 + i * 0.05) / 1000.0 + 0.005;
    let got = v * i / 1000.0;
    assert!(
        (got - p_kw).abs() <= tolerance,
        "{what}: {v} V × {i} A = {got:.4} kW, register says {p_kw} kW"
    );
}

#[test]
fn a_scaled_register_reaches_the_reading_as_its_raw_integer() {
    // The one thing this port changed. 2672 is what the inverter reported; 267.2 V is what it
    // means, and the exponent is where that lives. No float is formed on the way, so the integer
    // the cloud stores is the integer the wire carried.
    let profile = profile().unwrap();
    let (r, _) = decode(&profile, "tcp-range-0580").unwrap();
    assert_eq!(r.get("pv1_voltage"), Some(&Value::Int(2672)));
    let entry = profile
        .entries()
        .iter()
        .find(|e| e.name == "pv1_voltage")
        .expect("the entry is there");
    assert_eq!(entry.exponent, -1);
    assert_close(r.num("pv1_voltage"), 267.2, "pv1_voltage");
}

#[test]
fn the_read_plan_is_the_one_that_was_captured() {
    // The fixtures are what this plan answered on the wire. A range edited in the profile but
    // never captured is a guess.
    let profile = profile().unwrap();
    let manifest = manifest().unwrap();
    for range in profile.ranges() {
        let captured =
            format!("{:#06X}-{:#06X}", range.addr, range.addr + range.qty - 1).replace("0X", "0x");
        assert!(
            manifest
                .vector
                .iter()
                .any(|v| v.values.is_some() && v.range.as_deref() == Some(captured.as_str())),
            "range {} ({captured}) has no captured reply",
            range.name
        );
    }
}

#[test]
fn no_captured_reply_breaks_a_bound() {
    let profile = profile().unwrap();
    let mut checked = 0;
    for v in manifest().unwrap().vector {
        if v.values.is_some() {
            decode(&profile, &v.name).unwrap();
            checked += 1;
        }
    }
    assert!(checked > 0, "no reply vector decoded");
}

#[test]
fn pv_strings_obey_ohms_law() {
    let profile = profile().unwrap();
    let (r, _) = decode(&profile, "tcp-range-0580").unwrap();
    for n in 1..=2 {
        assert_ohms_law(
            r.num(&format!("pv{n}_voltage")),
            r.num(&format!("pv{n}_current")),
            r.num(&format!("pv{n}_power")),
            &format!("pv{n}"),
        );
    }
}

#[test]
fn grid_output_obeys_ohms_law_and_drops_what_the_mask_rules_out() {
    let profile = profile().unwrap();
    let (r, _) = decode(&profile, "tcp-range-0480").unwrap();
    assert_close(r.num("grid_frequency"), 60.02, "grid_frequency");
    // Single-phase: the per-phase output power reads 0 and its mask bit is clear, so the total
    // carries the phase.
    assert_ohms_law(
        r.num("grid_voltage_l1"),
        r.num("output_current_l1"),
        r.num("output_power"),
        "grid output",
    );
    for absent in ["output_power_l1", "grid_voltage_l2", "grid_voltage_l3"] {
        assert!(r.get(absent).is_none(), "{absent} is masked invalid");
    }
}

#[test]
fn system_info_agrees_with_the_capture_clock() {
    let profile = profile().unwrap();
    let (r, v) = decode(&profile, "tcp-range-0400").unwrap();
    // The inverter's own calendar, read at the capture's timestamp. A one-off address lands on a
    // neighbouring register and breaks this.
    let (date, time) = v.source_ts.split_once('T').unwrap();
    let date: Vec<f64> = date.split('-').map(|p| p.parse().unwrap()).collect();
    assert_close(r.num("clock_year") + 2000.0, date[0], "clock_year");
    assert_close(r.num("clock_month"), date[1], "clock_month");
    assert_close(r.num("clock_day"), date[2], "clock_day");

    // Grid-connected minutes today cannot outrun the UTC day, and powered minutes include
    // connected ones — both break if a U32's words swap.
    let clock: Vec<f64> = time
        .split(':')
        .take(2)
        .map(|p| p.parse().unwrap())
        .collect();
    assert!(r.num("generation_time_today") <= clock[0] * 60.0 + clock[1]);
    assert!(r.num("running_time_total") >= r.num("generation_time_total"));
    assert!(
        r.num("generation_time_total") > 10_000.0,
        "a U32 read as its high word"
    );

    assert_eq!(
        r.get("state"),
        Some(&Value::Int(2)),
        "grid-connected at midday"
    );
    // An unpopulated sensor reads a plausible 120 °C; only the mask says so.
    assert!(r.get("temperature_heatsink2").is_none());
}

#[test]
fn energy_counters_agree_and_absent_meters_are_dropped() {
    let profile = profile().unwrap();
    let (r, _) = decode(&profile, "tcp-range-0680").unwrap();
    assert_close(r.num("energy_today"), 12.27, "energy_today");
    assert_close(r.num("energy_total"), 1413.9, "energy_total");
    assert!(r.num("energy_today") <= r.num("energy_total"));
    // No meter, no battery on this installation: masked, never zeros.
    for absent in [
        "load_energy_today",
        "import_energy_total",
        "battery_charge_energy_today",
    ] {
        assert!(r.get(absent).is_none(), "{absent} is masked invalid");
    }
}

#[test]
fn general_partition_decodes_version_and_chip_codes() {
    let profile = profile().unwrap();
    let (r, _) = decode(&profile, "tcp-range-0040").unwrap();
    assert_eq!(r.text("protocol_version"), "1.23");
    assert_eq!(r.text("comm_mcu_code"), "0G");
    assert_eq!(r.text("ctrl1_mcu_code"), "5T");
    assert_eq!(r.text("ctrl2_mcu_code"), "0M");
}
