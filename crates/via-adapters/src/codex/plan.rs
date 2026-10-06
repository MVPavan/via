//! Codex's pure planning data (vendors/codex.md §3, §4, §7; C2 §5, §6.1):
//! the declared capabilities, the `checked` versions, the canonical effort
//! table, the bound mapping and its gate, the inherited-configuration
//! declarations and the reserved vendor keys.

use std::collections::BTreeMap;
use std::path::Path;

use via_routes::codex::{SandboxMode, SandboxPolicy};

use crate::capabilities::{BoundMode, Capabilities, ParamSupport, Support, UsageSupport, Verbs};
use crate::plan::{Bound, Category, CategoryDecl, InheritState, Switch, VendorOptions};

/// The versions maintainers' live check passed (C2 §5 version rule): the
/// re-probes of 2026-09-30 (`via-5lr.3.1`) and live rounds 1 and 2 through
/// VIA on 0.160.0 (2026-10-05/06, `via-5lr.3.4`).
pub(crate) const CHECKED: &[&str] = &["0.159.2", "0.160.0"];

/// The canonical C1 efforts and the Codex `ReasoningEffort` each maps to
/// (AD18). Any other non-empty value is a vendor value that only a
/// discovered `model/list` catalog can judge.
const EFFORTS: &[(&str, &str)] = &[
    ("low", "low"),
    ("medium", "medium"),
    ("high", "high"),
    ("xhigh", "xhigh"),
    ("max", "max"),
];

/// Most bytes of a turn's JSON-encoded prompt plus its JSON-encoded cwd
/// (C1 §4 `prompt`; via-5lr.6, x.3.2 X5). Codex echoes the prompt whole
/// in the user message's `item/started` and `item/completed`
/// notifications, one line each. The limit was set against Wire's 1 MiB
/// line cap (1,048,576 bytes with its LF) and is kept now that the Codex
/// cap is 8 MiB (via-5lr.3.5): raising it is a C1 change. The
/// recorded echo lines carry the prompt once, never the cwd, and at most
/// 340 other bytes, LF included (every 0.159.2 fixture); 1 MiB less 8 KiB
/// leaves over 7.5 KiB for fields a later version adds. The cwd is
/// counted too, as `opencode-serve` counts it, for headroom.
pub(crate) const PROMPT_ECHO_MAX: usize = 1_040_384;

/// Vendor keys refused as `vendor_option_conflict` (C2 §6.1, packet §3):
/// what VIA sets on every thread and turn, the config keys behind them,
/// and the selectors that would change identity, policy or tools.
const RESERVED: &[&str] = &[
    "sandbox",
    "sandboxPolicy",
    "approvalPolicy",
    "approvalsReviewer",
    "cwd",
    "model",
    "developerInstructions",
    "baseInstructions",
    "ephemeral",
    "threadId",
    "outputSchema",
    "effort",
    "sandbox_mode",
    "approval_policy",
    "model_reasoning_effort",
    "config",
    "modelProvider",
    "excludeTurns",
    "permissionProfile",
    "activePermissionProfile",
    "serviceTierForTurn",
    "disabledPluginIds",
    "toolOutput",
    "clientUserMessageId",
    "turnTrigger",
    "sessionStartSource",
    "threadSource",
];

/// The capabilities `codex-app-server` declares (packet §7, C1 §4.2): the
/// limited bounds, qualified live by `via-5lr.3.4` with `network: false`
/// only, which the route honours (`network_control`); `full` needs
/// `network: true`.
pub(crate) fn capabilities() -> Capabilities {
    let unsupported = |reason: &str| Support::Unsupported {
        reason: reason.to_owned(),
    };
    Capabilities {
        verbs: Verbs {
            spawn: Support::Native,
            resume: Support::Native,
            // Owner, 2026-10-04: deferred past the first release
            // (via-gaz); callers stop and resume instead.
            steer: unsupported("deferred past the first release"),
            cancel: Support::Native,
            close: Support::Native,
        },
        params: ParamSupport {
            instructions: Support::Native,
            output_schema: Support::Native,
            effort: Support::Native,
            max_steps: unsupported("codex-app-server has no per-turn step limit"),
        },
        bounds: vec![
            BoundMode::ReadOnly,
            BoundMode::WorkspaceWrite,
            BoundMode::Full,
        ],
        // `network: false` is honoured in both limited bounds (via-5lr.3.4).
        network_control: true,
        recover: unsupported(
            "an owned stdio server cannot rejoin an in-flight turn after a daemon restart",
        ),
        usage: UsageSupport {
            tokens: "turn".to_owned(),
            cost: "unavailable".to_owned(),
        },
    }
}

/// The inherited-configuration declarations (packet §4, C2 §6.2; owner
/// 2026-10-06). Only `--disable hooks` is a switch VIA applies (verified);
/// for the first release VIA disables nothing else (owner 2026-10-05).
/// With no switch, a category is `on` (the user's configuration applies,
/// whatever it contains) where recorded live evidence shows Codex loads
/// it: hooks (the owner's hooks ran, 2026-09-30), MCP servers (the user's
/// servers and `codex_apps` started, 2026-10-05) and instruction files
/// (`instructionSources` listed the loaded AGENTS.md, 0.159.2 re-probe),
/// and, from `via-5lr.3.4`'s 0.160.0 runs (2026-10-06), skills (listed
/// with no switch), agents (the model named the project's agent roles)
/// and plugins (their skills listed; Codex loads plugins after the server
/// starts, so a turn accepted right after a fresh start may not see them).
/// An off VIA cannot apply is declared unverified, so it reports
/// `unknown`, never a suppression.
pub(crate) fn categories() -> BTreeMap<Category, CategoryDecl> {
    let loaded = |off| CategoryDecl {
        on: Switch::None,
        off,
        observed: Some(InheritState::On),
    };
    Category::ALL
        .into_iter()
        .map(|category| match category {
            Category::Hooks => (category, loaded(Switch::Verified)),
            Category::McpServers
            | Category::InstructionFiles
            | Category::Plugins
            | Category::Skills
            | Category::Agents => (category, loaded(Switch::Unverified)),
        })
        .collect()
}

/// The Codex value of a canonical C1 effort; `None` for a vendor value.
pub(crate) fn canonical_effort(effort: &str) -> Option<&'static str> {
    EFFORTS
        .iter()
        .find(|(canonical, _)| *canonical == effort)
        .map(|(_, vendor)| *vendor)
}

/// Why the route refuses `effort` purely, if it does: an empty value is
/// one it knows to be invalid (AD18). A non-canonical non-empty value is
/// judged against the discovered catalog in `run_turn`.
pub(crate) fn effort_refused(effort: &str) -> bool {
    effort.is_empty()
}

/// What the handshake's echo check compares against a thread reply
/// (packet §3), less the approval policy and reviewer, which are
/// constants: the resolved model, the session cwd and the turn's
/// sandbox. It is also exactly the variable part of the refusal cache's
/// key (C2 §5), so a refusal never covers a request the check would not
/// have refused.
#[derive(Debug)]
pub(crate) struct Echoed<'a> {
    /// The resolved model.
    pub(crate) model: &'a str,
    /// The session's working directory.
    pub(crate) cwd: &'a Path,
    /// The turn's sandbox, mode and policy, as derived from its bound.
    pub(crate) sandbox: &'a Sandbox,
}

/// A bound as Codex applies it: the thread `sandbox` mode at start and
/// resume, and the structured `sandboxPolicy` of every `turn/start`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Sandbox {
    /// `thread/start` and `thread/resume` `sandbox`.
    pub(crate) mode: SandboxMode,
    /// `turn/start` `sandboxPolicy`.
    pub(crate) policy: SandboxPolicy,
}

/// Why a bound is refused (`bound_unsupported`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum BoundRefusal {
    /// `full` with `network: false`: no policy variant exists.
    FullWithoutNetwork,
    /// A limited bound with `network: true`: its network access is not
    /// qualified.
    LimitedWithNetwork,
}

impl BoundRefusal {
    /// The refusal's message, naming the route.
    pub(crate) fn message(self, route: &str) -> String {
        match self {
            Self::FullWithoutNetwork => {
                format!("route {route} has no full-access policy without network access")
            }
            Self::LimitedWithNetwork => format!(
                "route {route} has not verified network access under a limited bound: use network false"
            ),
        }
    }
}

/// The packet §3 bound mapping, gated: a limited bound maps to its policy
/// with `network: false` only (`via-5lr.3.4` qualified no network access
/// under a sandbox); `full` with `network: false` is always refused; there
/// is no fallback to `full`.
pub(crate) fn sandbox(bound: &Bound) -> Result<Sandbox, BoundRefusal> {
    let mapped = match bound.mode {
        BoundMode::Full if !bound.network => return Err(BoundRefusal::FullWithoutNetwork),
        BoundMode::ReadOnly | BoundMode::WorkspaceWrite if bound.network => {
            return Err(BoundRefusal::LimitedWithNetwork);
        }
        BoundMode::Full => Sandbox {
            mode: SandboxMode::DangerFullAccess,
            policy: SandboxPolicy::DangerFullAccess,
        },
        BoundMode::ReadOnly => Sandbox {
            mode: SandboxMode::ReadOnly,
            policy: SandboxPolicy::ReadOnly {
                network_access: bound.network,
            },
        },
        BoundMode::WorkspaceWrite => Sandbox {
            mode: SandboxMode::WorkspaceWrite,
            policy: SandboxPolicy::WorkspaceWrite {
                writable_roots: bound.extra_write_dirs.clone(),
                network_access: bound.network,
                exclude_slash_tmp: true,
                exclude_tmpdir_env_var: true,
            },
        },
    };
    Ok(mapped)
}

/// How the route judges the caller's Codex vendor options (C2 §6.1).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum VendorRefusal {
    /// A reserved key: canonical parameters win.
    Reserved,
    /// Any other key: the allow-list is empty.
    NotAllowed,
}

/// The first refusal of the `codex` options in `vendor`; options for other
/// harnesses are not this route's. A reserved key is reported before an
/// unknown one.
pub(crate) fn vendor_refusal(harness: &str, vendor: &VendorOptions) -> Option<VendorRefusal> {
    let options = vendor.get(harness)?;
    if options.keys().any(|key| RESERVED.contains(&key.as_str())) {
        Some(VendorRefusal::Reserved)
    } else if options.is_empty() {
        None
    } else {
        Some(VendorRefusal::NotAllowed)
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Map, json};

    use super::*;

    fn bound(mode: BoundMode, network: bool) -> Bound {
        Bound {
            mode,
            extra_write_dirs: vec!["/extra".into()],
            network,
        }
    }

    /// Packet §3: `full` + network maps to `danger-full-access` /
    /// `dangerFullAccess`; the qualified limited bounds without network map
    /// to their own policies; `full` without network and a limited bound
    /// with network are refused, never mapped to `full`.
    #[test]
    fn bound_mapping_and_gate() {
        assert_eq!(
            sandbox(&bound(BoundMode::Full, true)),
            Ok(Sandbox {
                mode: SandboxMode::DangerFullAccess,
                policy: SandboxPolicy::DangerFullAccess,
            })
        );
        assert_eq!(
            sandbox(&bound(BoundMode::Full, false)),
            Err(BoundRefusal::FullWithoutNetwork)
        );
        assert_eq!(
            sandbox(&bound(BoundMode::ReadOnly, false)),
            Ok(Sandbox {
                mode: SandboxMode::ReadOnly,
                policy: SandboxPolicy::ReadOnly {
                    network_access: false
                },
            })
        );
        assert_eq!(
            sandbox(&bound(BoundMode::WorkspaceWrite, false)),
            Ok(Sandbox {
                mode: SandboxMode::WorkspaceWrite,
                policy: SandboxPolicy::WorkspaceWrite {
                    writable_roots: vec!["/extra".into()],
                    network_access: false,
                    exclude_slash_tmp: true,
                    exclude_tmpdir_env_var: true,
                },
            })
        );
        for mode in [BoundMode::ReadOnly, BoundMode::WorkspaceWrite] {
            assert_eq!(
                sandbox(&bound(mode, true)),
                Err(BoundRefusal::LimitedWithNetwork),
                "{mode:?}"
            );
        }
    }

    /// `describe` offers exactly the bounds [`sandbox`] can map for some
    /// `network` value.
    #[test]
    fn declared_bounds_follow_the_gate() {
        assert_eq!(
            capabilities().bounds,
            [
                BoundMode::ReadOnly,
                BoundMode::WorkspaceWrite,
                BoundMode::Full
            ]
        );
        assert!(capabilities().network_control);
    }

    /// The limited policies encode as packet §3's table, tmp exclusions
    /// included.
    #[test]
    fn limited_policies_encode_as_the_packet_table() {
        let policy = SandboxPolicy::WorkspaceWrite {
            writable_roots: vec!["/extra".into()],
            network_access: false,
            exclude_slash_tmp: true,
            exclude_tmpdir_env_var: true,
        };
        assert_eq!(
            serde_json::to_value(&policy).unwrap(),
            json!({"type": "workspaceWrite", "writableRoots": ["/extra"],
                "networkAccess": false, "excludeSlashTmp": true, "excludeTmpdirEnvVar": true})
        );
        assert_eq!(
            serde_json::to_value(SandboxPolicy::ReadOnly {
                network_access: true
            })
            .unwrap(),
            json!({"type": "readOnly", "networkAccess": true})
        );
        assert_eq!(
            serde_json::to_value(SandboxMode::WorkspaceWrite).unwrap(),
            json!("workspace-write")
        );
    }

    /// C2 §6.1: reserved keys are `vendor_option_conflict` and any other
    /// Codex key is not allowed; other harnesses' options are not judged.
    #[test]
    fn vendor_keys() {
        let options = |pairs: &[&str]| {
            let mut map = Map::new();
            for key in pairs {
                map.insert((*key).to_owned(), json!(1));
            }
            let mut vendor = VendorOptions::new();
            vendor.insert("codex".to_owned(), map);
            vendor
        };
        for key in RESERVED {
            assert_eq!(
                vendor_refusal("codex", &options(&[key])),
                Some(VendorRefusal::Reserved),
                "{key}"
            );
        }
        assert_eq!(
            vendor_refusal("codex", &options(&["personality", "sandbox"])),
            Some(VendorRefusal::Reserved)
        );
        assert_eq!(
            vendor_refusal("codex", &options(&["personality"])),
            Some(VendorRefusal::NotAllowed)
        );
        assert_eq!(vendor_refusal("codex", &options(&[])), None);
        assert_eq!(vendor_refusal("claude", &options(&["sandbox"])), None);
    }

    /// AD18: the canonical efforts map to themselves; empty is refused;
    /// a vendor value passes to discovery.
    #[test]
    fn effort_table() {
        for effort in ["low", "medium", "high", "xhigh", "max"] {
            assert_eq!(canonical_effort(effort), Some(effort));
            assert!(!effort_refused(effort));
        }
        assert_eq!(canonical_effort("ultra"), None);
        assert!(!effort_refused("ultra"));
        assert!(effort_refused(""));
    }
}
