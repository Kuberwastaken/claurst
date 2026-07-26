/// Spinner verbs displayed during processing.
pub const SPINNER_VERBS: &[&str] = &[
    "Accomplishing", "Actioning", "Actualizing", "Architecting", "Baking", "Beaming",
    "Beboppin'", "Befuddling", "Billowing", "Blanching", "Bloviating", "Boogieing",
    "Boondoggling", "Booping", "Bootstrapping", "Brewing", "Bunning", "Burrowing",
    "Calculating", "Canoodling", "Caramelizing", "Cascading", "Catapulting", "Cerebrating",
    "Channeling", "Choreographing", "Churning", "Clauding", "Coalescing", "Cogitating",
    "Combobulating", "Composing", "Computing", "Concocting", "Considering", "Contemplating",
    "Cooking", "Crafting", "Creating", "Crunching", "Crystallizing", "Cultivating",
    "Deciphering", "Deliberating", "Determining", "Dilly-dallying", "Discombobulating",
    "Doing", "Doodling", "Drizzling", "Ebbing", "Effecting", "Elucidating", "Embellishing",
    "Enchanting", "Envisioning", "Evaporating", "Fermenting", "Fiddle-faddling", "Finagling",
    "Flambéing", "Flibbertigibbeting", "Flowing", "Flummoxing", "Fluttering", "Forging",
    "Forming", "Frolicking", "Frosting", "Gallivanting", "Galloping", "Garnishing",
    "Generating", "Gesticulating", "Germinating", "Gitifying", "Grooving", "Gusting",
    "Harmonizing", "Hashing", "Hatching", "Herding", "Honking", "Hullaballooing",
    "Hyperspacing", "Ideating", "Imagining", "Improvising", "Incubating", "Inferring",
    "Infusing", "Ionizing", "Jitterbugging", "Julienning", "Kneading", "Leavening",
    "Levitating", "Lollygagging", "Manifesting", "Marinating", "Meandering", "Metamorphosing",
    "Misting", "Moonwalking", "Moseying", "Mulling", "Mustering", "Musing", "Nebulizing",
    "Nesting", "Newspapering", "Noodling", "Nucleating", "Orbiting", "Orchestrating",
    "Osmosing", "Perambulating", "Percolating", "Perusing", "Philosophising",
    "Photosynthesizing", "Pollinating", "Pondering", "Pontificating", "Pouncing",
    "Precipitating", "Prestidigitating", "Processing", "Proofing", "Propagating", "Puttering",
    "Puzzling", "Quantumizing", "Razzle-dazzling", "Razzmatazzing", "Recombobulating",
    "Reticulating", "Roosting", "Ruminating", "Sautéing", "Scampering", "Schlepping",
    "Scurrying", "Seasoning", "Shenaniganing", "Shimmying", "Simmering", "Skedaddling",
    "Sketching", "Slithering", "Smooshing", "Sock-hopping", "Spelunking", "Spinning",
    "Sprouting", "Stewing", "Sublimating", "Swirling", "Swooping", "Symbioting",
    "Synthesizing", "Tempering", "Thinking", "Thundering", "Tinkering", "Tomfoolering",
    "Topsy-turvying", "Transfiguring", "Transmuting", "Twisting", "Undulating", "Unfurling",
    "Unravelling", "Vibing", "Waddling", "Wandering", "Warping", "Whatchamacalliting",
    "Whirlpooling", "Whirring", "Whisking", "Wibbling", "Working", "Wrangling", "Zesting",
    "Zigzagging",
    // Crab wordplay (Rustle) — cohesive with the completion verbs below.
    "Carapacing", "Scuttling", "Molting", "Clawing", "Pinching", "Snipping",
    "Sidling", "Skittering", "Chelating", "Crabwalking", "Clacking", "Scrabbling",
    "Shelling", "Nipping", "Beachcombing", "Pincering", "Barnacling", "Reef-crawling",
    "Tide-pooling",
];

/// Past-tense verbs shown in the status row after a turn completes.
///
/// A mix of the neutral originals and a big pile of crab / crustacean wordplay,
/// in honour of Rustle (claurst's crab mascot) — so "Carapaced for 2m 5s" and
/// friends scuttle by when a turn finishes.
pub const TURN_COMPLETION_VERBS: &[&str] = &[
    // Neutral.
    "Baked", "Brewed", "Churned", "Cogitated", "Cooked", "Crunched",
    "Pondered", "Processed", "Worked",
    // Crab wordplay (Rustle).
    "Carapaced", "Scuttled", "Molted", "Clawed", "Pinched", "Snipped",
    "Sidled", "Skittered", "Burrowed", "Chelated", "Crabwalked", "Clacked",
    "Scrabbled", "Shelled", "Nipped", "Beachcombed", "Scurried", "Pincered",
    "Barnacled", "Tide-pooled", "Crustaceated", "Molt-hopped", "Clam-baked",
    "Shell-shocked", "Low-tided", "Reef-crawled",
];

/// Select a random spinner verb.
pub fn sample_spinner_verb(seed: usize) -> &'static str {
    SPINNER_VERBS[seed % SPINNER_VERBS.len()]
}

/// Select a random completion verb.
pub fn sample_completion_verb(seed: usize) -> &'static str {
    TURN_COMPLETION_VERBS[seed % TURN_COMPLETION_VERBS.len()]
}
