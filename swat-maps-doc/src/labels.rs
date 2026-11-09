//! One label per place: the de-duplication a point layer's name labels
//! go through before they reach the map.
//!
//! A table with one row per (thing × place) -- a station once per
//! operator, a shop once per brand -- names the same place several
//! times, a few hundred metres apart. Labelled row by row the map writes
//! "Shimbashi" four times. [`one_per_place`] keeps one label for every
//! run of equal texts that sit within [`SAME_PLACE_M`] of each other, at
//! the row that weighs most (the largest drawn point, typically), so the
//! label hangs off the dot the eye goes to first.
//!
//! Equal texts further apart than that are different places with the same
//! name and keep a label each.

use std::collections::HashMap;

/// Two labels with the same text this close (metres) name one place.
pub const SAME_PLACE_M: f64 = 1000.0;

/// One candidate label: its text, where it hangs, and how much it weighs
/// (the heaviest of a place's rows keeps the label).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LabelPlace<'a> {
    pub text: &'a str,
    pub lon: f64,
    pub lat: f64,
    pub weight: f64,
}

/// Which of `labels` to draw: `true` for the heaviest of each place's
/// equal texts (ties: the earliest), `false` for the rest. Texts compare
/// trimmed and case-sensitively; a non-finite weight counts as the
/// lightest, except `f64::INFINITY`, which always wins (a pinned row).
pub fn one_per_place(labels: &[LabelPlace<'_>]) -> Vec<bool> {
    let weight = |w: f64| if w.is_nan() { f64::NEG_INFINITY } else { w };
    let mut order: Vec<usize> = (0..labels.len()).collect();
    order.sort_by(|&a, &b| weight(labels[b].weight).total_cmp(&weight(labels[a].weight)).then(a.cmp(&b)));
    let mut kept: HashMap<&str, Vec<(f64, f64)>> = HashMap::new();
    let mut keep = vec![false; labels.len()];
    for i in order {
        let l = &labels[i];
        let places = kept.entry(l.text.trim()).or_default();
        if places.iter().any(|&(lon, lat)| distance_m(lon, lat, l.lon, l.lat) <= SAME_PLACE_M) {
            continue;
        }
        places.push((l.lon, l.lat));
        keep[i] = true;
    }
    keep
}

/// Equirectangular distance in metres: exact enough at the scale of one
/// place.
fn distance_m(lon_a: f64, lat_a: f64, lon_b: f64, lat_b: f64) -> f64 {
    const M_PER_DEG: f64 = 111_320.0;
    let mid = ((lat_a + lat_b) * 0.5).to_radians().cos();
    let dx = (lon_a - lon_b) * M_PER_DEG * mid;
    let dy = (lat_a - lat_b) * M_PER_DEG;
    (dx * dx + dy * dy).sqrt()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn place(text: &str, lon: f64, lat: f64, weight: f64) -> LabelPlace<'_> {
        LabelPlace { text, lon, lat, weight }
    }

    #[test]
    fn a_station_listed_once_per_operator_keeps_one_label_at_its_busiest_row() {
        // Shimbashi as four operators publish it, 100-300 m apart.
        let rows = [
            place("Shimbashi", 139.758513, 35.667383, 204_567.0),
            place("Shimbashi", 139.75873, 35.666205, 463_256.0),
            place("Shimbashi", 139.758986, 35.664885, 61_689.0),
            place("Shimbashi", 139.759753, 35.665397, 58_257.0),
            place("Yurakucho", 139.7630, 35.6751, 1.0),
        ];
        assert_eq!(one_per_place(&rows), vec![false, true, false, false, true]);
    }

    #[test]
    fn the_same_name_in_two_far_apart_places_keeps_both_labels() {
        let rows = [place("Kasuga", 139.75, 35.70, 1.0), place("Kasuga", 139.60, 35.70, 1.0)];
        assert_eq!(one_per_place(&rows), vec![true, true]);
    }

    #[test]
    fn equal_weights_keep_the_first_row_and_different_texts_never_merge() {
        let rows = [place("A", 0.0, 0.0, 5.0), place("A", 0.0, 0.0001, 5.0), place("B", 0.0, 0.0, 1.0)];
        assert_eq!(one_per_place(&rows), vec![true, false, true]);
    }

    #[test]
    fn a_pinned_row_wins_its_place_over_a_heavier_one() {
        let rows = [place("A", 0.0, 0.0, 9.0), place("A", 0.0, 0.001, f64::INFINITY)];
        assert_eq!(one_per_place(&rows), vec![false, true]);
    }

    #[test]
    fn a_nan_weight_loses_to_any_number() {
        let rows = [place("A", 0.0, 0.0, f64::NAN), place("A", 0.0, 0.0, -3.0)];
        assert_eq!(one_per_place(&rows), vec![false, true]);
    }
}
