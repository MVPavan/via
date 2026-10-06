//! Raw vendor-argument passthrough (C2 §6.3; owner, 2026-10-06): a
//! session's frozen `vendor_args`, with the bounds C1 §4 gives them, and
//! the matching every route applies to them against its reserved flags.
//! Pure: reads strings only.

use serde::{Deserialize, Serialize};

/// Most arguments in one `vendor_args` (C1 §4).
pub const VENDOR_ARGS_MAX: usize = 64;

/// Most bytes in one `vendor_args`: the sum of its arguments' UTF-8
/// lengths (C1 §4).
pub const VENDOR_ARGS_BYTES_MAX: usize = 16 * 1024;

/// A session's raw vendor arguments, frozen at spawn (C1 §4
/// `vendor_args`): at most [`VENDOR_ARGS_MAX`] strings of at most
/// [`VENDOR_ARGS_BYTES_MAX`] bytes together, none holding NUL. A stored
/// list that breaks a bound does not decode: it is corruption.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(try_from = "Vec<String>", into = "Vec<String>")]
pub struct VendorArgs(Vec<String>);

/// Why a list is not a `vendor_args` (C1 §4).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VendorArgsError {
    /// More than [`VENDOR_ARGS_MAX`] arguments.
    TooMany,
    /// More than [`VENDOR_ARGS_BYTES_MAX`] bytes together.
    TooLong,
    /// An argument holds a NUL character, which no argv can carry.
    Nul,
}

impl VendorArgsError {
    /// VIA's own message, naming the bound.
    pub fn message(self) -> &'static str {
        match self {
            Self::TooMany => "vendor_args holds more than 64 arguments",
            Self::TooLong => "vendor_args is longer than 16 KiB in total",
            Self::Nul => "a vendor_args argument holds a NUL character",
        }
    }
}

impl std::fmt::Display for VendorArgsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.message())
    }
}

impl TryFrom<Vec<String>> for VendorArgs {
    type Error = VendorArgsError;

    fn try_from(args: Vec<String>) -> Result<Self, Self::Error> {
        if args.len() > VENDOR_ARGS_MAX {
            return Err(VendorArgsError::TooMany);
        }
        if args.iter().map(String::len).sum::<usize>() > VENDOR_ARGS_BYTES_MAX {
            return Err(VendorArgsError::TooLong);
        }
        if args.iter().any(|arg| arg.contains('\0')) {
            return Err(VendorArgsError::Nul);
        }
        Ok(Self(args))
    }
}

impl From<VendorArgs> for Vec<String> {
    fn from(args: VendorArgs) -> Self {
        args.0
    }
}

impl VendorArgs {
    /// The arguments, in order.
    pub fn as_slice(&self) -> &[String] {
        &self.0
    }

    /// Whether the session passes none.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The sum of the arguments' UTF-8 lengths.
    pub fn bytes(&self) -> usize {
        self.0.iter().map(String::len).sum()
    }
}

/// How an option of a route's value-option table takes its value (C2
/// §6.3 rule 2).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Takes {
    /// One value: attached (`--opt=value`, `-ovalue`), else exactly the
    /// next element, as Commander and clap take a required value. A next
    /// element starting with `-` is ambiguous (the vendor may bind it as
    /// the value or read it as an option) and refused.
    One,
    /// Variadic: accepted only as `--opt=value`, one value per occurrence,
    /// since the vendor would take each following bare element as a
    /// further value and VIA cannot tell where the list ends.
    Many,
}

/// One route's reserved flags and value-option table (C2 §6.3), over
/// normalized long names ([`normalize`]).
pub(crate) struct Rules {
    /// Whether a normalized long name is reserved.
    pub(crate) long_reserved: fn(&str) -> bool,
    /// Whether a short letter is reserved.
    pub(crate) short_reserved: fn(char) -> bool,
    /// The unreserved long options that take a value.
    pub(crate) long_value: fn(&str) -> Option<Takes>,
    /// The unreserved short letters that take a value, with their
    /// normalized long name.
    pub(crate) short_value: fn(char) -> Option<(&'static str, Takes)>,
    /// Whether `value` is reserved for the option of normalized long name
    /// `name` (Codex `config` keys and features).
    pub(crate) value_reserved: fn(&str, &str) -> bool,
}

/// A long option's name normalized: leading dashes dropped, lowercased,
/// with `-`, `_` and `.` removed, so `--Permission_Mode` and
/// `--permission-mode` are one name.
pub(crate) fn normalize(name: &str) -> String {
    name.trim_start_matches('-')
        .chars()
        .filter(|c| !matches!(c, '-' | '_' | '.'))
        .flat_map(char::to_lowercase)
        .collect()
}

/// The index of the first argument that could set what `rules` reserve
/// (C2 §6.3), or `None` when every argument passes. The reading is
/// conservative: an element is either unambiguously an option, the value
/// of the option before it, or refused.
pub(crate) fn conflict(args: &[String], rules: &Rules) -> Option<usize> {
    // The option whose value is the next element.
    let mut pending: Option<String> = None;
    for (index, arg) in args.iter().enumerate() {
        if let Some(name) = pending.take() {
            // Rule 2: exactly the next element; one starting with `-`
            // (`--` included) is ambiguous.
            if arg.starts_with('-') || (rules.value_reserved)(&name, arg) {
                return Some(index);
            }
            continue;
        }
        // Rule 1: `--` ends the vendor's options; VIA owns the operands.
        if arg == "--" {
            return Some(index);
        }
        if let Some(long) = arg.strip_prefix("--") {
            let (name, value) = match long.split_once('=') {
                Some((name, value)) => (name, Some(value)),
                None => (long, None),
            };
            let name = normalize(name);
            if (rules.long_reserved)(&name) {
                return Some(index);
            }
            match ((rules.long_value)(&name), value) {
                (Some(_), Some(value)) if (rules.value_reserved)(&name, value) => {
                    return Some(index);
                }
                (Some(Takes::One), None) => pending = Some(name),
                (Some(Takes::Many), None) => return Some(index),
                _ => {}
            }
            continue;
        }
        if let Some(cluster) = arg.strip_prefix('-')
            && !cluster.is_empty()
        {
            for (at, letter) in cluster.char_indices() {
                if (rules.short_reserved)(letter) {
                    return Some(index);
                }
                let Some((name, takes)) = (rules.short_value)(letter) else {
                    // A switch: the cluster goes on.
                    continue;
                };
                // Anything attached, even `=` alone, is the value: Commander
                // binds `-n=` to "=", so the next element is never this
                // option's (review pass-2). Both spellings are judged.
                let attached = &cluster[at + letter.len_utf8()..];
                let stripped = attached.strip_prefix('=').unwrap_or(attached);
                if attached.is_empty() {
                    if takes == Takes::Many {
                        return Some(index);
                    }
                    pending = Some(name.to_owned());
                } else if (rules.value_reserved)(name, stripped)
                    || (rules.value_reserved)(name, attached)
                {
                    return Some(index);
                }
                break;
            }
            continue;
        }
        // Rule 4: an operand.
        return Some(index);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|arg| (*arg).to_owned()).collect()
    }

    /// A route reserving `--model` and `--perm*`, short `-p`; `--opt` and
    /// `-o` take one value, `--many` is variadic; a `--opt` value `bad`
    /// is reserved.
    fn rules() -> Rules {
        Rules {
            long_reserved: |name| name == "model" || name.starts_with("perm"),
            short_reserved: |letter| letter.eq_ignore_ascii_case(&'p'),
            long_value: |name| match name {
                "opt" => Some(Takes::One),
                "many" => Some(Takes::Many),
                _ => None,
            },
            short_value: |letter| (letter == 'o').then_some(("opt", Takes::One)),
            value_reserved: |name, value| name == "opt" && value == "bad",
        }
    }

    /// C1 §4's bounds: 64 arguments, 16 KiB together, no NUL; each one
    /// over is refused, at the bound accepted; a stored list decodes
    /// through the same rule.
    #[test]
    fn bounds() {
        let at = vec!["a".to_owned(); VENDOR_ARGS_MAX];
        assert!(VendorArgs::try_from(at.clone()).is_ok());
        let mut over = at;
        over.push("a".to_owned());
        assert_eq!(VendorArgs::try_from(over), Err(VendorArgsError::TooMany));
        let long = vec!["b".repeat(VENDOR_ARGS_BYTES_MAX / 2); 2];
        assert_eq!(
            VendorArgs::try_from(long.clone()).map(|a| a.bytes()),
            Ok(16 * 1024)
        );
        let mut longer = long;
        longer.push("c".to_owned());
        assert_eq!(VendorArgs::try_from(longer), Err(VendorArgsError::TooLong));
        assert_eq!(
            VendorArgs::try_from(args(&["--x", "a\0b"])),
            Err(VendorArgsError::Nul)
        );
        assert!(serde_json::from_str::<VendorArgs>(r#"["a\u0000"]"#).is_err());
        let decoded: VendorArgs = serde_json::from_str(r#"["--x","y"]"#).unwrap();
        assert_eq!(decoded.as_slice(), &args(&["--x", "y"])[..]);
        assert_eq!(serde_json::to_string(&decoded).unwrap(), r#"["--x","y"]"#);
    }

    /// C2 §6.3: reserved long names in every spelling and attached-value
    /// form, reserved letters anywhere in a switch cluster, `--` alone and
    /// operands are refused at their index; unreserved options and their
    /// values pass.
    #[test]
    fn matching() {
        let rules = rules();
        let first = |list: &[&str]| conflict(&args(list), &rules);
        for refused in [
            &["--model", "x"][..],
            &["--model=x"],
            &["--MODEL"],
            &["--Perm_Mode=y"],
            &["--permission.mode"],
            &["-p"],
            &["-P"],
            &["-xp"],
            &["--"],
            &["operand"],
            &["-"],
            &["--opt", "v", "operand"],
            &["--opt=v", "operand"],
            &["-ov", "operand"],
            &["--switch", "operand"],
            &["--opt=bad"],
            &["--opt", "bad"],
            &["-obad"],
            &["-o=bad"],
            &["-o", "bad"],
        ] {
            assert!(first(refused).is_some(), "{refused:?}");
        }
        assert_eq!(first(&["--ok", "--opt", "v", "--model"]), Some(3));
        for passed in [
            &[][..],
            &["--switch"],
            &["--opt", "v"],
            &["--opt=v"],
            &["--opt"],
            &["-o", "v"],
            &["-ov"],
            &["-o=v"],
            &["-xov"],
            &["-opvalue"],
            &["--many=a"],
            &["--many=a", "--many=b", "--opt", "v"],
            &["--unknown=v", "--switch"],
        ] {
            assert_eq!(first(passed), None, "{passed:?}");
        }
        assert_eq!(normalize("--Allowed_Tools.x"), "allowedtoolsx");
    }

    /// Review pass 1, Important 1: a value option given without `=` takes
    /// exactly the next element, so a dash element there is ambiguous and
    /// refused, and the element after it is never read as an option; a
    /// variadic option takes one attached value per occurrence, so a bare
    /// element after it is an operand and a separate value is refused.
    #[test]
    fn value_options_are_read_conservatively() {
        let rules = rules();
        let first = |list: &[&str]| conflict(&args(list), &rules);
        assert_eq!(first(&["--opt", "--switch", "INJECTED PROMPT"]), Some(1));
        assert_eq!(first(&["--opt", "-p"]), Some(1));
        assert_eq!(first(&["--opt", "--"]), Some(1));
        assert_eq!(first(&["--opt", "-"]), Some(1));
        assert_eq!(first(&["-o", "--switch"]), Some(1));
        assert_eq!(first(&["--many=a", "INJECTED PROMPT"]), Some(1));
        assert_eq!(first(&["--many", "a"]), Some(0));
        assert_eq!(first(&["--many"]), Some(0));
        assert_eq!(first(&["--opt", "v", "INJECTED PROMPT"]), Some(2));
        // An attached `=` alone is a value (Commander binds "="), never a
        // pending one: the next element is an operand (review pass-2).
        assert_eq!(first(&["-o=", "INJECTED PROMPT"]), Some(1));
        assert_eq!(first(&["-o=v"]), None);
        assert_eq!(first(&["-ov"]), None);
    }
}
