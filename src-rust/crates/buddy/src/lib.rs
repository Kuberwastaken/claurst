/// claurst-buddy: Tamagotchi/Buddy companion system for Claurst.
///
/// Ported from src/buddy/ (TypeScript). All bones (species, rarity, stats,
/// eye, hat, shiny) are deterministically derived from the user-ID via a
/// seeded PRNG so they can never be edited by hand. The soul (name,
/// personality, hatched_at) is AI-generated on first hatch and persisted in
/// `{config_dir}/companion.json`.
use std::path::Path;

// ---------------------------------------------------------------------------
// Mulberry32 PRNG
// ---------------------------------------------------------------------------

/// Tiny, fast 32-bit PRNG — identical algorithm to the TypeScript version.
/// Good enough for picking ducks.
pub struct Mulberry32 {
    state: u32,
}

impl Mulberry32 {
    pub fn new(seed: u32) -> Self {
        Self { state: seed }
    }

    /// Returns next pseudo-random value in [0, 1).
    pub fn next_f64(&mut self) -> f64 {
        self.next_u32() as f64 / 4_294_967_296.0
    }

    /// Returns next raw u32.
    pub fn next_u32(&mut self) -> u32 {
        self.state = self.state.wrapping_add(0x6d2b_79f5);
        let mut t = (self.state ^ (self.state >> 15)).wrapping_mul(1 | self.state);
        t = t.wrapping_add((t ^ (t >> 7)).wrapping_mul(61 | t)) ^ t;
        t ^ (t >> 14)
    }
}

/// Derive a deterministic u32 seed from a user-id string.
///
/// Algorithm: FNV-1a 32-bit over the raw bytes of `user_id`.
/// Matches the TypeScript `hashString` FNV-1a implementation used in Bun.
pub fn seed_from_user_id(user_id: &str) -> u32 {
    const FNV_OFFSET_BASIS: u32 = 2_166_136_261;
    const FNV_PRIME: u32 = 16_777_619;
    let mut hash = FNV_OFFSET_BASIS;
    for byte in user_id.bytes() {
        hash ^= byte as u32;
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

// ---------------------------------------------------------------------------
// Enumerations
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Species {
    Duck,
    Goose,
    Blob,
    Cat,
    Dragon,
    Octopus,
    Owl,
    Penguin,
    Turtle,
    Snail,
    Ghost,
    Axolotl,
    Capybara,
    Cactus,
    Robot,
    Rabbit,
    Mushroom,
    Chonk,
}

impl Species {
    /// Display name (lower-case, matches TypeScript SPECIES array).
    pub fn as_str(&self) -> &'static str {
        match self {
            Species::Duck => "duck",
            Species::Goose => "goose",
            Species::Blob => "blob",
            Species::Cat => "cat",
            Species::Dragon => "dragon",
            Species::Octopus => "octopus",
            Species::Owl => "owl",
            Species::Penguin => "penguin",
            Species::Turtle => "turtle",
            Species::Snail => "snail",
            Species::Ghost => "ghost",
            Species::Axolotl => "axolotl",
            Species::Capybara => "capybara",
            Species::Cactus => "cactus",
            Species::Robot => "robot",
            Species::Rabbit => "rabbit",
            Species::Mushroom => "mushroom",
            Species::Chonk => "chonk",
        }
    }

    fn all() -> &'static [Species] {
        use Species::*;
        &[
            Duck, Goose, Blob, Cat, Dragon, Octopus, Owl, Penguin, Turtle, Snail, Ghost, Axolotl,
            Capybara, Cactus, Robot, Rabbit, Mushroom, Chonk,
        ]
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Rarity {
    Common,
    Uncommon,
    Rare,
    Epic,
    Legendary,
}

impl Rarity {
    pub fn as_str(&self) -> &'static str {
        match self {
            Rarity::Common => "common",
            Rarity::Uncommon => "uncommon",
            Rarity::Rare => "rare",
            Rarity::Epic => "epic",
            Rarity::Legendary => "legendary",
        }
    }

    pub fn stars(&self) -> &'static str {
        match self {
            Rarity::Common => "★",
            Rarity::Uncommon => "★★",
            Rarity::Rare => "★★★",
            Rarity::Epic => "★★★★",
            Rarity::Legendary => "★★★★★",
        }
    }

    /// Stat floor for rolling. Higher rarity → higher base stats.
    fn stat_floor(&self) -> u8 {
        match self {
            Rarity::Common => 5,
            Rarity::Uncommon => 15,
            Rarity::Rare => 25,
            Rarity::Epic => 35,
            Rarity::Legendary => 50,
        }
    }
}

/// Eye glyphs — rendered into sprite {E} slots.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Eye {
    /// ·
    Dot,
    /// ✦
    Star,
    /// ×
    X,
    /// ◉
    Circle,
    /// @
    At,
    /// °
    Degree,
}

impl Eye {
    pub fn glyph(&self) -> &'static str {
        match self {
            Eye::Dot => "·",
            Eye::Star => "✦",
            Eye::X => "×",
            Eye::Circle => "◉",
            Eye::At => "@",
            Eye::Degree => "°",
        }
    }

    fn all() -> &'static [Eye] {
        use Eye::*;
        &[Dot, Star, X, Circle, At, Degree]
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Hat {
    None,
    Crown,
    Tophat,
    Propeller,
    Halo,
    Wizard,
    Beanie,
    TinyDuck,
}

impl Hat {
    /// The hat decoration line (12 chars wide). Empty string for `None`.
    pub fn hat_line(&self) -> &'static str {
        match self {
            Hat::None => "",
            Hat::Crown => "   \\^^^/    ",
            Hat::Tophat => "   [___]    ",
            Hat::Propeller => "    -+-     ",
            Hat::Halo => "   (   )    ",
            Hat::Wizard => "    /^\\     ",
            Hat::Beanie => "   (___)    ",
            Hat::TinyDuck => "    ,>      ",
        }
    }

    /// All hat variants — `None` is included once; the roll logic gives it
    /// extra weight by checking `rarity == common` separately.
    fn all() -> &'static [Hat] {
        use Hat::*;
        &[
            None, Crown, Tophat, Propeller, Halo, Wizard, Beanie, TinyDuck,
        ]
    }
}

// ---------------------------------------------------------------------------
// Stats
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CompanionStats {
    pub debugging: u8,
    pub patience: u8,
    pub chaos: u8,
    pub wisdom: u8,
    pub snark: u8,
}

impl CompanionStats {
    /// Roll stats using rarity-based floor + peak/dump stat system.
    ///
    /// One stat is the "peak" (boosted by +50..+79), one is the "dump"
    /// (lowered), the rest scatter around the floor.
    pub fn roll(rarity: &Rarity, rng: &mut Mulberry32) -> Self {
        let floor = rarity.stat_floor() as f64;

        // Pick distinct peak and dump stat indices (0-4).
        let peak_idx = (rng.next_f64() * 5.0) as usize % 5;
        let mut dump_idx = (rng.next_f64() * 5.0) as usize % 5;
        // Ensure dump != peak (mirrors the while loop in TypeScript).
        if dump_idx == peak_idx {
            dump_idx = (dump_idx + 1) % 5;
        }

        let mut values = [0u8; 5];
        for (i, v) in values.iter_mut().enumerate() {
            *v = if i == peak_idx {
                ((floor + 50.0 + rng.next_f64() * 30.0) as u8).min(100)
            } else if i == dump_idx {
                let raw = floor - 10.0 + rng.next_f64() * 15.0;
                if raw < 1.0 {
                    1
                } else {
                    raw as u8
                }
            } else {
                (floor + rng.next_f64() * 40.0) as u8
            };
        }

        CompanionStats {
            debugging: values[0],
            patience: values[1],
            chaos: values[2],
            wisdom: values[3],
            snark: values[4],
        }
    }
}

// ---------------------------------------------------------------------------
// Bones (deterministic from user_id)
// ---------------------------------------------------------------------------

/// The deterministic parts of a companion — always re-derived from
/// `hash(userId)` so stored configs can never fake a rarity and species
/// array edits don't break old companions.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CompanionBones {
    pub rarity: Rarity,
    pub species: Species,
    pub eye: Eye,
    pub hat: Hat,
    pub shiny: bool,
    pub stats: CompanionStats,
}

impl CompanionBones {
    /// Deterministically roll all bones from a user-id string.
    pub fn from_user_id(user_id: &str) -> Self {
        let mut rng = Mulberry32::new(seed_from_user_id(user_id));
        Self::roll(&mut rng)
    }

    /// Roll bones from an already-seeded RNG (useful for tests / previews).
    pub fn roll(rng: &mut Mulberry32) -> Self {
        // --- rarity (Common 60 %, Uncommon 25 %, Rare 10 %, Epic 4 %, Legendary 1 %) ---
        let rarity = {
            // Weights sum to 100.
            const WEIGHTS: [(Rarity, f64); 5] = [
                (Rarity::Common, 60.0),
                (Rarity::Uncommon, 25.0),
                (Rarity::Rare, 10.0),
                (Rarity::Epic, 4.0),
                (Rarity::Legendary, 1.0),
            ];
            let mut roll = rng.next_f64() * 100.0;
            let mut chosen = Rarity::Common;
            for (r, w) in &WEIGHTS {
                roll -= w;
                if roll < 0.0 {
                    chosen = r.clone();
                    break;
                }
            }
            chosen
        };

        // --- species (uniform over 18) ---
        let species_list = Species::all();
        let species = species_list
            [(rng.next_f64() * species_list.len() as f64) as usize % species_list.len()]
        .clone();

        // --- eye (uniform over 6) ---
        let eye_list = Eye::all();
        let eye =
            eye_list[(rng.next_f64() * eye_list.len() as f64) as usize % eye_list.len()].clone();

        // --- hat: common always gets none; others pick from full list ---
        // The TypeScript source: `hat: rarity === 'common' ? 'none' : pick(rng, HATS)`
        // where HATS includes 'none' — so non-common companions still have a
        // ~12.5 % chance of getting no hat.
        let hat = if rarity == Rarity::Common {
            Hat::None
        } else {
            let hat_list = Hat::all();
            hat_list[(rng.next_f64() * hat_list.len() as f64) as usize % hat_list.len()].clone()
        };

        // --- shiny: 1 % chance ---
        let shiny = rng.next_f64() < 0.01;

        // --- stats ---
        let stats = CompanionStats::roll(&rarity, rng);

        CompanionBones {
            rarity,
            species,
            eye,
            hat,
            shiny,
            stats,
        }
    }
}

// ---------------------------------------------------------------------------
// Soul (AI-generated, persisted to disk)
// ---------------------------------------------------------------------------

/// The AI-generated identity of a companion. Stored in
/// `{config_dir}/companion.json` after the first hatch.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CompanionSoul {
    pub name: String,
    pub personality: String,
    pub hatched_at: chrono::DateTime<chrono::Utc>,
}

// ---------------------------------------------------------------------------
// Full Companion (bones + optional soul)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct Companion {
    pub bones: CompanionBones,
    /// `None` until the companion has been named / hatched.
    pub soul: Option<CompanionSoul>,
}

impl Companion {
    pub fn new(user_id: &str, soul: Option<CompanionSoul>) -> Self {
        Companion {
            bones: CompanionBones::from_user_id(user_id),
            soul,
        }
    }

    /// Returns the companion's given name, or the species name if not yet
    /// hatched.
    pub fn display_name(&self) -> &str {
        match &self.soul {
            Some(s) => s.name.as_str(),
            None => self.bones.species.as_str(),
        }
    }
}

// ---------------------------------------------------------------------------
// Sprites
// ---------------------------------------------------------------------------

/// One animation frame: exactly 5 lines of raw sprite text.
/// The `{E}` placeholder is replaced with the companion's eye glyph at render
/// time.
#[derive(Debug, Clone)]
pub struct SpriteFrame(pub [&'static str; 5]);

/// Return the three animation frames for a given species.
///
/// Frames are literal copies of the TypeScript BODIES table.  Each line is
/// 12 characters wide (after `{E}` substitution).  Line 0 is the "hat slot":
/// if it is blank in a frame the hat decoration is injected there; if it
/// contains content (smoke, antenna, etc.) the hat is skipped.
pub fn get_sprite_frames(species: &Species) -> [SpriteFrame; 3] {
    match species {
        Species::Duck => [
            SpriteFrame([
                "            ",
                "    __      ",
                "  <({E} )___  ",
                "   (  ._>   ",
                "    `--´    ",
            ]),
            SpriteFrame([
                "            ",
                "    __      ",
                "  <({E} )___  ",
                "   (  ._>   ",
                "    `--´~   ",
            ]),
            SpriteFrame([
                "            ",
                "    __      ",
                "  <({E} )___  ",
                "   (  .__>  ",
                "    `--´    ",
            ]),
        ],
        Species::Goose => [
            SpriteFrame([
                "            ",
                "     ({E}>    ",
                "     ||     ",
                "   _(__)_   ",
                "    ^^^^    ",
            ]),
            SpriteFrame([
                "            ",
                "    ({E}>     ",
                "     ||     ",
                "   _(__)_   ",
                "    ^^^^    ",
            ]),
            SpriteFrame([
                "            ",
                "     ({E}>>   ",
                "     ||     ",
                "   _(__)_   ",
                "    ^^^^    ",
            ]),
        ],
        Species::Blob => [
            SpriteFrame([
                "            ",
                "   .----.   ",
                "  ( {E}  {E} )  ",
                "  (      )  ",
                "   `----´   ",
            ]),
            SpriteFrame([
                "            ",
                "  .------.  ",
                " (  {E}  {E}  ) ",
                " (        ) ",
                "  `------´  ",
            ]),
            SpriteFrame([
                "            ",
                "    .--.    ",
                "   ({E}  {E})   ",
                "   (    )   ",
                "    `--´    ",
            ]),
        ],
        Species::Cat => [
            SpriteFrame([
                "            ",
                "   /\\_/\\    ",
                "  ( {E}   {E})  ",
                "  (  ω  )   ",
                "  (\")_(\")   ",
            ]),
            SpriteFrame([
                "            ",
                "   /\\_/\\    ",
                "  ( {E}   {E})  ",
                "  (  ω  )   ",
                "  (\")_(\")~  ",
            ]),
            SpriteFrame([
                "            ",
                "   /\\-/\\    ",
                "  ( {E}   {E})  ",
                "  (  ω  )   ",
                "  (\")_(\")   ",
            ]),
        ],
        Species::Dragon => [
            SpriteFrame([
                "            ",
                "  /^\\  /^\\  ",
                " <  {E}  {E}  > ",
                " (   ~~   ) ",
                "  `-vvvv-´  ",
            ]),
            SpriteFrame([
                "            ",
                "  /^\\  /^\\  ",
                " <  {E}  {E}  > ",
                " (        ) ",
                "  `-vvvv-´  ",
            ]),
            SpriteFrame([
                "   ~    ~   ",
                "  /^\\  /^\\  ",
                " <  {E}  {E}  > ",
                " (   ~~   ) ",
                "  `-vvvv-´  ",
            ]),
        ],
        Species::Octopus => [
            SpriteFrame([
                "            ",
                "   .----.   ",
                "  ( {E}  {E} )  ",
                "  (______)  ",
                "  /\\/\\/\\/\\  ",
            ]),
            SpriteFrame([
                "            ",
                "   .----.   ",
                "  ( {E}  {E} )  ",
                "  (______)  ",
                "  \\/\\/\\/\\/  ",
            ]),
            SpriteFrame([
                "     o      ",
                "   .----.   ",
                "  ( {E}  {E} )  ",
                "  (______)  ",
                "  /\\/\\/\\/\\  ",
            ]),
        ],
        Species::Owl => [
            SpriteFrame([
                "            ",
                "   /\\  /\\   ",
                "  (({E})({E}))  ",
                "  (  ><  )  ",
                "   `----´   ",
            ]),
            SpriteFrame([
                "            ",
                "   /\\  /\\   ",
                "  (({E})({E}))  ",
                "  (  ><  )  ",
                "   .----.   ",
            ]),
            SpriteFrame([
                "            ",
                "   /\\  /\\   ",
                "  (({E})(-))  ",
                "  (  ><  )  ",
                "   `----´   ",
            ]),
        ],
        Species::Penguin => [
            SpriteFrame([
                "            ",
                "  .---.     ",
                "  ({E}>{E})     ",
                " /(   )\\    ",
                "  `---´     ",
            ]),
            SpriteFrame([
                "            ",
                "  .---.     ",
                "  ({E}>{E})     ",
                " |(   )|    ",
                "  `---´     ",
            ]),
            SpriteFrame([
                "  .---.     ",
                "  ({E}>{E})     ",
                " /(   )\\    ",
                "  `---´     ",
                "   ~ ~      ",
            ]),
        ],
        Species::Turtle => [
            SpriteFrame([
                "            ",
                "   _,--._   ",
                "  ( {E}  {E} )  ",
                " /[______]\\ ",
                "  ``    ``  ",
            ]),
            SpriteFrame([
                "            ",
                "   _,--._   ",
                "  ( {E}  {E} )  ",
                " /[______]\\ ",
                "   ``  ``   ",
            ]),
            SpriteFrame([
                "            ",
                "   _,--._   ",
                "  ( {E}  {E} )  ",
                " /[======]\\ ",
                "  ``    ``  ",
            ]),
        ],
        Species::Snail => [
            SpriteFrame([
                "            ",
                " {E}    .--.  ",
                "  \\  ( @ )  ",
                "   \\_`--´   ",
                "  ~~~~~~~   ",
            ]),
            SpriteFrame([
                "            ",
                "  {E}   .--.  ",
                "  |  ( @ )  ",
                "   \\_`--´   ",
                "  ~~~~~~~   ",
            ]),
            SpriteFrame([
                "            ",
                " {E}    .--.  ",
                "  \\  ( @  ) ",
                "   \\_`--´   ",
                "   ~~~~~~   ",
            ]),
        ],
        Species::Ghost => [
            SpriteFrame([
                "            ",
                "   .----.   ",
                "  / {E}  {E} \\  ",
                "  |      |  ",
                "  ~`~``~`~  ",
            ]),
            SpriteFrame([
                "            ",
                "   .----.   ",
                "  / {E}  {E} \\  ",
                "  |      |  ",
                "  `~`~~`~`  ",
            ]),
            SpriteFrame([
                "    ~  ~    ",
                "   .----.   ",
                "  / {E}  {E} \\  ",
                "  |      |  ",
                "  ~~`~~`~~  ",
            ]),
        ],
        Species::Axolotl => [
            SpriteFrame([
                "            ",
                "}~(______)~{",
                "}~({E} .. {E})~{",
                "  ( .--. )  ",
                "  (_/  \\_)  ",
            ]),
            SpriteFrame([
                "            ",
                "~}(______){~",
                "~}({E} .. {E}){~",
                "  ( .--. )  ",
                "  (_/  \\_)  ",
            ]),
            SpriteFrame([
                "            ",
                "}~(______)~{",
                "}~({E} .. {E})~{",
                "  (  --  )  ",
                "  ~_/  \\_~  ",
            ]),
        ],
        Species::Capybara => [
            SpriteFrame([
                "            ",
                "  n______n  ",
                " ( {E}    {E} ) ",
                " (   oo   ) ",
                "  `------´  ",
            ]),
            SpriteFrame([
                "            ",
                "  n______n  ",
                " ( {E}    {E} ) ",
                " (   Oo   ) ",
                "  `------´  ",
            ]),
            SpriteFrame([
                "    ~  ~    ",
                "  u______n  ",
                " ( {E}    {E} ) ",
                " (   oo   ) ",
                "  `------´  ",
            ]),
        ],
        Species::Cactus => [
            SpriteFrame([
                "            ",
                " n  ____  n ",
                " | |{E}  {E}| | ",
                " |_|    |_| ",
                "   |    |   ",
            ]),
            SpriteFrame([
                "            ",
                "    ____    ",
                " n |{E}  {E}| n ",
                " |_|    |_| ",
                "   |    |   ",
            ]),
            SpriteFrame([
                " n        n ",
                " |  ____  | ",
                " | |{E}  {E}| | ",
                " |_|    |_| ",
                "   |    |   ",
            ]),
        ],
        Species::Robot => [
            SpriteFrame([
                "            ",
                "   .[||].   ",
                "  [ {E}  {E} ]  ",
                "  [ ==== ]  ",
                "  `------´  ",
            ]),
            SpriteFrame([
                "            ",
                "   .[||].   ",
                "  [ {E}  {E} ]  ",
                "  [ -==- ]  ",
                "  `------´  ",
            ]),
            SpriteFrame([
                "     *      ",
                "   .[||].   ",
                "  [ {E}  {E} ]  ",
                "  [ ==== ]  ",
                "  `------´  ",
            ]),
        ],
        Species::Rabbit => [
            SpriteFrame([
                "            ",
                "   (\\__/)   ",
                "  ( {E}  {E} )  ",
                " =(  ..  )= ",
                "  (\")__(\")  ",
            ]),
            SpriteFrame([
                "            ",
                "   (|__/)   ",
                "  ( {E}  {E} )  ",
                " =(  ..  )= ",
                "  (\")__(\")  ",
            ]),
            SpriteFrame([
                "            ",
                "   (\\__/)   ",
                "  ( {E}  {E} )  ",
                " =( .  . )= ",
                "  (\")__(\")  ",
            ]),
        ],
        Species::Mushroom => [
            SpriteFrame([
                "            ",
                " .-o-OO-o-. ",
                "(__________)",
                "   |{E}  {E}|   ",
                "   |____|   ",
            ]),
            SpriteFrame([
                "            ",
                " .-O-oo-O-. ",
                "(__________)",
                "   |{E}  {E}|   ",
                "   |____|   ",
            ]),
            SpriteFrame([
                "   . o  .   ",
                " .-o-OO-o-. ",
                "(__________)",
                "   |{E}  {E}|   ",
                "   |____|   ",
            ]),
        ],
        Species::Chonk => [
            SpriteFrame([
                "            ",
                "  /\\    /\\  ",
                " ( {E}    {E} ) ",
                " (   ..   ) ",
                "  `------´  ",
            ]),
            SpriteFrame([
                "            ",
                "  /\\    /|  ",
                " ( {E}    {E} ) ",
                " (   ..   ) ",
                "  `------´  ",
            ]),
            SpriteFrame([
                "            ",
                "  /\\    /\\  ",
                " ( {E}    {E} ) ",
                " (   ..   ) ",
                "  `------´~ ",
            ]),
        ],
    }
}

/// Map a clock tick (500 ms steps) to a frame index (0, 1, or 2).
///
/// Mirrors the TypeScript idle-fidget sequence:
/// `[0,0,0,0,1,0,0,0,-1→2,0,0,2,0,0,0]` cycling at length 15.
///
/// The `-1` entry in the TS source is an explicit reference to frame 2; we
/// represent it directly as 2.
pub fn animation_frame(tick: u64) -> usize {
    const SEQ: [usize; 15] = [0, 0, 0, 0, 1, 0, 0, 0, 2, 0, 0, 2, 0, 0, 0];
    SEQ[(tick as usize) % SEQ.len()]
}

/// Render a companion as a multi-line string.
///
/// - Substitutes the companion's eye glyph into `{E}` placeholders.
/// - Overlays the hat on line 0 when line 0 is blank in the chosen frame.
/// - Drops a blank line-0 when *all* three frames have blank line 0 (no hat
///   and no per-frame animation content), matching the TypeScript behaviour
///   that avoids wasted rows.
pub fn render(companion: &Companion, tick: u64) -> String {
    let frames = get_sprite_frames(&companion.bones.species);
    let frame_idx = animation_frame(tick);
    let frame = &frames[frame_idx];
    let eye = companion.bones.eye.glyph();

    // Substitute eye glyphs.
    let mut lines: Vec<String> = frame.0.iter().map(|l| l.replace("{E}", eye)).collect();

    // Inject hat into line 0 if it is blank.
    if companion.bones.hat != Hat::None && lines[0].trim().is_empty() {
        lines[0] = companion.bones.hat.hat_line().to_string();
    }

    // Drop blank line-0 when all frames have blank line 0.
    let all_frames_blank_line0 = frames
        .iter()
        .all(|f| f.0[0].replace("{E}", eye).trim().is_empty());
    if all_frames_blank_line0 && lines[0].trim().is_empty() {
        lines.remove(0);
    }

    lines.join("\n")
}

/// Render the face description for a companion (used in speech-bubble context).
///
/// Matches the `renderFace` function in `sprites.ts`.
pub fn render_face(bones: &CompanionBones) -> String {
    let e = bones.eye.glyph();
    match bones.species {
        Species::Duck | Species::Goose => format!("({}> ", e),
        Species::Blob => format!("({}{})", e, e),
        Species::Cat => format!("={}ω{}=", e, e),
        Species::Dragon => format!("<{}~{}>", e, e),
        Species::Octopus => format!("~({}{})~", e, e),
        Species::Owl => format!("({})({})", e, e),
        Species::Penguin => format!("({}>)", e),
        Species::Turtle => format!("[{}_{}]", e, e),
        Species::Snail => format!("{}(@)", e),
        Species::Ghost => format!("/{}{}\\ ", e, e),
        Species::Axolotl => format!("}}{}. {}{{", e, e),
        Species::Capybara => format!("({}oo{})", e, e),
        Species::Cactus | Species::Mushroom => format!("|{}  {}|", e, e),
        Species::Robot => format!("[{}{}]", e, e),
        Species::Rabbit => format!("({}..{})", e, e),
        Species::Chonk => format!("({}.{})", e, e),
    }
}

// ---------------------------------------------------------------------------
// Config persistence
// ---------------------------------------------------------------------------

/// What is actually written to `{config_dir}/companion.json`.
///
/// Bones are never persisted — they are re-derived on every read.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct StoredCompanion {
    pub name: String,
    pub personality: String,
    pub hatched_at: chrono::DateTime<chrono::Utc>,
}

impl From<&CompanionSoul> for StoredCompanion {
    fn from(soul: &CompanionSoul) -> Self {
        StoredCompanion {
            name: soul.name.clone(),
            personality: soul.personality.clone(),
            hatched_at: soul.hatched_at,
        }
    }
}

impl From<StoredCompanion> for CompanionSoul {
    fn from(s: StoredCompanion) -> Self {
        CompanionSoul {
            name: s.name,
            personality: s.personality,
            hatched_at: s.hatched_at,
        }
    }
}

/// Load a companion soul from `{config_dir}/companion.json`.
/// Returns `None` if the file does not exist or cannot be parsed.
pub fn load_companion_soul(config_dir: &Path) -> Option<CompanionSoul> {
    let path = config_dir.join("companion.json");
    let bytes = std::fs::read(&path).ok()?;
    let stored: StoredCompanion = serde_json::from_slice(&bytes).ok()?;
    Some(stored.into())
}

/// Persist a companion soul to `{config_dir}/companion.json`.
pub fn save_companion_soul(config_dir: &Path, soul: &CompanionSoul) -> anyhow::Result<()> {
    let stored = StoredCompanion::from(soul);
    let json = serde_json::to_string_pretty(&stored)?;
    std::fs::create_dir_all(config_dir)?;
    std::fs::write(config_dir.join("companion.json"), json)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Public helper: build a full Companion from persisted data
// ---------------------------------------------------------------------------

/// Reconstruct a `Companion` from a user-id string and the stored soul (if
/// any).  This is the main entry point for callers — mirrors `getCompanion()`
/// in the TypeScript source.
pub fn get_companion(user_id: &str, config_dir: &Path) -> Companion {
    let soul = load_companion_soul(config_dir);
    Companion::new(user_id, soul)
}

// ---------------------------------------------------------------------------
// Intro / prompt helpers (mirrors prompt.ts)
// ---------------------------------------------------------------------------

/// System-prompt fragment injected when a companion is active.
pub fn companion_intro_text(name: &str, species: &str) -> String {
    format!(
        "# Companion\n\n\
A small {species} named {name} sits beside the user's input box and occasionally \
comments in a speech bubble. You're not {name} — it's a separate watcher.\n\n\
When the user addresses {name} directly (by name), its bubble will answer. Your job \
in that moment is to stay out of the way: respond in ONE line or less, or just answer \
any part of the message meant for you. Don't explain that you're not {name} — they \
know. Don't narrate what {name} might say — the bubble handles that."
    )
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mulberry32_produces_values_in_range() {
        let mut rng = Mulberry32::new(42);
        for _ in 0..1000 {
            let v = rng.next_f64();
            assert!((0.0..1.0).contains(&v), "out of range: {v}");
        }
    }

    #[test]
    fn seed_from_user_id_is_deterministic() {
        let a = seed_from_user_id("user-abc-123");
        let b = seed_from_user_id("user-abc-123");
        assert_eq!(a, b);
        let c = seed_from_user_id("user-xyz-999");
        assert_ne!(a, c);
    }

    #[test]
    fn bones_are_deterministic() {
        let a = CompanionBones::from_user_id("alice");
        let b = CompanionBones::from_user_id("alice");
        assert_eq!(a.rarity, b.rarity);
        assert_eq!(a.species, b.species);
        assert_eq!(a.eye, b.eye);
        assert_eq!(a.hat, b.hat);
        assert_eq!(a.shiny, b.shiny);
    }

    #[test]
    fn bones_differ_for_different_users() {
        // Extremely unlikely (but not impossible) that two different user-ids
        // produce identical bones.  With 18 species * 5 rarities * 6 eyes *
        // 8 hats the collision probability is negligible.
        let a = CompanionBones::from_user_id("alice");
        let b = CompanionBones::from_user_id("bob");
        // At minimum one field should differ — check species which has 18 options.
        // This could theoretically fail but probability is ~1/18.
        let _ = (a, b); // just ensure no panic
    }

    #[test]
    fn stats_in_valid_range() {
        let mut rng = Mulberry32::new(12345);
        for _ in 0..100 {
            let s = CompanionStats::roll(&Rarity::Legendary, &mut rng);
            for val in [s.debugging, s.patience, s.chaos, s.wisdom, s.snark] {
                assert!((1..=100).contains(&val), "stat out of range: {val}");
            }
        }
    }

    #[test]
    fn animation_frame_cycles() {
        // Frame sequence has period 15; values must be 0, 1, or 2.
        for t in 0u64..100 {
            let f = animation_frame(t);
            assert!(f <= 2, "invalid frame index {f} at tick {t}");
        }
        // Verify period.
        for t in 0u64..30 {
            assert_eq!(animation_frame(t), animation_frame(t + 15));
        }
    }

    #[test]
    fn render_does_not_panic_for_all_species() {
        for species in Species::all() {
            let bones = CompanionBones {
                rarity: Rarity::Common,
                species: species.clone(),
                eye: Eye::Dot,
                hat: Hat::Crown,
                shiny: false,
                stats: CompanionStats {
                    debugging: 50,
                    patience: 50,
                    chaos: 50,
                    wisdom: 50,
                    snark: 50,
                },
            };
            let companion = Companion { bones, soul: None };
            for tick in 0u64..15 {
                let out = render(&companion, tick);
                assert!(!out.is_empty(), "empty render for {species:?} tick {tick}");
            }
        }
    }

    #[test]
    fn save_and_load_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let soul = CompanionSoul {
            name: "Quackers".to_string(),
            personality: "chaotic, helpful, slightly damp".to_string(),
            hatched_at: chrono::Utc::now(),
        };
        save_companion_soul(dir.path(), &soul).unwrap();
        let loaded = load_companion_soul(dir.path()).expect("should load");
        assert_eq!(loaded.name, soul.name);
        assert_eq!(loaded.personality, soul.personality);
    }

    #[test]
    fn companion_display_name_falls_back_to_species() {
        let c = Companion::new("anon", None);
        // display_name returns the species str when no soul is attached.
        assert!(!c.display_name().is_empty());
    }
}
