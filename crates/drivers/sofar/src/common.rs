//! Names every inverter shares, each with the unit its values are in.
//!
//! A profile maps its own registers onto these and scales into the unit, so the
//! archive can compare two installations without knowing either inverter. A
//! register no other inverter would have goes under `[[extra]]` instead.

/// Common names with a fixed spelling.
const EXACT: &[(&str, Option<&str>)] = &[
    ("grid_frequency", Some("Hz")),
    ("output_power", Some("kW")),
    ("output_reactive_power", Some("kvar")),
    ("output_apparent_power", Some("kVA")),
    // The meter at the point of common coupling: what the house trades with
    // the grid, as opposed to what the inverter puts out.
    ("meter_power", Some("kW")),
    ("meter_reactive_power", Some("kvar")),
    ("meter_apparent_power", Some("kVA")),
    ("load_power", Some("kW")),
    ("energy_today", Some("kWh")),
    ("energy_total", Some("kWh")),
    ("load_energy_today", Some("kWh")),
    ("load_energy_total", Some("kWh")),
    ("import_energy_today", Some("kWh")),
    ("import_energy_total", Some("kWh")),
    ("export_energy_today", Some("kWh")),
    ("export_energy_total", Some("kWh")),
    ("battery_charge_energy_today", Some("kWh")),
    ("battery_charge_energy_total", Some("kWh")),
    ("battery_discharge_energy_today", Some("kWh")),
    ("battery_discharge_energy_total", Some("kWh")),
    ("generation_time_today", Some("min")),
    ("generation_time_total", Some("min")),
    ("running_time_total", Some("min")),
    ("insulation_resistance", Some("kΩ")),
    ("clock_year", None),
    ("clock_month", None),
    ("clock_day", None),
    ("clock_hour", None),
    ("clock_minute", None),
    ("clock_second", None),
];

/// Per-phase names, spelled `<stem>_l1` to `<stem>_l3`.
const PHASE: &[(&str, Option<&str>)] = &[
    ("grid_voltage", Some("V")),
    ("output_current", Some("A")),
    ("output_power", Some("kW")),
    ("output_reactive_power", Some("kvar")),
    ("output_power_factor", None),
    ("meter_current", Some("A")),
    ("meter_power", Some("kW")),
    ("meter_reactive_power", Some("kvar")),
    ("meter_power_factor", None),
];

/// Per-string names, spelled `pv<n>_<quantity>`.
const PV: &[(&str, Option<&str>)] = &[
    ("voltage", Some("V")),
    ("current", Some("A")),
    ("power", Some("kW")),
];

/// Numbered sensors, spelled `<stem><n>`.
const SENSOR: &[(&str, Option<&str>)] = &[
    ("temperature_ambient", Some("°C")),
    ("temperature_heatsink", Some("°C")),
    ("temperature_module", Some("°C")),
];

/// The unit a common name is in: `None` if `name` is not a common name,
/// `Some(None)` if it is one and carries no unit.
#[must_use]
pub fn canonical_unit(name: &str) -> Option<Option<&'static str>> {
    let lookup = |table: &[(&'static str, Option<&'static str>)], key: &str| {
        table.iter().find(|(k, _)| *k == key).map(|(_, unit)| *unit)
    };
    if let Some(unit) = lookup(EXACT, name) {
        return Some(unit);
    }
    if let Some(stem) = ["_l1", "_l2", "_l3"]
        .iter()
        .find_map(|phase| name.strip_suffix(phase))
    {
        return lookup(PHASE, stem);
    }
    if let Some((n, quantity)) = name
        .strip_prefix("pv")
        .and_then(|rest| rest.split_once('_'))
        && ordinal(n)
    {
        return lookup(PV, quantity);
    }
    SENSOR.iter().find_map(|(stem, unit)| {
        name.strip_prefix(stem)
            .is_some_and(ordinal)
            .then_some(*unit)
    })
}

/// A 1-based index as a name writes it: digits, no leading zero.
fn ordinal(s: &str) -> bool {
    !s.is_empty() && !s.starts_with('0') && s.bytes().all(|b| b.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_family_of_name_resolves_to_its_unit() {
        assert_eq!(canonical_unit("energy_today"), Some(Some("kWh")));
        assert_eq!(canonical_unit("clock_year"), Some(None));
        assert_eq!(canonical_unit("grid_voltage_l3"), Some(Some("V")));
        assert_eq!(canonical_unit("meter_power_factor_l1"), Some(None));
        assert_eq!(canonical_unit("pv12_power"), Some(Some("kW")));
        assert_eq!(canonical_unit("temperature_heatsink6"), Some(Some("°C")));
    }

    #[test]
    fn near_misses_are_not_common_names() {
        // A typo must land as an unknown name, where the profile check can
        // reject it, not as a lookalike that silently takes a unit.
        for name in [
            "pv_power",
            "pv0_power",
            "pv01_power",
            "pv1_energy",
            "grid_voltage",
            "grid_voltage_l4",
            "temperature_module",
            "temperature_module0",
            "energy_yesterday",
            "fault1",
        ] {
            assert_eq!(canonical_unit(name), None, "{name}");
        }
    }
}
