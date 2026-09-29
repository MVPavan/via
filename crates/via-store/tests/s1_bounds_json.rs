//! Task 4 design §10.2 (A10): `json_limits::scan` bounds a JSON document's
//! depth (64) and node count (65,536: every value and every key) before
//! any serde pass, tracking string boundaries and escapes exactly. The
//! claim checked here is inferred: on valid documents the scanner and
//! `serde_json` agree on the tokens, so its counts equal those of the value
//! `serde_json` builds, and on any prefix it never counts more. Seeded
//! (`VIA_TEST_SEED`); the seed is in every failure message. Written before
//! the scanner.
#![expect(
    clippy::unwrap_used,
    reason = "test fixtures and assertions fail loudly"
)]

use serde_json::{Map, Value};
use via_store::json_limits::{LimitError, MAX_DEPTH, MAX_NODES, Scanned, scan};

/// xorshift64*: a small seeded generator; no dependency.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    fn below(&mut self, bound: u64) -> u64 {
        self.next() % bound
    }
}

fn seed() -> u64 {
    std::env::var("VIA_TEST_SEED")
        .ok()
        .and_then(|seed| seed.parse().ok())
        .unwrap_or(0x5eed_0a10)
        .max(1)
}

/// Characters that stress string tracking: quotes, backslashes, brackets,
/// control characters and multi-byte text.
const CHARS: &[char] = &[
    'a', '"', '\\', '[', ']', '{', '}', ',', ':', ' ', '\n', '\u{1}', '\u{1f}', 'é', '€', '😀',
    '/', 'u',
];

fn string(rng: &mut Rng) -> String {
    (0..rng.below(12))
        .map(|_| CHARS[usize::try_from(rng.below(CHARS.len() as u64)).unwrap()])
        .collect()
}

fn value(rng: &mut Rng, depth: u32) -> Value {
    let kind = if depth >= 8 {
        rng.below(5)
    } else {
        rng.below(7)
    };
    match kind {
        0 => Value::Null,
        1 => Value::Bool(rng.below(2) == 0),
        2 => serde_json::json!(rng.next() >> rng.below(64)),
        3 => serde_json::json!(f64::from(-i32::try_from(rng.below(1000)).unwrap()) / 7.0),
        4 => Value::String(string(rng)),
        5 => Value::Array((0..rng.below(5)).map(|_| value(rng, depth + 1)).collect()),
        _ => {
            let mut map = Map::new();
            for _ in 0..rng.below(5) {
                map.insert(string(rng), value(rng, depth + 1));
            }
            Value::Object(map)
        }
    }
}

/// The depth and node count `serde_json`'s own token stream gives: every
/// value and every key is a node, duplicate keys included; a scalar has
/// depth 0.
struct Counted(Scanned);

impl<'de> serde::Deserialize<'de> for Counted {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(Counter).map(Counted)
    }
}

struct Counter;

impl<'de> serde::de::Visitor<'de> for Counter {
    type Value = Scanned;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("any JSON value")
    }

    fn visit_bool<E>(self, _: bool) -> Result<Scanned, E> {
        Ok(SCALAR)
    }

    fn visit_i64<E>(self, _: i64) -> Result<Scanned, E> {
        Ok(SCALAR)
    }

    fn visit_u64<E>(self, _: u64) -> Result<Scanned, E> {
        Ok(SCALAR)
    }

    fn visit_f64<E>(self, _: f64) -> Result<Scanned, E> {
        Ok(SCALAR)
    }

    fn visit_str<E>(self, _: &str) -> Result<Scanned, E> {
        Ok(SCALAR)
    }

    fn visit_unit<E>(self) -> Result<Scanned, E> {
        Ok(SCALAR)
    }

    fn visit_seq<A: serde::de::SeqAccess<'de>>(self, mut seq: A) -> Result<Scanned, A::Error> {
        let mut counted = Scanned { depth: 1, nodes: 1 };
        while let Some(Counted(item)) = seq.next_element()? {
            counted.depth = counted.depth.max(item.depth + 1);
            counted.nodes += item.nodes;
        }
        Ok(counted)
    }

    fn visit_map<A: serde::de::MapAccess<'de>>(self, mut map: A) -> Result<Scanned, A::Error> {
        let mut counted = Scanned { depth: 1, nodes: 1 };
        while map.next_key::<serde::de::IgnoredAny>()?.is_some() {
            let Counted(item) = map.next_value()?;
            counted.depth = counted.depth.max(item.depth + 1);
            counted.nodes += 1 + item.nodes;
        }
        Ok(counted)
    }
}

const SCALAR: Scanned = Scanned { depth: 0, nodes: 1 };

/// What `serde_json` reads from `bytes`, if it reads a value.
fn measure(bytes: &[u8]) -> Option<Scanned> {
    serde_json::from_slice::<Counted>(bytes)
        .ok()
        .map(|Counted(counted)| counted)
}

fn nested(depth: usize) -> String {
    format!("{}{}", "[".repeat(depth), "]".repeat(depth))
}

fn list(nodes: usize) -> String {
    // The list itself is one node.
    format!("[{}]", vec!["0"; nodes - 1].join(","))
}

#[test]
fn s1_bounds_json_limits_agree_with_serde_json() {
    assert_eq!((MAX_DEPTH, MAX_NODES), (64, 65_536));
    // The bounds: depth 64 and 65,536 nodes pass, one more fails.
    assert_eq!(
        scan(nested(64).as_bytes()).unwrap(),
        Scanned {
            depth: 64,
            nodes: 64
        }
    );
    assert_eq!(scan(nested(65).as_bytes()), Err(LimitError::Depth));
    assert_eq!(scan(list(MAX_NODES).as_bytes()).unwrap().nodes, MAX_NODES);
    assert_eq!(scan(list(MAX_NODES + 1).as_bytes()), Err(LimitError::Nodes));
    let keys = format!(
        "{{{}}}",
        (0..32_768)
            .map(|key| format!("\"{key}\":0"))
            .collect::<Vec<_>>()
            .join(",")
    );
    // An object, 32,768 keys and 32,768 values: one node over.
    assert_eq!(scan(keys.as_bytes()), Err(LimitError::Nodes));
    // Brackets and quotes inside strings are text, escapes included.
    let tricky = r#"["[[[{{", "\"]]]", "\\", "\\\"[", {"k\"}": "["}]"#;
    assert_eq!(scan(tricky.as_bytes()).ok(), measure(tricky.as_bytes()));
    // serde_json itself accepts 65 levels: the scanner is the bound.
    assert!(serde_json::from_str::<Value>(&nested(65)).is_ok());

    let seed = seed();
    let mut rng = Rng(seed);
    for case in 0..2_000 {
        let document = value(&mut rng, 0);
        let text = if rng.below(2) == 0 {
            serde_json::to_string(&document).unwrap()
        } else {
            serde_json::to_string_pretty(&document).unwrap()
        };
        let expected = measure(text.as_bytes()).unwrap();
        let scanned = scan(text.as_bytes())
            .unwrap_or_else(|error| panic!("seed {seed} case {case}: {error:?} on {text}"));
        assert_eq!(scanned, expected, "seed {seed} case {case}: {text}");
        // Any prefix counts no more than the whole document.
        let cut = usize::try_from(rng.below(text.len() as u64 + 1)).unwrap();
        let prefix = scan(&text.as_bytes()[..cut])
            .unwrap_or_else(|error| panic!("seed {seed} case {case}: prefix {error:?}"));
        assert!(
            prefix.depth <= scanned.depth && prefix.nodes <= scanned.nodes,
            "seed {seed} case {case}: prefix {prefix:?} of {scanned:?} at {cut}"
        );
        // Mutated bytes never panic; when serde_json still reads them as a
        // value, the counts agree.
        let mut bytes = text.into_bytes();
        for _ in 0..3 {
            if bytes.is_empty() {
                break;
            }
            let at = usize::try_from(rng.below(bytes.len() as u64)).unwrap();
            bytes[at] = b"[]{}\",:\\ 0a"[usize::try_from(rng.below(11)).unwrap()];
        }
        let mutated = scan(&bytes);
        if let Some(read) = measure(&bytes) {
            assert_eq!(mutated, Ok(read), "seed {seed} case {case}");
        }
    }
}
