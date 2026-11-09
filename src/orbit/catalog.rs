//! The drawn set: validated records de-duplicated, co-located stacks
//! collapsed onto one dot (R10), classified into a [`Group`], each holding
//! its SGP4 constants. Objects are sorted by NORAD id, so an index is stable
//! for the same input and [`Catalog::index_of`] is a binary search.

use super::omm::{to_elements, OmmRecord};
use std::collections::{HashMap, HashSet};

/// The colour group a dot is drawn in (spec §4.2), first matching rule wins.
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Group {
    Stations = 0,
    Starlink = 1,
    Gps = 2,
    Geo = 3,
    Other = 4,
    /// CelesTrak's `galileo` list (the EU's navigation constellation).
    Galileo = 5,
    /// CelesTrak's `glo-ops` list (Russia's GLONASS, operational).
    Glonass = 6,
    /// CelesTrak's `beidou` list (China's navigation constellation).
    Beidou = 7,
}

/// How many [`Group`]s there are: the length of every per-group array.
pub const GROUP_COUNT: usize = 8;

/// `0xRRGGBB` per [`Group`], indexed by `group as usize`.
pub const GROUP_COLOURS: [u32; GROUP_COUNT] = [0xFF5A36, 0x3D8BFF, 0xF5A623, 0xA974F0, 0x8C99A6, 0x2FA4D6, 0xD9533A, 0xB8A11E];

/// The NORAD id of the ISS (ZARYA), the object the demo opens on.
pub const ISS_NORAD: u32 = 25544;

/// The CelesTrak group lists an object may be in. `None` = that group's
/// list was never downloaded (so nothing is classified by it).
#[derive(Clone, Debug, Default)]
pub struct Membership {
    /// NORAD ids in CelesTrak's `stations` list.
    pub stations: Option<HashSet<u32>>,
    /// NORAD ids in CelesTrak's `gps-ops` list.
    pub gps: Option<HashSet<u32>>,
    /// NORAD ids in CelesTrak's `geo` list.
    pub geo: Option<HashSet<u32>>,
    /// NORAD ids in CelesTrak's `galileo` list.
    pub galileo: Option<HashSet<u32>>,
    /// NORAD ids in CelesTrak's `glo-ops` list.
    pub glonass: Option<HashSet<u32>>,
    /// NORAD ids in CelesTrak's `beidou` list.
    pub beidou: Option<HashSet<u32>>,
}

/// One drawn object.
pub struct SatObject {
    /// The validated elements it was built from.
    pub record: OmmRecord,
    /// The group it is coloured and toggled by.
    pub group: Group,
    /// Objects with the very same elements (docked modules, visiting craft),
    /// listed in this object's card and not drawn: `(norad_id, object_name)`.
    pub attached: Attached,
    /// sgp4's initialised propagator for `record`. **Unstable:** this is an
    /// `sgp4` type, so an `sgp4` upgrade can change it; prefer
    /// `orbit::propagate::state_at`.
    pub constants: sgp4::Constants,
}

pub struct Catalog {
    /// Sorted by `record.norad_id`.
    pub objects: Vec<SatObject>,
    /// Records sgp4 refused to initialise (`Constants::from_elements` erred).
    pub skipped_by_sgp4: usize,
}

/// `SatObject::attached`: `(norad_id, object_name)` of the stack's other members.
pub type Attached = Vec<(u32, Box<str>)>;

/// The elements two records must share, bit for bit, to be drawn as one.
fn stack_key(r: &OmmRecord) -> [u64; 7] {
    [
        r.epoch_unix.to_bits(),
        r.mean_motion.to_bits(),
        r.eccentricity.to_bits(),
        r.inclination_deg.to_bits(),
        r.raan_deg.to_bits(),
        r.arg_pericenter_deg.to_bits(),
        r.mean_anomaly_deg.to_bits(),
    ]
}

fn classify(r: &OmmRecord, m: &Membership) -> Group {
    let has = |set: &Option<HashSet<u32>>| set.as_ref().is_some_and(|s| s.contains(&r.norad_id));
    if has(&m.stations) {
        Group::Stations
    } else if has(&m.gps) {
        Group::Gps
    } else if has(&m.galileo) {
        Group::Galileo
    } else if has(&m.glonass) {
        Group::Glonass
    } else if has(&m.beidou) {
        // Before `geo`: BeiDou's GEO and IGSO satellites are navigation
        // first -- the navigation lists win over the orbit-shape list.
        Group::Beidou
    } else if has(&m.geo) {
        Group::Geo
    } else if r.object_name.starts_with("STARLINK") {
        Group::Starlink
    } else {
        Group::Other
    }
}

impl Catalog {
    /// De-duplicates by NORAD id (the greatest `epoch_unix` wins), collapses
    /// identical element sets onto their lowest NORAD id, classifies, and
    /// initialises SGP4 (an `Err` drops the object and counts it).
    #[must_use]
    pub fn build(records: Vec<OmmRecord>, membership: &Membership) -> Catalog {
        // 1. One record per NORAD id: the newest epoch.
        let mut by_id: HashMap<u32, OmmRecord> = HashMap::with_capacity(records.len());
        for r in records {
            match by_id.get(&r.norad_id) {
                Some(have) if have.epoch_unix >= r.epoch_unix => {}
                _ => {
                    by_id.insert(r.norad_id, r);
                }
            }
        }
        let mut unique: Vec<OmmRecord> = by_id.into_values().collect();
        unique.sort_unstable_by_key(|r| r.norad_id);

        // 2. Stacks: identical elements collapse onto the lowest id. `unique`
        //    is id-sorted, so the first record seen for a key is the primary.
        let mut primary_of: HashMap<[u64; 7], usize> = HashMap::with_capacity(unique.len());
        let mut primaries: Vec<(OmmRecord, Attached)> = Vec::with_capacity(unique.len());
        for r in unique {
            match primary_of.get(&stack_key(&r)) {
                Some(&i) => primaries[i].1.push((r.norad_id, r.object_name.clone())),
                None => {
                    primary_of.insert(stack_key(&r), primaries.len());
                    primaries.push((r, Vec::new()));
                }
            }
        }

        // 3-5. Classify, initialise SGP4; still sorted by id.
        let mut skipped_by_sgp4 = 0;
        let mut objects = Vec::with_capacity(primaries.len());
        for (record, attached) in primaries {
            match sgp4::Constants::from_elements(&to_elements(&record)) {
                Ok(constants) => {
                    let group = classify(&record, membership);
                    objects.push(SatObject { record, group, attached, constants });
                }
                Err(_) => skipped_by_sgp4 += 1,
            }
        }
        Catalog { objects, skipped_by_sgp4 }
    }

    /// The index of the drawn object with this NORAD id; `None` if it is not
    /// drawn (unknown, refused by sgp4, or attached to another object).
    #[must_use]
    pub fn index_of(&self, norad: u32) -> Option<usize> {
        self.objects.binary_search_by_key(&norad, |o| o.record.norad_id).ok()
    }

    /// Drawn objects per [`Group`], indexed by `group as usize`.
    #[must_use]
    pub fn group_counts(&self) -> [usize; GROUP_COUNT] {
        let mut n = [0; GROUP_COUNT];
        for o in &self.objects {
            n[o.group as usize] += 1;
        }
        n
    }

    /// `(min, median, max)` of the drawn objects' epochs, Unix seconds;
    /// `None` when the catalogue is empty. The median of an even count is
    /// the upper middle value.
    #[must_use]
    pub fn epoch_stats_unix(&self) -> Option<(f64, f64, f64)> {
        let mut e: Vec<f64> = self.objects.iter().map(|o| o.record.epoch_unix).collect();
        if e.is_empty() {
            return None;
        }
        e.sort_by(f64::total_cmp);
        Some((e[0], e[e.len() / 2], e[e.len() - 1]))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::orbit::omm::parse_omm;
    const STATIONS: &[u8] = include_bytes!("../../tests/fixtures/orbit/stations.json");

    fn stations_catalog() -> Catalog {
        let recs = parse_omm(STATIONS).unwrap().records;
        let ids = recs.iter().map(|r| r.norad_id).collect();
        Catalog::build(recs, &Membership { stations: Some(ids), ..Membership::default() })
    }

    #[test]
    fn identical_elements_collapse_onto_the_lowest_norad_id() {
        let c = stations_catalog();
        // 22 records: the ISS stack (8 share elements), the CSS stack (5 share), 9 singles.
        assert_eq!(c.objects.len(), 11);
        let iss = &c.objects[c.index_of(ISS_NORAD).expect("the ISS is drawn")];
        let mut attached: Vec<u32> = iss.attached.iter().map(|a| a.0).collect();
        attached.sort_unstable();
        assert_eq!(attached, vec![36086, 49044, 67796, 68689, 68837, 100057, 100712]);
        assert!(c.index_of(36086).is_none(), "POISK is listed in the ISS's card, not drawn");
        let css = &c.objects[c.index_of(48274).expect("TIANHE is the lowest CSS id")];
        assert_eq!(css.attached.len(), 4);
    }

    #[test]
    fn a_duplicate_norad_id_keeps_the_newest_epoch() {
        let mut recs = parse_omm(STATIONS).unwrap().records;
        let mut older = recs.iter().find(|r| r.norad_id == 49271).unwrap().clone();
        older.epoch_unix -= 86_400.0;
        older.mean_anomaly_deg = 1.0;
        recs.push(older);
        let c = Catalog::build(recs, &Membership::default());
        let o = &c.objects[c.index_of(49271).unwrap()];
        assert!((o.record.mean_anomaly_deg - 1.0).abs() > 1e-9, "the older copy won");
    }

    #[test]
    fn classification_follows_membership_then_the_starlink_prefix() {
        let mut recs = parse_omm(STATIONS).unwrap().records;
        let mut s = recs[0].clone();
        s.norad_id = 44_713;
        s.object_name = "STARLINK-1007".into();
        s.mean_anomaly_deg = 99.0;
        recs.push(s);
        let c = Catalog::build(recs, &Membership { stations: Some([25544].into()), ..Membership::default() });
        assert_eq!(c.objects[c.index_of(25544).unwrap()].group, Group::Stations);
        assert_eq!(c.objects[c.index_of(44_713).unwrap()].group, Group::Starlink);
        assert_eq!(c.objects[c.index_of(49271).unwrap()].group, Group::Other);
    }

    #[test]
    fn group_counts_and_epoch_stats() {
        let c = stations_catalog();
        assert_eq!(c.group_counts(), [11, 0, 0, 0, 0, 0, 0, 0]);
        let (lo, med, hi) = c.epoch_stats_unix().unwrap();
        assert!(lo <= med && med <= hi);
    }
}
