//! The "About this map" panel's text (spec §6): the dots, the hexagons and
//! the donut clusters.

/// The About panel's paragraphs, in order.
pub const ABOUT: &[&str] = &[
    "2020 Census: 331,449,281 people, one dot each, coloured by race and ethnicity in Kyle Walker's eight groups.",
    "Dot locations are simulated. Each person is placed at random inside their Census block, not at home addresses: \
     a dot is not a residence. Zoom in to see one dot per person; at lower zooms each dot stands for several people.",
    "Hexagons show each area's largest group; light grey means a tie. Hexagons with fewer than 10 people are hidden, \
     so a blank area is not necessarily unpopulated.",
    "Donuts show the group shares and the total of the people nearby; sizes use a compressed scale; click one to zoom \
     in. Hiding a group takes it out of the rings, but a donut's size and count still include everyone.",
    "States show a pie per state, its area by population (California largest, the smallest states and DC kept \
     readable at a minimum size), and a bar per state in the panel; click either to frame the state and read its \
     groups. Hidden groups are left out and the rest shown as shares of what remains.",
    "Source: U.S. Census Bureau, 2020 Census Redistricting Data (P.L. 94-171), table P2, by block. For Washington, DC, \
     every block's P2 counts are identical to the DHC table P9 that Kyle Walker's map uses. Hispanic or Latino \
     includes any race; all other groups are non-Hispanic.",
    "Privacy: the Census Bureau protects the 2020 counts with differential privacy, adding statistical noise to \
     block counts to protect identities. In sparsely populated areas this can produce unexpected counts, including zeros. A zero is not proof \
     that no one in a group lives there.",
    "Data: U.S. Census Bureau, 2020 Census P.L. 94-171 and TIGER/Line 2020 block shapes, and the 2020 \
     cartographic boundary states at 1:20M (public domain). Colours: the \
     Okabe-Ito colour-blind-safe palette.",
];

/// The inspiration credit, drawn as a link under [`ABOUT`].
pub const INSPIRED_BY: &str =
    "Inspired by Kyle Walker's 2020 race and ethnicity map and the Racial Dot Map (Dustin Cable, University of Virginia, 2013).";

/// Kyle Walker's map.
pub const WALKER_URL: &str = "https://walker-data.com/maps/census-2020/race.html";

/// The credit the U.S. Census Bureau is owed on the map's attribution line
/// (the archive's own metadata carries it; the demo checks).
pub const CENSUS_CREDIT: &str = "U.S. Census Bureau";
