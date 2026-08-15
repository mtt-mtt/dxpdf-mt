//! Numbering resolution — flatten abstract + instance + overrides into lookup table.

use std::collections::{HashMap, HashSet};

use crate::model::{
    AbstractNumbering, Alignment, Indentation, LevelSuffix, NumId, NumPicBulletId, NumberFormat,
    NumberingDefinitions, NumberingInstance, NumberingLevelDefinition, RunProperties, StyleId,
};
use crate::render::resolve::locale::Locale;

use super::styles::ResolvedStyle;

/// A resolved numbering level — ready for label generation.
#[derive(Clone, Debug)]
pub struct ResolvedNumberingLevel {
    pub format: NumberFormat,
    pub level_text: String,
    pub start: u32,
    /// §17.9.3: run properties for the numbering symbol (font, color, etc.).
    pub run_properties: Option<RunProperties>,
    /// §17.9.3: paragraph indentation from the numbering level definition.
    /// When present, overrides the paragraph style's indentation.
    pub indentation: Option<Indentation>,
    /// §17.9.23 / §17.3.1.21: effective value supplied by the
    /// numbering level's paragraph properties.
    pub overflow_punct: Option<bool>,
    /// §17.9.7: justification of the numbering symbol (left, center, right).
    pub justification: Option<Alignment>,
    /// §17.9.10: reference to a picture bullet definition.
    pub lvl_pic_bullet_id: Option<NumPicBulletId>,
    /// §17.9.29: separator between the label and the paragraph text.
    pub suffix: LevelSuffix,
    /// §17.9.8: render all level numbers as decimal (legal numbering).
    pub is_legal: bool,
}

/// Resolve numbering definitions into a flat lookup: `NumId` →
/// `Vec<ResolvedNumberingLevel>`.
///
/// Each instance's abstract definition is looked up and level overrides applied.
/// An abstract definition with `w:numStyleLink` first resolves the linked
/// numbering style's `w:numPr/w:numId`; the outer instance remains the lookup
/// and counter identity, and its overrides are applied last.
pub fn resolve_numbering(
    defs: &NumberingDefinitions,
    styles: &HashMap<StyleId, ResolvedStyle>,
) -> HashMap<NumId, Vec<ResolvedNumberingLevel>> {
    let mut result = HashMap::new();
    let mut memo = HashMap::new();

    for num_id in defs.numbering_instances.keys().copied() {
        let levels = resolve_instance(num_id, defs, styles, &mut memo, &mut HashSet::new())
            .unwrap_or_else(|NumberingStyleCycle| resolve_local_instance(num_id, defs));
        result.insert(num_id, levels);
    }

    result
}

/// A cycle is propagated to the top-level caller instead of being converted to
/// a memoized fallback halfway through the chain. Otherwise the levels chosen
/// for a cycle would depend on `HashMap` iteration order.
#[derive(Clone, Copy, Debug)]
struct NumberingStyleCycle;

fn resolve_instance(
    num_id: NumId,
    defs: &NumberingDefinitions,
    styles: &HashMap<StyleId, ResolvedStyle>,
    memo: &mut HashMap<NumId, Vec<ResolvedNumberingLevel>>,
    visiting: &mut HashSet<NumId>,
) -> Result<Vec<ResolvedNumberingLevel>, NumberingStyleCycle> {
    if let Some(levels) = memo.get(&num_id) {
        return Ok(levels.clone());
    }

    if !visiting.insert(num_id) {
        log::warn!(
            "cycle detected while resolving w:numStyleLink at numId={}",
            num_id.value()
        );
        return Err(NumberingStyleCycle);
    }

    let resolved = (|| {
        let Some(instance) = defs.numbering_instances.get(&num_id) else {
            return Ok(Vec::new());
        };
        let Some(abstract_num) = defs.abstract_nums.get(&instance.abstract_num_id) else {
            return Ok(Vec::new());
        };

        let mut levels = match resolve_style_link(abstract_num, defs, styles, memo, visiting)? {
            Some(linked) => linked,
            None => resolve_local_levels(abstract_num),
        };
        apply_level_overrides(&mut levels, instance);
        Ok(levels)
    })();

    visiting.remove(&num_id);
    if let Ok(levels) = &resolved {
        memo.insert(num_id, levels.clone());
    }
    resolved
}

fn resolve_style_link(
    abstract_num: &AbstractNumbering,
    defs: &NumberingDefinitions,
    styles: &HashMap<StyleId, ResolvedStyle>,
    memo: &mut HashMap<NumId, Vec<ResolvedNumberingLevel>>,
    visiting: &mut HashSet<NumId>,
) -> Result<Option<Vec<ResolvedNumberingLevel>>, NumberingStyleCycle> {
    let Some(style_id) = abstract_num.num_style_link.as_ref() else {
        return Ok(None);
    };
    let Some(style) = styles.get(style_id) else {
        log::warn!(
            "w:numStyleLink references missing style '{}'",
            style_id.as_str()
        );
        return Ok(None);
    };
    let Some(numbering) = style.paragraph.numbering else {
        log::warn!(
            "w:numStyleLink style '{}' has no w:numPr/w:numId",
            style_id.as_str()
        );
        return Ok(None);
    };

    // In paragraph properties, numId=0 explicitly removes numbering. It must
    // never become a reference to a concrete instance that happens to use 0.
    if numbering.num_id == 0 {
        return Ok(None);
    }

    let linked_num_id = NumId::new(numbering.num_id);
    let Some(linked_instance) = defs.numbering_instances.get(&linked_num_id) else {
        log::warn!(
            "w:numStyleLink style '{}' references missing numId={}",
            style_id.as_str(),
            numbering.num_id
        );
        return Ok(None);
    };
    if !defs
        .abstract_nums
        .contains_key(&linked_instance.abstract_num_id)
    {
        log::warn!(
            "w:numStyleLink style '{}' references numId={} with missing abstractNumId={}",
            style_id.as_str(),
            numbering.num_id,
            linked_instance.abstract_num_id.value()
        );
        return Ok(None);
    }

    resolve_instance(linked_num_id, defs, styles, memo, visiting).map(Some)
}

fn resolve_local_instance(
    num_id: NumId,
    defs: &NumberingDefinitions,
) -> Vec<ResolvedNumberingLevel> {
    let Some(instance) = defs.numbering_instances.get(&num_id) else {
        return Vec::new();
    };
    let mut levels = defs
        .abstract_nums
        .get(&instance.abstract_num_id)
        .map(resolve_local_levels)
        .unwrap_or_default();
    apply_level_overrides(&mut levels, instance);
    levels
}

fn resolve_local_levels(abstract_num: &AbstractNumbering) -> Vec<ResolvedNumberingLevel> {
    abstract_num.levels.iter().map(resolve_level).collect()
}

fn apply_level_overrides(levels: &mut [ResolvedNumberingLevel], instance: &NumberingInstance) {
    // §17.9.9: a `<w:lvlOverride>` may supply a full replacement `<w:lvl>`
    // and/or a `<w:startOverride>` that restarts the level's counter.
    for ovr in &instance.level_overrides {
        let idx = ovr.level as usize;
        if idx >= levels.len() {
            continue; // override references a level beyond the abstract def
        }
        if let Some(def) = &ovr.definition {
            levels[idx] = resolve_level(def);
        }
        if let Some(start) = ovr.start_override {
            levels[idx].start = start;
        }
    }
}

fn resolve_level(def: &NumberingLevelDefinition) -> ResolvedNumberingLevel {
    ResolvedNumberingLevel {
        format: def.format.unwrap_or(NumberFormat::None),
        level_text: def.level_text.clone(),
        start: def.start.unwrap_or(1),
        run_properties: def.run_properties.clone(),
        indentation: def.indentation,
        overflow_punct: def.overflow_punct,
        justification: def.justification,
        lvl_pic_bullet_id: def.lvl_pic_bullet_id,
        suffix: def.suffix,
        is_legal: def.is_legal,
    }
}

/// §17.9.11: format a list label by expanding the level_text template.
/// `%1` is replaced with the formatted counter for level 0, `%2` for level 1, etc.
/// Returns `None` for `NumberFormat::None`.
pub fn format_list_label(
    levels: &[ResolvedNumberingLevel],
    level: u8,
    counters: &HashMap<(NumId, u8), u32>,
    num_id: NumId,
    locale: Locale,
) -> Option<String> {
    let lvl = levels.get(level as usize)?;
    if lvl.format == NumberFormat::None {
        return None;
    }
    if lvl.format == NumberFormat::Bullet {
        return Some(lvl.level_text.clone());
    }

    // Expand template: %1 → level 0 counter, %2 → level 1 counter, etc.
    // §17.9.8: when this level is "legal", every referenced counter is rendered
    // as decimal regardless of the individual levels' own formats.
    let mut result = lvl.level_text.clone();
    for i in (0..=level).rev() {
        let placeholder = format!("%{}", i + 1);
        if result.contains(&placeholder) {
            let count = counters.get(&(num_id, i)).copied().unwrap_or(1);
            let fmt = if lvl.is_legal {
                NumberFormat::Decimal
            } else {
                levels
                    .get(i as usize)
                    .map(|l| l.format)
                    .unwrap_or(NumberFormat::Decimal)
            };
            let formatted = format_number(count, fmt, locale);
            result = result.replace(&placeholder, &formatted);
        }
    }
    Some(result)
}

/// §17.18.59 `ST_NumberFormat`: render one counter.
///
/// Total over `NumberFormat` — the `_ =>` arm this replaced answered
/// `n.to_string()` for every format it did not implement, which is right for
/// none of them: it printed a digit where `none` asks for nothing, and it hid
/// `cardinalText` and `ordinalText` behind an answer that looked deliberate.
///
/// `locale` decides only the three language-dependent formats; the rest are
/// the same in every language, which is why they take it without using it.
fn format_number(n: u32, fmt: NumberFormat, locale: Locale) -> String {
    match fmt {
        NumberFormat::Decimal => n.to_string(),
        NumberFormat::LowerLetter => to_letter_lower(n),
        NumberFormat::UpperLetter => to_letter_upper(n),
        NumberFormat::LowerRoman => to_roman_lower(n),
        NumberFormat::UpperRoman => to_roman_upper(n),

        // §17.9.27: the three formats that are written differently in every
        // language. This engine spells one of them; for the rest the digits
        // are the honest answer — see `Locale::spells_numbers`.
        NumberFormat::Ordinal if locale.spells_numbers() => format_ordinal(n),
        NumberFormat::CardinalText if locale.spells_numbers() => to_cardinal_text(n),
        NumberFormat::OrdinalText if locale.spells_numbers() => to_ordinal_text(n),
        NumberFormat::Ordinal | NumberFormat::CardinalText | NumberFormat::OrdinalText => {
            n.to_string()
        }

        // §17.18.59: `bullet` renders the level text, `none` renders nothing —
        // neither renders the counter. `format_list_label` returns before
        // reaching either, so these are unreachable in practice; answering with
        // the digit would be wrong if a future caller did reach them.
        NumberFormat::Bullet | NumberFormat::None => String::new(),
    }
}

/// §17.9.27 `cardinalText`: the number in English words, e.g. `1234` →
/// "One Thousand Two Hundred Thirty-Four".
///
/// US English convention, which is what Word writes: tens and units joined by a
/// hyphen, scale groups by a space, and **no** "and" before the final group.
/// Each word capitalised. Unverified against a Word render; recorded here
/// rather than guessed at each call site.
fn to_cardinal_text(n: u32) -> String {
    const UNITS: [&str; 20] = [
        "Zero",
        "One",
        "Two",
        "Three",
        "Four",
        "Five",
        "Six",
        "Seven",
        "Eight",
        "Nine",
        "Ten",
        "Eleven",
        "Twelve",
        "Thirteen",
        "Fourteen",
        "Fifteen",
        "Sixteen",
        "Seventeen",
        "Eighteen",
        "Nineteen",
    ];
    const TENS: [&str; 10] = [
        "", "", "Twenty", "Thirty", "Forty", "Fifty", "Sixty", "Seventy", "Eighty", "Ninety",
    ];
    /// Groups of a thousand, smallest first. `u32::MAX` needs three.
    const SCALES: [&str; 4] = ["", "Thousand", "Million", "Billion"];

    /// 1..=999 — never called with 0, so it never emits a stray "Zero".
    fn under_thousand(n: u32) -> String {
        match n {
            0 => String::new(),
            1..=19 => UNITS[n as usize].to_string(),
            20..=99 => {
                let (tens, unit) = (TENS[(n / 10) as usize], n % 10);
                if unit == 0 {
                    tens.to_string()
                } else {
                    format!("{tens}-{}", UNITS[unit as usize])
                }
            }
            _ => {
                let (hundreds, rest) = (UNITS[(n / 100) as usize], n % 100);
                if rest == 0 {
                    format!("{hundreds} Hundred")
                } else {
                    format!("{hundreds} Hundred {}", under_thousand(rest))
                }
            }
        }
    }

    if n == 0 {
        return UNITS[0].to_string();
    }

    // Split into thousand-groups, then emit largest-first.
    let mut groups = Vec::new();
    let mut rest = n;
    while rest > 0 {
        groups.push(rest % 1000);
        rest /= 1000;
    }
    let mut words = Vec::new();
    for (i, group) in groups.iter().enumerate().rev() {
        if *group == 0 {
            continue;
        }
        let scale = SCALES[i];
        words.push(if scale.is_empty() {
            under_thousand(*group)
        } else {
            format!("{} {scale}", under_thousand(*group))
        });
    }
    words.join(" ")
}

/// §17.9.27 `ordinalText`: the number as an English ordinal in words, e.g.
/// `21` → "Twenty-First".
///
/// Only the **final word** takes the ordinal form — "One Thousand Two Hundred
/// Thirty-Four" becomes "…Thirty-Fourth", not "First Thousandth …" — so this
/// spells the cardinal and rewrites its last word. The separator before that
/// word (space or hyphen) is preserved exactly.
fn to_ordinal_text(n: u32) -> String {
    let cardinal = to_cardinal_text(n);
    // ASCII throughout, so a byte index from `rfind` is a char boundary.
    match cardinal.rfind([' ', '-']) {
        Some(i) => format!("{}{}", &cardinal[..=i], ordinal_word(&cardinal[i + 1..])),
        None => ordinal_word(&cardinal),
    }
}

/// The ordinal form of one English number word.
///
/// The irregulars are listed; everything else — "Four", "Six", "Seven", "Ten",
/// the teens, and the scale words "Hundred"/"Thousand"/"Million"/"Billion" —
/// takes a plain `th`.
fn ordinal_word(word: &str) -> String {
    match word {
        "One" => "First",
        "Two" => "Second",
        "Three" => "Third",
        "Five" => "Fifth",
        "Eight" => "Eighth",
        "Nine" => "Ninth",
        "Twelve" => "Twelfth",
        "Twenty" => "Twentieth",
        "Thirty" => "Thirtieth",
        "Forty" => "Fortieth",
        "Fifty" => "Fiftieth",
        "Sixty" => "Sixtieth",
        "Seventy" => "Seventieth",
        "Eighty" => "Eightieth",
        "Ninety" => "Ninetieth",
        other => return format!("{other}th"),
    }
    .to_string()
}

fn to_letter_lower(n: u32) -> String {
    if n == 0 {
        return String::new();
    }
    // ST_NumberFormat `lowerLetter`: Word repeats the letter on overflow —
    // a…z, then aa, bb, …, zz, aaa (a *repeating* scheme, not bijective
    // base-26). Item 27 is "aa", not "a" again.
    let idx = ((n - 1) % 26) as u8;
    let count = ((n - 1) / 26) as usize + 1;
    std::iter::repeat_n((b'a' + idx) as char, count).collect()
}

fn to_letter_upper(n: u32) -> String {
    to_letter_lower(n).to_uppercase()
}

fn to_roman_lower(mut n: u32) -> String {
    const VALS: [(u32, &str); 13] = [
        (1000, "m"),
        (900, "cm"),
        (500, "d"),
        (400, "cd"),
        (100, "c"),
        (90, "xc"),
        (50, "l"),
        (40, "xl"),
        (10, "x"),
        (9, "ix"),
        (5, "v"),
        (4, "iv"),
        (1, "i"),
    ];
    let mut s = String::new();
    for &(val, sym) in &VALS {
        while n >= val {
            s.push_str(sym);
            n -= val;
        }
    }
    s
}

fn to_roman_upper(n: u32) -> String {
    to_roman_lower(n).to_uppercase()
}

fn format_ordinal(n: u32) -> String {
    let suffix = match n % 100 {
        11..=13 => "th",
        _ => match n % 10 {
            1 => "st",
            2 => "nd",
            3 => "rd",
            _ => "th",
        },
    };
    format!("{n}{suffix}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::*;

    fn make_defs(
        abstracts: Vec<(AbstractNumId, Vec<NumberingLevelDefinition>)>,
        instances: Vec<(NumId, AbstractNumId, Vec<NumberingLevelDefinition>)>,
    ) -> NumberingDefinitions {
        NumberingDefinitions {
            abstract_nums: abstracts
                .into_iter()
                .map(|(id, levels)| {
                    (
                        id,
                        AbstractNumbering {
                            num_style_link: None,
                            levels,
                        },
                    )
                })
                .collect(),
            numbering_instances: instances
                .into_iter()
                .map(|(num_id, abstract_id, overrides)| {
                    (
                        num_id,
                        NumberingInstance {
                            abstract_num_id: abstract_id,
                            level_overrides: overrides
                                .into_iter()
                                .map(|def| crate::model::LevelOverride {
                                    level: def.level,
                                    start_override: None,
                                    definition: Some(def),
                                })
                                .collect(),
                        },
                    )
                })
                .collect(),
            pic_bullets: HashMap::new(),
        }
    }

    fn level(lvl: u8, fmt: NumberFormat, text: &str, start: u32) -> NumberingLevelDefinition {
        NumberingLevelDefinition {
            level: lvl,
            format: Some(fmt),
            level_text: text.to_string(),
            start: Some(start),
            justification: None,
            indentation: None,
            overflow_punct: None,
            run_properties: None,
            lvl_pic_bullet_id: None,
            suffix: LevelSuffix::default(),
            is_legal: false,
        }
    }

    fn numbering_style(num_id: i64) -> ResolvedStyle {
        let mut paragraph = ParagraphProperties::default();
        paragraph.numbering = Some(NumberingReference { num_id, level: 0 });
        ResolvedStyle {
            paragraph,
            run: RunProperties::default(),
            table: None,
            table_style_overrides: Vec::new(),
            is_toc_entry: false,
        }
    }

    fn instance(abstract_num_id: i64) -> NumberingInstance {
        NumberingInstance {
            abstract_num_id: AbstractNumId::new(abstract_num_id),
            level_overrides: Vec::new(),
        }
    }

    #[test]
    fn single_instance_resolves_from_abstract() {
        let defs = make_defs(
            vec![(
                AbstractNumId::new(0),
                vec![level(0, NumberFormat::Decimal, "%1.", 1)],
            )],
            vec![(NumId::new(1), AbstractNumId::new(0), vec![])],
        );

        let resolved = resolve_numbering(&defs, &HashMap::new());
        let levels = resolved.get(&NumId::new(1)).unwrap();

        assert_eq!(levels.len(), 1);
        assert_eq!(levels[0].format, NumberFormat::Decimal);
        assert_eq!(levels[0].level_text, "%1.");
        assert_eq!(levels[0].start, 1);
    }

    #[test]
    fn level_override_replaces_abstract_level() {
        let defs = make_defs(
            vec![(
                AbstractNumId::new(0),
                vec![
                    level(0, NumberFormat::Decimal, "%1.", 1),
                    level(1, NumberFormat::LowerLetter, "%2)", 1),
                ],
            )],
            vec![(
                NumId::new(1),
                AbstractNumId::new(0),
                // Override level 0 to bullet
                vec![level(0, NumberFormat::Bullet, "•", 1)],
            )],
        );

        let resolved = resolve_numbering(&defs, &HashMap::new());
        let levels = resolved.get(&NumId::new(1)).unwrap();

        assert_eq!(levels.len(), 2);
        assert_eq!(levels[0].format, NumberFormat::Bullet, "overridden");
        assert_eq!(levels[0].level_text, "•");
        assert_eq!(levels[1].format, NumberFormat::LowerLetter, "from abstract");
    }

    #[test]
    fn missing_abstract_produces_empty_levels() {
        let defs = make_defs(
            vec![],
            vec![(NumId::new(1), AbstractNumId::new(99), vec![])],
        );

        let resolved = resolve_numbering(&defs, &HashMap::new());
        let levels = resolved.get(&NumId::new(1)).unwrap();
        assert!(levels.is_empty());
    }

    #[test]
    fn multiple_instances_same_abstract() {
        let defs = make_defs(
            vec![(
                AbstractNumId::new(0),
                vec![level(0, NumberFormat::Decimal, "%1.", 1)],
            )],
            vec![
                (NumId::new(1), AbstractNumId::new(0), vec![]),
                (
                    NumId::new(2),
                    AbstractNumId::new(0),
                    vec![level(0, NumberFormat::Decimal, "%1)", 10)],
                ),
            ],
        );

        let resolved = resolve_numbering(&defs, &HashMap::new());

        let l1 = resolved.get(&NumId::new(1)).unwrap();
        assert_eq!(l1[0].level_text, "%1.");
        assert_eq!(l1[0].start, 1);

        let l2 = resolved.get(&NumId::new(2)).unwrap();
        assert_eq!(l2[0].level_text, "%1)");
        assert_eq!(l2[0].start, 10);
    }

    #[test]
    fn start_override_restarts_level_counter() {
        // A startOverride-only lvlOverride resets the level's start value.
        let mut abstract_nums = HashMap::new();
        abstract_nums.insert(
            AbstractNumId::new(0),
            AbstractNumbering {
                num_style_link: None,
                levels: vec![level(0, NumberFormat::Decimal, "%1.", 1)],
            },
        );
        let mut numbering_instances = HashMap::new();
        numbering_instances.insert(
            NumId::new(1),
            NumberingInstance {
                abstract_num_id: AbstractNumId::new(0),
                level_overrides: vec![crate::model::LevelOverride {
                    level: 0,
                    start_override: Some(5),
                    definition: None,
                }],
            },
        );
        let defs = NumberingDefinitions {
            abstract_nums,
            numbering_instances,
            pic_bullets: HashMap::new(),
        };
        let resolved = resolve_numbering(&defs, &HashMap::new());
        assert_eq!(resolved[&NumId::new(1)][0].start, 5);
    }

    #[test]
    fn num_style_link_resolves_recursively_and_keeps_outer_identity_and_overrides() {
        let mut defs = NumberingDefinitions::default();
        defs.abstract_nums.insert(
            AbstractNumId::new(1),
            AbstractNumbering {
                num_style_link: Some(StyleId::new("FirstLink")),
                levels: Vec::new(),
            },
        );
        defs.abstract_nums.insert(
            AbstractNumId::new(2),
            AbstractNumbering {
                num_style_link: Some(StyleId::new("SecondLink")),
                levels: Vec::new(),
            },
        );
        defs.abstract_nums.insert(
            AbstractNumId::new(3),
            AbstractNumbering {
                num_style_link: None,
                levels: vec![level(0, NumberFormat::Decimal, "第 %1 章", 1)],
            },
        );

        let mut outer = instance(1);
        outer.level_overrides.push(LevelOverride {
            level: 0,
            start_override: Some(7),
            definition: None,
        });
        defs.numbering_instances.insert(NumId::new(50), outer);
        defs.numbering_instances.insert(NumId::new(14), instance(2));
        defs.numbering_instances.insert(NumId::new(24), instance(3));

        let styles = HashMap::from([
            (StyleId::new("FirstLink"), numbering_style(14)),
            (StyleId::new("SecondLink"), numbering_style(24)),
        ]);
        let resolved = resolve_numbering(&defs, &styles);
        let outer_levels = &resolved[&NumId::new(50)];
        assert_eq!(outer_levels[0].level_text, "第 %1 章");
        assert_eq!(outer_levels[0].start, 7, "outer override applies last");

        let counters = HashMap::from([((NumId::new(50), 0), 3), ((NumId::new(24), 0), 9)]);
        assert_eq!(
            format_list_label(outer_levels, 0, &counters, NumId::new(50), Locale::English),
            Some("第 3 章".to_string()),
            "the linked levels use the outer numId counter"
        );
    }

    #[test]
    fn num_style_link_never_follows_num_id_zero_sentinel() {
        let mut defs = NumberingDefinitions::default();
        defs.abstract_nums.insert(
            AbstractNumId::new(1),
            AbstractNumbering {
                num_style_link: Some(StyleId::new("DisabledNumbering")),
                levels: vec![level(0, NumberFormat::Decimal, "local-%1", 1)],
            },
        );
        defs.abstract_nums.insert(
            AbstractNumId::new(2),
            AbstractNumbering {
                num_style_link: None,
                levels: vec![level(0, NumberFormat::Decimal, "wrong-%1", 1)],
            },
        );
        defs.numbering_instances.insert(NumId::new(50), instance(1));
        // A malformed producer may define concrete numId=0. The style-level
        // zero still means "numbering off" and must not bind to this instance.
        defs.numbering_instances.insert(NumId::new(0), instance(2));

        let styles = HashMap::from([(StyleId::new("DisabledNumbering"), numbering_style(0))]);
        let resolved = resolve_numbering(&defs, &styles);
        assert_eq!(resolved[&NumId::new(50)][0].level_text, "local-%1");
    }

    #[test]
    fn invalid_num_style_links_fall_back_to_local_levels() {
        let mut defs = NumberingDefinitions::default();
        defs.abstract_nums.insert(
            AbstractNumId::new(1),
            AbstractNumbering {
                num_style_link: Some(StyleId::new("MissingStyle")),
                levels: vec![level(0, NumberFormat::Decimal, "missing-style-%1", 1)],
            },
        );
        defs.abstract_nums.insert(
            AbstractNumId::new(2),
            AbstractNumbering {
                num_style_link: Some(StyleId::new("DanglingNumId")),
                levels: vec![level(0, NumberFormat::Decimal, "missing-num-%1", 1)],
            },
        );
        defs.abstract_nums.insert(
            AbstractNumId::new(3),
            AbstractNumbering {
                num_style_link: Some(StyleId::new("MissingAbstract")),
                levels: vec![level(0, NumberFormat::Decimal, "missing-abstract-%1", 1)],
            },
        );
        defs.numbering_instances.insert(NumId::new(1), instance(1));
        defs.numbering_instances.insert(NumId::new(2), instance(2));
        defs.numbering_instances.insert(NumId::new(3), instance(3));
        defs.numbering_instances
            .insert(NumId::new(998), instance(999));

        let styles = HashMap::from([
            (StyleId::new("DanglingNumId"), numbering_style(999)),
            (StyleId::new("MissingAbstract"), numbering_style(998)),
        ]);
        let resolved = resolve_numbering(&defs, &styles);
        assert_eq!(resolved[&NumId::new(1)][0].level_text, "missing-style-%1");
        assert_eq!(resolved[&NumId::new(2)][0].level_text, "missing-num-%1");
        assert_eq!(
            resolved[&NumId::new(3)][0].level_text,
            "missing-abstract-%1"
        );
    }

    #[test]
    fn num_style_link_cycle_falls_back_per_outer_instance_without_memoizing() {
        let mut defs = NumberingDefinitions::default();
        defs.abstract_nums.insert(
            AbstractNumId::new(1),
            AbstractNumbering {
                num_style_link: Some(StyleId::new("ToTwo")),
                levels: vec![level(0, NumberFormat::Decimal, "one-%1", 1)],
            },
        );
        defs.abstract_nums.insert(
            AbstractNumId::new(2),
            AbstractNumbering {
                num_style_link: Some(StyleId::new("ToOne")),
                levels: vec![level(0, NumberFormat::Decimal, "two-%1", 1)],
            },
        );
        defs.numbering_instances.insert(NumId::new(1), instance(1));
        defs.numbering_instances.insert(NumId::new(2), instance(2));
        let styles = HashMap::from([
            (StyleId::new("ToTwo"), numbering_style(2)),
            (StyleId::new("ToOne"), numbering_style(1)),
        ]);

        let resolved = resolve_numbering(&defs, &styles);
        assert_eq!(resolved[&NumId::new(1)][0].level_text, "one-%1");
        assert_eq!(resolved[&NumId::new(2)][0].level_text, "two-%1");
    }

    #[test]
    fn legal_numbering_renders_all_levels_decimal() {
        let levels = vec![
            ResolvedNumberingLevel {
                format: NumberFormat::UpperRoman,
                level_text: "%1".to_string(),
                start: 1,
                run_properties: None,
                indentation: None,
                overflow_punct: None,
                justification: None,
                lvl_pic_bullet_id: None,
                suffix: LevelSuffix::default(),
                is_legal: false,
            },
            ResolvedNumberingLevel {
                format: NumberFormat::LowerLetter,
                level_text: "%1.%2".to_string(),
                start: 1,
                run_properties: None,
                indentation: None,
                overflow_punct: None,
                justification: None,
                lvl_pic_bullet_id: None,
                suffix: LevelSuffix::default(),
                is_legal: true,
            },
        ];
        let mut counters = HashMap::new();
        counters.insert((NumId::new(1), 0u8), 3u32); // would be "III" un-legal
        counters.insert((NumId::new(1), 1u8), 2u32); // would be "b" un-legal
        let label =
            format_list_label(&levels, 1, &counters, NumId::new(1), Locale::English).unwrap();
        assert_eq!(
            label, "3.2",
            "isLgl forces decimal for every referenced level"
        );
    }

    #[test]
    fn level_with_no_format_defaults_to_none() {
        let defs = make_defs(
            vec![(
                AbstractNumId::new(0),
                vec![NumberingLevelDefinition {
                    level: 0,
                    format: None,
                    level_text: String::new(),
                    start: None,
                    justification: None,
                    indentation: None,
                    overflow_punct: None,
                    run_properties: None,
                    lvl_pic_bullet_id: None,
                    suffix: LevelSuffix::default(),
                    is_legal: false,
                }],
            )],
            vec![(NumId::new(1), AbstractNumId::new(0), vec![])],
        );

        let resolved = resolve_numbering(&defs, &HashMap::new());
        let levels = resolved.get(&NumId::new(1)).unwrap();
        assert_eq!(levels[0].format, NumberFormat::None);
        assert_eq!(levels[0].start, 1);
    }

    #[test]
    fn lower_letter_repeats_on_overflow() {
        // §17.9 lowerLetter: a…z then aa, bb, … (repeating, not bijective).
        assert_eq!(
            format_number(1, NumberFormat::LowerLetter, Locale::English),
            "a"
        );
        assert_eq!(
            format_number(26, NumberFormat::LowerLetter, Locale::English),
            "z"
        );
        assert_eq!(
            format_number(27, NumberFormat::LowerLetter, Locale::English),
            "aa"
        );
        assert_eq!(
            format_number(28, NumberFormat::LowerLetter, Locale::English),
            "bb"
        );
        assert_eq!(
            format_number(52, NumberFormat::LowerLetter, Locale::English),
            "zz"
        );
        assert_eq!(
            format_number(53, NumberFormat::LowerLetter, Locale::English),
            "aaa"
        );
    }

    #[test]
    fn upper_letter_matches_lower_uppercased() {
        assert_eq!(
            format_number(27, NumberFormat::UpperLetter, Locale::English),
            "AA"
        );
    }

    #[test]
    fn roman_and_ordinal_formats() {
        assert_eq!(
            format_number(4, NumberFormat::LowerRoman, Locale::English),
            "iv"
        );
        assert_eq!(
            format_number(2026, NumberFormat::UpperRoman, Locale::English),
            "MMXXVI"
        );
        assert_eq!(
            format_number(1, NumberFormat::Ordinal, Locale::English),
            "1st"
        );
        assert_eq!(
            format_number(2, NumberFormat::Ordinal, Locale::English),
            "2nd"
        );
        assert_eq!(
            format_number(11, NumberFormat::Ordinal, Locale::English),
            "11th"
        );
        assert_eq!(
            format_number(23, NumberFormat::Ordinal, Locale::English),
            "23rd"
        );
        assert_eq!(
            format_number(111, NumberFormat::Ordinal, Locale::English),
            "111th"
        );
    }

    // ── §17.9.27 number words ────────────────────────────────────────────

    #[test]
    fn cardinal_text_spells_each_decade_boundary() {
        for (n, want) in [
            (0, "Zero"),
            (1, "One"),
            (12, "Twelve"),
            (19, "Nineteen"),
            (20, "Twenty"),
            (21, "Twenty-One"),
            (99, "Ninety-Nine"),
            (100, "One Hundred"),
            (101, "One Hundred One"),
            (115, "One Hundred Fifteen"),
            (999, "Nine Hundred Ninety-Nine"),
        ] {
            assert_eq!(to_cardinal_text(n), want, "{n}");
        }
    }

    /// A zero group is skipped rather than spelled: 1,000,007 has no "Thousand"
    /// in it at all. Getting this wrong yields "One Million Zero Thousand …".
    #[test]
    fn cardinal_text_skips_empty_scale_groups() {
        assert_eq!(to_cardinal_text(1_000), "One Thousand");
        assert_eq!(to_cardinal_text(1_000_007), "One Million Seven");
        assert_eq!(
            to_cardinal_text(1_234_567),
            "One Million Two Hundred Thirty-Four Thousand Five Hundred Sixty-Seven",
        );
    }

    /// The largest counter the type admits, so the scale table cannot run off
    /// its end.
    #[test]
    fn cardinal_text_spells_the_whole_u32_range() {
        assert_eq!(
            to_cardinal_text(u32::MAX),
            "Four Billion Two Hundred Ninety-Four Million Nine Hundred Sixty-Seven \
             Thousand Two Hundred Ninety-Five",
        );
    }

    /// §17.9.27 `ordinalText` changes the **last word only**, and the irregular
    /// forms are where a naive `+ "th"` breaks.
    #[test]
    fn ordinal_text_rewrites_only_the_final_word() {
        for (n, want) in [
            (1, "First"),
            (2, "Second"),
            (3, "Third"),
            (4, "Fourth"),
            (5, "Fifth"),
            (8, "Eighth"),
            (9, "Ninth"),
            (12, "Twelfth"),
            (13, "Thirteenth"),
            (20, "Twentieth"),
            (21, "Twenty-First"),
            (40, "Fortieth"),
            (100, "One Hundredth"),
            (101, "One Hundred First"),
            (1_000, "One Thousandth"),
            (1_021, "One Thousand Twenty-First"),
        ] {
            assert_eq!(to_ordinal_text(n), want, "{n}");
        }
    }

    /// §17.18.59: neither `bullet` nor `none` renders the counter. The `_ =>`
    /// arm this replaced printed the digit for both.
    #[test]
    fn formats_that_render_no_counter_render_nothing() {
        assert_eq!(format_number(7, NumberFormat::Bullet, Locale::English), "");
        assert_eq!(format_number(7, NumberFormat::None, Locale::English), "");
    }

    /// A language whose number words this engine cannot spell gets the digits —
    /// for all three text formats, not just the one that was implemented.
    #[test]
    fn a_non_spelling_locale_gets_digits_for_every_text_format() {
        for fmt in [
            NumberFormat::Ordinal,
            NumberFormat::CardinalText,
            NumberFormat::OrdinalText,
        ] {
            assert_eq!(format_number(3, fmt, Locale::CommaDecimal), "3", "{fmt:?}");
            assert_eq!(format_number(3, fmt, Locale::PointDecimal), "3", "{fmt:?}");
        }
    }

    /// …and the formats that are the same in every language are untouched by it.
    #[test]
    fn language_independent_formats_ignore_the_locale() {
        for locale in [
            Locale::English,
            Locale::CommaDecimal,
            Locale::PointDecimal,
            Locale::Unrecognised,
        ] {
            assert_eq!(format_number(4, NumberFormat::Decimal, locale), "4");
            assert_eq!(format_number(4, NumberFormat::LowerRoman, locale), "iv");
            assert_eq!(format_number(4, NumberFormat::UpperLetter, locale), "D");
        }
    }
}
