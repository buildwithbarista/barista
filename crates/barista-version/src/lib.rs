//! Maven version comparison and ordering.
//!
//! This crate is a Rust port of Apache Maven's `ComparableVersion`
//! algorithm — the canonical version-ordering routine used by every
//! Maven 3 and Maven 4 build to decide which artifact version is
//! "newer" than another.
//!
//! Reference (Apache Maven, Apache License 2.0):
//! <https://github.com/apache/maven/blob/master/compat/maven-artifact/src/main/java/org/apache/maven/artifact/versioning/ComparableVersion.java>
//!
//! # Highlights of the algorithm
//!
//! * Both `.` and `-` are accepted as separators, with subtly different
//!   semantics — a `-` opens a *nested list* whose ordering is "less
//!   important than" the surrounding integers.
//! * Transitions between digits and non-digits are implicit separators,
//!   so `1.0alpha1` parses the same as `1.0-alpha-1`.
//! * Single-letter qualifiers `a`, `b`, `m` followed immediately by a
//!   digit are expanded to `alpha`, `beta`, `milestone`.
//! * Qualifier comparison is case-insensitive and uses a fixed
//!   ordering: `alpha` < `beta` < `milestone` < `rc`/`cr` <
//!   `snapshot` < release (`""`, `ga`, `final`, `release`) < `sp`.
//!   Unknown qualifiers sort lexicographically AFTER `sp`.
//! * Trailing zero items are stripped during normalization, so
//!   `"1"`, `"1.0"`, and `"1.0.0"` compare equal.
//!
//! # Usage
//!
//! ```
//! use barista_version::Version;
//!
//! let a: Version = "1.0-alpha-1".parse().unwrap();
//! let b: Version = "1.0".parse().unwrap();
//! assert!(a < b);
//!
//! // Padding-zero equality
//! let one: Version = "1".parse().unwrap();
//! let one_oh: Version = "1.0.0".parse().unwrap();
//! assert_eq!(one, one_oh);
//! ```

#![forbid(unsafe_code)]

use std::cmp::Ordering;
use std::convert::Infallible;
use std::fmt;
use std::hash::{Hash, Hasher};
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

// ---------------------------------------------------------------------------
// Qualifier ordering table
// ---------------------------------------------------------------------------

/// Known qualifiers, in ascending order. Index in this table is the
/// "comparable ordinal" for the qualifier. The empty string `""`
/// represents the release version itself; anything before it is a
/// pre-release qualifier, anything after is post-release.
const QUALIFIERS: &[&str] = &["alpha", "beta", "milestone", "rc", "snapshot", "", "sp"];

/// Qualifier strings that should be treated identically to the empty
/// (release) qualifier — `1ga`, `1release`, and `1final` all sort the
/// same as `1`, but are not `equals` to it (their canonical form differs).
const RELEASE_QUALIFIERS: &[&str] = &["ga", "final", "release"];

/// Comparable-qualifier index that represents the release version itself.
/// Stored as a string to play nicely with the prefixed lexical fallback
/// used for unknown qualifiers.
const RELEASE_INDEX: &str = "5";

/// Apply Maven's `ALIASES` table — currently only `cr → rc`.
fn alias(q: &str) -> &str {
    match q {
        "cr" => "rc",
        other => other,
    }
}

/// Return a string that, when compared lexically against another such
/// string, yields the correct qualifier ordering.
///
/// Known qualifiers map to the stringified `QUALIFIERS` index (single
/// digit, so lexical compare == numeric compare). Unknown qualifiers
/// map to `"7-<qualifier>"` — prefixed with `QUALIFIERS.len()` so they
/// always sort after every known qualifier — and then compared
/// lexically against each other.
///
/// `RELEASE_QUALIFIERS` (`ga`, `final`, `release`) collapse to the
/// release index, matching Maven's behaviour where `1ga` sorts the same
/// as `1` ordinally.
fn comparable_qualifier(qualifier: &str) -> String {
    if RELEASE_QUALIFIERS.contains(&qualifier) {
        return RELEASE_INDEX.to_owned();
    }
    if let Some(idx) = QUALIFIERS.iter().position(|q| *q == qualifier) {
        return idx.to_string();
    }
    format!("{}-{}", QUALIFIERS.len(), qualifier)
}

// ---------------------------------------------------------------------------
// Item
// ---------------------------------------------------------------------------

/// One parsed token from a version string.
///
/// This mirrors the inner-class hierarchy in Maven's Java implementation
/// — `IntItem` / `LongItem` / `BigIntegerItem` collapse here into a
/// single canonical-numeric form (a leading-zero-stripped digit string),
/// but `StringItem`, `CombinationItem`, and `ListItem` are preserved as
/// distinct variants because they carry different ordering semantics.
#[derive(Debug, Clone)]
enum Item {
    /// A numeric token. The string is the digit sequence with all
    /// leading zeros stripped (or `"0"` for the literal zero). Storing
    /// it as a string lets us compare arbitrarily-large numbers without
    /// pulling in a bignum dependency: compare lengths first, then
    /// lexically.
    Integer(String),
    /// A qualifier token. Already lowercased and alias-resolved at
    /// construction time.
    Str(String),
    /// A `<qualifier><digits>` fused token — for example, the `b2` in
    /// `2.1b2`. Maven distinguishes this so that `X1 > X` (e.g.
    /// `1.0.0alpha1 > 1.0.0alpha`).
    Combination(Box<Item>, Box<Item>),
    /// A list of items. The top-level container is a list, and each
    /// `-` followed by non-empty content opens a nested list.
    List(Vec<Item>),
}

impl Item {
    /// `true` if this item is the "zero of its kind" — used during
    /// normalization to strip trailing padding.
    fn is_null(&self) -> bool {
        match self {
            Item::Integer(s) => s == "0",
            Item::Str(s) => s.is_empty(),
            Item::Combination(_, _) => false,
            Item::List(v) => v.is_empty(),
        }
    }

    /// Construct a numeric item from a digit string, stripping any
    /// leading zeros so that `"007"` and `"7"` compare equal.
    fn integer(buf: &str) -> Item {
        Item::Integer(strip_leading_zeros(buf))
    }

    /// Construct a qualifier item from a raw string. If
    /// `followed_by_digit` is set and the value is exactly one letter,
    /// it is expanded: `a → alpha`, `b → beta`, `m → milestone`.
    /// Aliases (`cr → rc`) are then applied. This is the equivalent of
    /// Maven's `new StringItem(value, followedByDigit)`.
    fn string(value: &str, followed_by_digit: bool) -> Item {
        let mut v: String = value.to_owned();
        if followed_by_digit && v.chars().count() == 1 {
            v = match v.as_str() {
                "a" => "alpha".to_owned(),
                "b" => "beta".to_owned(),
                "m" => "milestone".to_owned(),
                _ => v,
            };
        }
        Item::Str(alias(&v).to_owned())
    }

    /// Construct a `Combination` from a `<letters><digits>` substring —
    /// the `b2` of `2.1b2`. The Java side strips embedded hyphens
    /// before splitting.
    fn combination(value: &str) -> Item {
        let cleaned: String = value.chars().filter(|c| *c != '-').collect();
        let split = cleaned
            .char_indices()
            .find(|(_, c)| c.is_ascii_digit())
            .map(|(i, _)| i)
            .unwrap_or(cleaned.len());
        let string_part = Item::string(&cleaned[..split], true);
        let digit_part = parse_item(false, true, &cleaned[split..]);
        Item::Combination(Box::new(string_part), Box::new(digit_part))
    }
}

/// Strip ASCII leading zeros, but leave a single `"0"` if the input is
/// empty or all zeros. Matches Maven's `stripLeadingZeroes`.
fn strip_leading_zeros(buf: &str) -> String {
    if buf.is_empty() {
        return "0".to_owned();
    }
    let trimmed: &str = buf.trim_start_matches('0');
    if trimmed.is_empty() {
        "0".to_owned()
    } else {
        trimmed.to_owned()
    }
}

/// Construct an `Item` from a substring of the original version, given
/// the parser-state flags collected while walking it.
fn parse_item(is_combination: bool, is_digit: bool, buf: &str) -> Item {
    if is_combination {
        Item::combination(&buf.replace('-', ""))
    } else if is_digit {
        Item::integer(buf)
    } else {
        Item::string(buf, false)
    }
}

// ---------------------------------------------------------------------------
// Item comparison
// ---------------------------------------------------------------------------

/// Compare two numeric strings that have already been stripped of
/// leading zeros: shorter is smaller; same-length falls back to
/// lexical compare (which, for digit strings, is identical to numeric
/// compare).
fn cmp_numeric(a: &str, b: &str) -> Ordering {
    a.len().cmp(&b.len()).then_with(|| a.cmp(b))
}

impl Item {
    /// Compare `self` against `other`. The `other = None` form is used
    /// when one side has "run out" of items during a list walk — this
    /// is where the padding-zero semantics live (a missing trailing
    /// item compares as if it were the zero of the appropriate kind).
    fn compare_to(&self, other: Option<&Item>) -> Ordering {
        let Some(other) = other else {
            return match self {
                Item::Integer(s) => {
                    if s == "0" {
                        Ordering::Equal
                    } else {
                        Ordering::Greater
                    }
                }
                Item::Str(s) => comparable_qualifier(s).as_str().cmp(RELEASE_INDEX),
                Item::Combination(string_part, _) => string_part.compare_to(None),
                Item::List(v) => {
                    if v.is_empty() {
                        Ordering::Equal
                    } else {
                        // MNG-6964: every element must vanish for the
                        // list to equal padding. The first non-equal
                        // element decides ordering.
                        for it in v {
                            let r = it.compare_to(None);
                            if r != Ordering::Equal {
                                return r;
                            }
                        }
                        Ordering::Equal
                    }
                }
            };
        };

        match self {
            Item::Integer(a) => match other {
                Item::Integer(b) => cmp_numeric(a, b),
                // 1.x > 1.any-qualifier; 1.1 > 1-sp; 1.1 > 1-1
                Item::Str(_) | Item::Combination(_, _) | Item::List(_) => Ordering::Greater,
            },
            Item::Str(a) => match other {
                Item::Integer(_) => Ordering::Less,
                Item::Str(b) => comparable_qualifier(a).cmp(&comparable_qualifier(b)),
                Item::Combination(other_str, _) => {
                    let result = self.compare_to(Some(other_str));
                    if result == Ordering::Equal {
                        Ordering::Less
                    } else {
                        result
                    }
                }
                Item::List(_) => Ordering::Less,
            },
            Item::Combination(string_part, digit_part) => match other {
                Item::Integer(_) => Ordering::Less,
                Item::Str(_) => {
                    let result = string_part.compare_to(Some(other));
                    if result == Ordering::Equal {
                        // X1 > X
                        Ordering::Greater
                    } else {
                        result
                    }
                }
                Item::List(_) => Ordering::Less,
                Item::Combination(other_str, other_digit) => {
                    let result = string_part.compare_to(Some(other_str));
                    if result == Ordering::Equal {
                        digit_part.compare_to(Some(other_digit))
                    } else {
                        result
                    }
                }
            },
            Item::List(items) => match other {
                Item::Integer(_) => Ordering::Less,
                Item::Str(_) | Item::Combination(_, _) => Ordering::Greater,
                Item::List(other_items) => compare_lists(items, other_items),
            },
        }
    }
}

/// Walk two item lists in parallel, applying padding-zero semantics
/// when one side runs out before the other.
fn compare_lists(left: &[Item], right: &[Item]) -> Ordering {
    let n = left.len().max(right.len());
    for i in 0..n {
        let l = left.get(i);
        let r = right.get(i);
        let result = match (l, r) {
            (Some(li), _) => li.compare_to(r),
            (None, Some(ri)) => ri.compare_to(None).reverse(),
            (None, None) => Ordering::Equal,
        };
        if result != Ordering::Equal {
            return result;
        }
    }
    Ordering::Equal
}

// ---------------------------------------------------------------------------
// Normalization (trailing-padding stripping)
// ---------------------------------------------------------------------------

/// Strip "null" trailing items from a list, recursively. This is the
/// step that makes `"1.0.0" == "1"` — the trailing `0`s vanish, leaving
/// both versions with the same canonical token list.
///
/// The rules (transcribed from Maven's `ListItem.normalize`):
///
/// 1. Walk back-to-front.
/// 2. If the last item is "null" (the zero of its kind), drop it.
/// 3. If a non-trailing item is "null" *and* its successor is a string,
///    drop it — `1.0-alpha` collapses to `1-alpha`.
/// 4. If a non-trailing item is "null" *and* its successor is a list
///    whose first element is a combination or string, drop it —
///    handles `1.0.0.alpha1` → `1.alpha1` shape collapses.
fn normalize(items: &mut Vec<Item>) {
    for it in items.iter_mut() {
        if let Item::List(inner) = it {
            normalize(inner);
        }
    }

    let mut i: isize = items.len() as isize - 1;
    while i >= 0 {
        let idx = i as usize;
        let drop = if items[idx].is_null() {
            if idx == items.len() - 1 {
                true
            } else {
                match &items[idx + 1] {
                    Item::Str(_) => true,
                    Item::List(inner) => matches!(
                        inner.first(),
                        Some(Item::Combination(_, _)) | Some(Item::Str(_))
                    ),
                    _ => false,
                }
            }
        } else {
            false
        };
        if drop {
            items.remove(idx);
        }
        i -= 1;
    }
}

// ---------------------------------------------------------------------------
// Parser
// ---------------------------------------------------------------------------

/// Tokenize a version string into a top-level item list.
///
/// This is a direct port of Maven's `parseVersion`. ASCII-digit
/// detection is preserved as-is — non-ASCII "digits" like `٨` are *not*
/// treated as digits, matching the `testNonAsciiDigits` test.
fn parse(version_in: &str) -> Vec<Item> {
    // Lowercase for case-insensitive qualifier matching.
    let version: String = version_in.to_lowercase();
    let bytes: &[u8] = version.as_bytes();

    // Stack of partially-built lists; the deepest is the one currently
    // receiving items.
    let mut stack: Vec<Vec<Item>> = vec![Vec::new()];

    let mut is_digit = false;
    let mut is_combination = false;
    let mut start = 0usize;

    let mut i = 0usize;
    while i < bytes.len() {
        let c = bytes[i];
        if c == b'.' {
            if i == start {
                push_item(&mut stack, Item::Integer("0".to_owned()));
            } else {
                let slice = &version[start..i];
                push_item(&mut stack, parse_item(is_combination, is_digit, slice));
            }
            is_combination = false;
            start = i + 1;
            i += 1;
        } else if c == b'-' {
            if i == start {
                push_item(&mut stack, Item::Integer("0".to_owned()));
            } else {
                // X-1 (letter then dash then digit) is treated as X1
                // (a combination), so don't break the token yet.
                if !is_digit && i != bytes.len() - 1 {
                    let c1 = bytes[i + 1];
                    if c1.is_ascii_digit() {
                        is_combination = true;
                        i += 1;
                        continue;
                    }
                }
                let slice = &version[start..i];
                push_item(&mut stack, parse_item(is_combination, is_digit, slice));
            }
            start = i + 1;
            if !top_list(&stack).is_empty() {
                stack.push(Vec::new());
            }
            is_combination = false;
            i += 1;
        } else if c.is_ascii_digit() {
            if !is_digit && i > start {
                // letter→digit boundary inside a token → CombinationItem.
                is_combination = true;
                if !top_list(&stack).is_empty() {
                    stack.push(Vec::new());
                }
            }
            is_digit = true;
            i += 1;
        } else {
            // Any non-ASCII-digit character (letters, non-ASCII, etc.).
            if is_digit && i > start {
                // digit→letter boundary: flush digits and open a nested
                // list for the upcoming letter token.
                let slice = &version[start..i];
                push_item(&mut stack, parse_item(is_combination, true, slice));
                start = i;
                stack.push(Vec::new());
                is_combination = false;
            }
            is_digit = false;
            // Step over the full UTF-8 sequence.
            i += utf8_step(c);
        }
    }

    if bytes.len() > start {
        // 1.0.0.X1 < 1.0.0-X2: a trailing string-tail of the top list
        // opens its own nested list so the canonical form differs.
        if !is_digit && !top_list(&stack).is_empty() {
            stack.push(Vec::new());
        }
        let slice = &version[start..];
        push_item(&mut stack, parse_item(is_combination, is_digit, slice));
    }

    // Collapse the stack into nested ListItems, normalizing each level
    // on the way out.
    while stack.len() > 1 {
        let mut top_frame = stack.pop().unwrap();
        normalize(&mut top_frame);
        let parent = stack.last_mut().unwrap();
        parent.push(Item::List(top_frame));
    }

    let mut root = stack.pop().unwrap();
    normalize(&mut root);
    root
}

/// Number of bytes consumed by the UTF-8 sequence starting with `b`.
fn utf8_step(b: u8) -> usize {
    if b < 0x80 {
        1
    } else if b < 0xC0 {
        // Continuation byte — shouldn't happen at this position, but
        // step one byte to make forward progress regardless.
        1
    } else if b < 0xE0 {
        2
    } else if b < 0xF0 {
        3
    } else {
        4
    }
}

fn top_list(stack: &[Vec<Item>]) -> &Vec<Item> {
    stack.last().expect("stack is never empty during parse")
}

fn push_item(stack: &mut [Vec<Item>], item: Item) {
    stack.last_mut().expect("stack is never empty").push(item);
}

// ---------------------------------------------------------------------------
// Display (canonical form, mirroring Java ListItem.toString)
// ---------------------------------------------------------------------------

fn item_display(it: &Item, out: &mut String) {
    match it {
        Item::Integer(s) | Item::Str(s) => out.push_str(s),
        Item::Combination(s, d) => {
            item_display(s, out);
            item_display(d, out);
        }
        Item::List(v) => out.push_str(&list_display(v)),
    }
}

fn list_display(items: &[Item]) -> String {
    let mut out = String::new();
    for (i, it) in items.iter().enumerate() {
        if i > 0 {
            out.push(if matches!(it, Item::List(_)) {
                '-'
            } else {
                '.'
            });
        }
        item_display(it, &mut out);
    }
    out
}

// ---------------------------------------------------------------------------
// Public API: Version
// ---------------------------------------------------------------------------

/// A parsed, comparable Maven version.
///
/// Two `Version`s are equal iff their canonical token lists are equal
/// — *not* iff their original strings match. So `"1"`, `"1.0"`, and
/// `"1.0.0"` are all equal `Version`s with three different originals.
/// [`Hash`] follows [`Eq`] and uses the canonical form, so `Version` is
/// safe to use as a `HashMap` key.
#[derive(Debug, Clone)]
pub struct Version {
    /// The original string, preserved byte-for-byte. [`fmt::Display`]
    /// and [`serde::Serialize`] both emit this so that round-tripping
    /// is stable.
    original: String,
    /// The canonical form — the result of stringifying the normalized
    /// token list. Used for [`Eq`], [`Hash`], and the public
    /// [`Version::canonical`] accessor.
    canonical: String,
    /// The parsed token list itself. The source of truth for [`Ord`].
    items: Vec<Item>,
}

impl Version {
    /// Parse a Maven version string. Total — never errors. Even
    /// gibberish parses to *something* (matching Maven's
    /// `ComparableVersion(String)` constructor).
    pub fn parse(s: &str) -> Self {
        let items = parse(s);
        let canonical = list_display(&items);
        Version {
            original: s.to_owned(),
            canonical,
            items,
        }
    }

    /// Stricter parse that rejects the empty string. Provided for
    /// callers that want a validation step before storing a version.
    /// Currently this is the only failure mode — Maven itself accepts
    /// every other input.
    pub fn parse_strict(s: &str) -> Result<Self, ParseError> {
        if s.is_empty() {
            return Err(ParseError::Empty);
        }
        Ok(Self::parse(s))
    }

    /// The original string this version was parsed from.
    pub fn as_str(&self) -> &str {
        &self.original
    }

    /// The canonical form, produced by stringifying the normalized
    /// token list. This is what [`Eq`] and [`Hash`] compare on.
    pub fn canonical(&self) -> &str {
        &self.canonical
    }

    /// Returns true if this version is a SNAPSHOT — i.e. the original
    /// string ends with `-SNAPSHOT` (case-insensitive).
    ///
    /// Used by the resolver to filter SNAPSHOT versions out of
    /// `RELEASE` meta-version resolution and to trigger SNAPSHOT-
    /// specific fetch logic (timestamp resolution via
    /// `maven-metadata.xml`).
    ///
    /// Note: this checks the original (as-typed) string, not the
    /// canonical form — a timestamped version like
    /// `1.0.0-20240101.123456-7` is *not* a SNAPSHOT in this sense
    /// even though it was produced by publishing one.
    pub fn is_snapshot(&self) -> bool {
        let s = self.original.as_bytes();
        const SUFFIX: &[u8] = b"-SNAPSHOT";
        if s.len() < SUFFIX.len() {
            return false;
        }
        let tail = &s[s.len() - SUFFIX.len()..];
        tail.iter()
            .zip(SUFFIX.iter())
            .all(|(a, b)| a.eq_ignore_ascii_case(b))
    }
}

/// Errors from [`Version::parse_strict`]. [`Version::parse`] /
/// [`FromStr`] never fail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseError {
    /// The input was the empty string.
    Empty,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ParseError::Empty => f.write_str("empty version string"),
        }
    }
}

impl std::error::Error for ParseError {}

impl FromStr for Version {
    type Err = Infallible;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Version::parse(s))
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.original)
    }
}

impl PartialEq for Version {
    fn eq(&self, other: &Self) -> bool {
        self.canonical == other.canonical
    }
}

impl Eq for Version {}

impl Hash for Version {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.canonical.hash(state);
    }
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> Ordering {
        compare_lists(&self.items, &other.items)
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Serialize for Version {
    fn serialize<S: Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        ser.serialize_str(&self.original)
    }
}

impl<'de> Deserialize<'de> for Version {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        let s = String::deserialize(de)?;
        Ok(Version::parse(&s))
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &str) -> Version {
        Version::parse(s)
    }

    #[track_caller]
    fn assert_lt(a: &str, b: &str) {
        let va = v(a);
        let vb = v(b);
        assert!(
            va < vb,
            "expected {a} < {b} (canonicals {} / {})",
            va.canonical(),
            vb.canonical()
        );
        assert!(vb > va, "expected {b} > {a}");
    }

    #[track_caller]
    fn assert_eq_versions(a: &str, b: &str) {
        let va = v(a);
        let vb = v(b);
        assert_eq!(
            va.cmp(&vb),
            Ordering::Equal,
            "expected {a} == {b} (canonicals {} / {})",
            va.canonical(),
            vb.canonical()
        );
        assert_eq!(va, vb);
        let mut h1 = std::collections::hash_map::DefaultHasher::new();
        let mut h2 = std::collections::hash_map::DefaultHasher::new();
        va.hash(&mut h1);
        vb.hash(&mut h2);
        assert_eq!(h1.finish(), h2.finish(), "hash mismatch for {a} / {b}");
    }

    #[track_caller]
    fn assert_same_order(a: &str, b: &str) {
        let va = v(a);
        let vb = v(b);
        assert_eq!(va.cmp(&vb), Ordering::Equal, "expected {a} ~ {b}");
    }

    // ---- Equality cases (ported from testVersionsEqual) ------------------

    #[test]
    fn eq_one_one() {
        assert_eq_versions("1", "1");
    }
    #[test]
    fn eq_one_one_oh() {
        assert_eq_versions("1", "1.0");
    }
    #[test]
    fn eq_one_one_oh_oh() {
        assert_eq_versions("1", "1.0.0");
    }
    #[test]
    fn eq_one_oh_one_oh_oh() {
        assert_eq_versions("1.0", "1.0.0");
    }
    #[test]
    fn eq_one_dash_zero() {
        assert_eq_versions("1", "1-0");
    }
    #[test]
    fn eq_one_oh_dash_zero() {
        assert_eq_versions("1", "1.0-0");
    }
    #[test]
    fn eq_one_a_dash_a() {
        assert_eq_versions("1a", "1-a");
    }
    #[test]
    fn eq_one_a_one_oh_dash_a() {
        assert_eq_versions("1a", "1.0-a");
    }
    #[test]
    fn eq_one_a_one_oh_oh_dash_a() {
        assert_eq_versions("1a", "1.0.0-a");
    }
    #[test]
    fn eq_cr_rc() {
        assert_eq_versions("1cr", "1rc");
    }
    #[test]
    fn eq_alias_a1() {
        assert_eq_versions("1a1", "1-alpha-1");
    }
    #[test]
    fn eq_alias_b2() {
        assert_eq_versions("1b2", "1-beta-2");
    }
    #[test]
    fn eq_alias_m3() {
        assert_eq_versions("1m3", "1-milestone-3");
    }
    #[test]
    fn eq_case_insensitive() {
        assert_eq_versions("1X", "1x");
    }
    #[test]
    fn eq_case_milestone() {
        assert_eq_versions("1m3", "1MILESTONE3");
    }
    #[test]
    fn eq_case_rc() {
        assert_eq_versions("1Cr", "1Rc");
    }

    // Release-qualifier aliases (ga, final, release) compare equal but
    // are not strictly equal — their canonical form differs.
    #[test]
    fn order_ga_release() {
        assert_same_order("1ga", "1");
    }
    #[test]
    fn order_release_release() {
        assert_same_order("1release", "1");
    }
    #[test]
    fn order_final_release() {
        assert_same_order("1final", "1");
    }
    #[test]
    fn order_ga_upper() {
        assert_same_order("1GA", "1");
    }

    // ---- Standard ordering -----------------------------------------------

    #[test]
    fn lt_alpha_release() {
        assert_lt("1.0-alpha-1", "1.0");
    }
    #[test]
    fn lt_alpha1_alpha2() {
        assert_lt("1.0-alpha-1", "1.0-alpha-2");
    }
    #[test]
    fn lt_alpha_beta() {
        assert_lt("1.0-alpha-1", "1.0-beta-1");
    }
    #[test]
    fn lt_beta_snapshot() {
        assert_lt("1.0-beta-1", "1.0-SNAPSHOT");
    }
    #[test]
    fn lt_snapshot_release() {
        assert_lt("1.0-SNAPSHOT", "1.0");
    }
    #[test]
    fn lt_alpha_snapshot_alpha() {
        assert_lt("1.0-alpha-1-SNAPSHOT", "1.0-alpha-1");
    }
    #[test]
    fn lt_release_dash_one() {
        assert_lt("1.0", "1.0-1");
    }
    #[test]
    fn lt_dash_one_dash_two() {
        assert_lt("1.0-1", "1.0-2");
    }
    #[test]
    fn lt_dot_zero_dash_one() {
        assert_lt("1.0.0", "1.0-1");
    }
    #[test]
    fn lt_two_zero_dash_one() {
        assert_lt("2.0-1", "2.0.1");
    }
    #[test]
    fn lt_xyz_lex() {
        assert_lt("2.0.1-klm", "2.0.1-lmn");
    }
    #[test]
    fn lt_release_xyz() {
        assert_lt("2.0.1", "2.0.1-xyz");
    }
    #[test]
    fn lt_release_123() {
        assert_lt("2.0.1", "2.0.1-123");
    }
    #[test]
    fn lt_xyz_123() {
        assert_lt("2.0.1-xyz", "2.0.1-123");
    }

    // ---- Numeric (not lex) ordering --------------------------------------

    #[test]
    fn lt_one_two() {
        assert_lt("1", "2");
    }
    #[test]
    fn lt_one_five_two() {
        assert_lt("1.5", "2");
    }
    #[test]
    fn lt_one_dot_zero_one_dot_one() {
        assert_lt("1.0", "1.1");
    }
    #[test]
    fn lt_one_dot_zero_dot_zero_one_dot_one() {
        assert_lt("1.0.0", "1.1");
    }
    #[test]
    fn lt_one_dot_zero_dot_one_one_dot_one() {
        assert_lt("1.0.1", "1.1");
    }
    #[test]
    fn lt_one_dot_one_one_dot_two_dot_zero() {
        assert_lt("1.1", "1.2.0");
    }
    #[test]
    fn lt_one_nine_one_ten() {
        assert_lt("1.9", "1.10");
    }
    #[test]
    fn lt_leading_zero_two() {
        assert_lt("0.7", "2");
    }
    #[test]
    fn lt_zero_seven_one_dot_zero_dot_seven() {
        assert_lt("0.2", "1.0.7");
    }

    // ---- Full qualifier ordering chain (ported from VERSIONS_QUALIFIER) --

    #[test]
    fn versions_qualifier_order() {
        let versions = [
            "1-alpha2snapshot",
            "1-alpha2",
            "1-alpha-123",
            "1-beta-2",
            "1-beta123",
            "1-m2",
            "1-m11",
            "1-rc",
            "1-cr2",
            "1-rc123",
            "1-SNAPSHOT",
            "1",
            "1-sp",
            "1-sp2",
            "1-sp123",
            "1-abc",
            "1-def",
            "1-pom-1",
            "1-1-snapshot",
            "1-1",
            "1-2",
            "1-123",
        ];
        let parsed: Vec<Version> = versions.iter().map(|s| v(s)).collect();
        for i in 1..parsed.len() {
            for j in i..parsed.len() {
                assert!(
                    parsed[i - 1] < parsed[j],
                    "expected {} < {} (canonicals: {} / {})",
                    versions[i - 1],
                    versions[j],
                    parsed[i - 1].canonical(),
                    parsed[j].canonical(),
                );
            }
        }
    }

    #[test]
    fn versions_number_order() {
        let versions = [
            "2.0", "2.0.a", "2-1", "2.0.2", "2.0.123", "2.1.0", "2.1-a", "2.1b", "2.1-c", "2.1-1",
            "2.1.0.1", "2.2", "2.123", "11.a2", "11.a11", "11.b2", "11.b11", "11.m2", "11.m11",
            "11", "11.a", "11b", "11c", "11m",
        ];
        let parsed: Vec<Version> = versions.iter().map(|s| v(s)).collect();
        for i in 1..parsed.len() {
            for j in i..parsed.len() {
                assert!(
                    parsed[i - 1] < parsed[j],
                    "expected {} < {} (canonicals: {} / {})",
                    versions[i - 1],
                    versions[j],
                    parsed[i - 1].canonical(),
                    parsed[j].canonical(),
                );
            }
        }
    }

    // ---- MNG-* edge cases ------------------------------------------------

    #[test]
    fn mng5568_b_lt_a() {
        assert_lt("6.1.0rc3", "6.1.0");
    }
    #[test]
    fn mng5568_b_lt_c() {
        assert_lt("6.1.0rc3", "6.1H.5-beta");
    }
    #[test]
    fn mng5568_a_lt_c() {
        assert_lt("6.1.0", "6.1H.5-beta");
    }

    #[test]
    fn mng6572_a_lt_b() {
        assert_lt("20190126.230843", "1234567890.12345");
    }
    #[test]
    fn mng6572_b_lt_c() {
        assert_lt("1234567890.12345", "123456789012345.1H.5-beta");
    }
    #[test]
    fn mng6572_a_lt_d() {
        assert_lt("20190126.230843", "12345678901234567890.1H.5-beta");
    }

    #[test]
    fn mng6964_a_lt_c() {
        assert_lt("1-0.alpha", "1");
    }
    #[test]
    fn mng6964_b_lt_c() {
        assert_lt("1-0.beta", "1");
    }
    #[test]
    fn mng6964_a_lt_b() {
        assert_lt("1-0.alpha", "1-0.beta");
    }

    #[test]
    fn mng7644_x1_lt_dash_x2() {
        for x in [
            "abc",
            "alpha",
            "a",
            "beta",
            "b",
            "def",
            "milestone",
            "m",
            "rc",
        ] {
            assert_lt(&format!("1.0.0.{x}1"), &format!("1.0.0-{x}2"));
            assert_eq_versions(&format!("2-{x}"), &format!("2.0.{x}"));
            assert_eq_versions(&format!("2-{x}"), &format!("2.0.0.{x}"));
            assert_eq_versions(&format!("2.0.{x}"), &format!("2.0.0.{x}"));
        }
    }

    #[test]
    fn mng7714_final_lt_sp() {
        assert_lt("1.0.final-redhat", "1.0-sp1-redhat");
        assert_lt("1.0.final-redhat", "1.0-sp-1-redhat");
        assert_lt("1.0.final-redhat", "1.0-sp.1-redhat");
    }

    // ---- Leading zeros, large numbers ------------------------------------

    #[test]
    fn leading_zero_equivalence() {
        let arr = [
            "0000000000000000001",
            "000000000000000001",
            "00000000000000001",
            "0000000000000001",
            "000000000000001",
            "00000000000001",
            "0000000000001",
            "000000000001",
            "00000000001",
            "0000000001",
            "000000001",
            "00000001",
            "0000001",
            "000001",
            "00001",
            "0001",
            "001",
            "01",
            "1",
        ];
        for i in 0..arr.len() {
            for j in i..arr.len() {
                assert_eq_versions(arr[i], arr[j]);
            }
        }
    }

    #[test]
    fn leading_zero_zero_equivalence() {
        let arr = ["00000000000000000", "0000000", "00", "0"];
        for i in 0..arr.len() {
            for j in i..arr.len() {
                assert_eq_versions(arr[i], arr[j]);
            }
        }
    }

    // ---- Canonical-form stability ----------------------------------------

    #[test]
    fn canonical_stable_zero_dot_x() {
        // MNG-7700: "0.x" canonicalizes to "x"
        assert_eq!(v("0.x").canonical(), "x");
    }

    #[test]
    fn canonical_zero_two() {
        assert_eq!(v("0.2").canonical(), "0.2");
    }

    #[test]
    fn canonical_idempotent() {
        // Re-parsing the canonical form must produce the same canonical form.
        for s in [
            "1",
            "1.0",
            "1.0.0",
            "1.0-alpha-1",
            "1.0-rc-1",
            "1.0-SNAPSHOT",
            "2.0.1-xyz",
            "1ga",
            "0.x",
            "0-1",
            "1.0.final-redhat",
        ] {
            let c1 = v(s).canonical().to_owned();
            let c2 = v(&c1).canonical().to_owned();
            assert_eq!(c1, c2, "canonical not idempotent for {s}: {c1} -> {c2}");
        }
    }

    // ---- Display / round-trip --------------------------------------------

    #[test]
    fn display_preserves_original() {
        let cv = v("1.0.0-Alpha-1");
        assert_eq!(format!("{cv}"), "1.0.0-Alpha-1");
    }

    #[test]
    fn fromstr_infallible() {
        let _: Version = "".parse().unwrap();
        let _: Version = "garbage!".parse().unwrap();
    }

    #[test]
    fn parse_strict_rejects_empty() {
        assert_eq!(Version::parse_strict(""), Err(ParseError::Empty));
        assert!(Version::parse_strict("1.0").is_ok());
    }

    // ---- Digits vs letters / non-ASCII -----------------------------------

    #[test]
    fn digit_gt_letter() {
        assert!(v("7") > v("J"));
        assert!(v("7") > v("c"));
    }

    #[test]
    fn non_ascii_letters_lex() {
        assert!(v("zebra") > v("aardvark"));
    }

    #[test]
    fn non_ascii_digit_not_a_digit() {
        // Arabic-Indic digit 8 should NOT be treated as a digit.
        assert!(v("1") > v("\u{0668}"));
    }

    // ---- Serde -----------------------------------------------------------

    #[test]
    fn serde_json_roundtrip() {
        let original = v("1.2.3-rc-4");
        let json = serde_json::to_string(&original).unwrap();
        assert_eq!(json, "\"1.2.3-rc-4\"");
        let back: Version = serde_json::from_str(&json).unwrap();
        assert_eq!(back.as_str(), "1.2.3-rc-4");
        assert_eq!(back, original);
    }

    #[test]
    fn serde_preserves_ordering() {
        let a: Version = serde_json::from_str("\"1.0-alpha-1\"").unwrap();
        let b: Version = serde_json::from_str("\"1.0\"").unwrap();
        assert!(a < b);
    }

    // ---- Hash / Eq follow canonical form ---------------------------------

    // ---- is_snapshot -----------------------------------------------------

    #[test]
    fn is_snapshot_upper() {
        assert!(v("1.0.0-SNAPSHOT").is_snapshot());
    }

    #[test]
    fn is_snapshot_lower() {
        assert!(v("1.0.0-snapshot").is_snapshot());
    }

    #[test]
    fn is_snapshot_mixed_case() {
        assert!(v("1.0.0-SnApShOt").is_snapshot());
    }

    #[test]
    fn is_snapshot_release_is_not() {
        assert!(!v("1.0.0").is_snapshot());
    }

    #[test]
    fn is_snapshot_rc_is_not() {
        assert!(!v("1.0.0-rc-1").is_snapshot());
    }

    #[test]
    fn is_snapshot_timestamped_is_not() {
        // A timestamped publish of a snapshot is not itself a
        // SNAPSHOT version.
        assert!(!v("1.0.0-20240101.123456-7").is_snapshot());
    }

    #[test]
    fn is_snapshot_empty_is_not() {
        assert!(!v("").is_snapshot());
    }

    #[test]
    fn is_snapshot_short_is_not() {
        assert!(!v("S").is_snapshot());
    }

    #[test]
    fn hashmap_key_canonical() {
        use std::collections::HashMap;
        let mut m: HashMap<Version, &'static str> = HashMap::new();
        m.insert(v("1.0.0"), "one");
        assert_eq!(m.get(&v("1")), Some(&"one"));
        assert_eq!(m.get(&v("1.0")), Some(&"one"));
    }
}
