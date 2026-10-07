//! `opencode-serve` conformance under Core's public Engine
//! (`docs/specs/vendors/opencode.md` §13; beads via-4sw.3.1 and
//! via-4sw.3.2): each case runs a daemon over the real Store, Host and
//! Wire against the fake vendor (`support/core_opencode.rs`) and asserts
//! the receipts, envelopes and status Core returns, and the requests the
//! fake answered. Chunk B covers the adapter surface and a session's
//! setup: the turn-level handshake refusal and its cache (OC01), the
//! inherited-configuration states (OC02), the session's creation, reopen
//! and readbacks (OC04, OC07) and the per-turn refusals and variant step
//! (OC11). Chunk C adds turn execution; chunk D covers controls, drain and bounds.
#![expect(
    clippy::unwrap_used,
    clippy::panic,
    reason = "test fixtures and assertions fail loudly"
)]

#[path = "support/core_opencode.rs"]
mod core_opencode;

use core_opencode::{
    Case, MODEL, SES, catalog_entry, class, location_target, route, rules, server, session_info,
    setup_succeeded, with_route,
};
use core_opencode::{session_info_for, with_session};
use serde_json::{Value, json};

/// Runs `body` on a current-thread runtime.
fn run<T, F: Future<Output = T>>(body: F) -> T {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(body)
}

/// Exactly the admitted turns submit prompts; refusals add none.
fn prompt_count(case: &Case, expected: usize) {
    let prompts: Vec<Value> = case
        .requests()
        .into_iter()
        .filter(|request| {
            request["target"]
                .as_str()
                .is_some_and(|target| target.ends_with("/prompt"))
        })
        .collect();
    assert_eq!(prompts.len(), expected, "{prompts:?}");
}

/// OC07 (§5, §6): a new session is created with the model identity, agent
/// `via`, the canonical cwd as its location and exactly the six default
/// permission rules; never with a caller-chosen ID; then VIA's instruction
/// entry is put and read back, and the identity is confirmed before submission.
#[test]
fn oc07_a_new_session_is_created_and_read_back() {
    let case = Case::new(&json!({}));
    let cwd = case.cwd("a");
    case.fixture(&server(&cwd, Some("Be brief.")));
    run(async {
        let daemon = case.daemon();
        let (session, _receipt) = daemon
            .spawn(&cwd, &json!({"instructions": {"text": "Be brief."}}))
            .await;
        let envelope = daemon.wait(&session, 1).await;
        setup_succeeded(&envelope);
        let creates = case.creates();
        assert_eq!(creates.len(), 1, "{creates:?}");
        assert_eq!(
            creates[0]["body"],
            json!({"model": {"providerID": "opencode", "id": "big-pickle"}, "agent": "via",
                   "location": {"directory": cwd}, "permissions": rules(false)}),
            "no caller-chosen id, no variant"
        );
        let puts = case.requests_to(
            "PUT",
            &format!("/api/experimental/session/{SES}/instructions/entries/via"),
        );
        assert_eq!(puts.len(), 1);
        assert_eq!(puts[0]["body"], json!({"value": "Be brief."}));
        assert_eq!(
            case.requests_to(
                "GET",
                &format!("/api/experimental/session/{SES}/instructions/entries")
            )
            .len(),
            1,
            "read back"
        );
        // Effort omitted: no catalog fetch at the location, no switch.
        assert!(case.requests_to("GET", "/api/model?").is_empty());
        assert!(
            case.requests_to("POST", &format!("/api/session/{SES}/model"))
                .is_empty()
        );
        prompt_count(&case, 1);
        let status = daemon.status(&session).await;
        assert_eq!(status["vendor_session_id"], SES, "{status}");
        // §6 close: no vendor call, the history kept.
        let before = case.requests().len();
        daemon.close(&session).await;
        assert_eq!(case.requests().len(), before, "close calls nothing");
        daemon.stop().await;
    });
    run(case.all_gone());
}

/// `harnesses.opencode.inherit` with skills off, the rest at OD2's
/// default.
fn skills_off() -> Value {
    json!({"hooks": false, "mcp_servers": false, "plugins": true,
        "skills": false, "agents": true, "instruction_files": true})
}

/// OC07, OC02 (§4.5, §5): skills requested off (the daemon's configured
/// `inherit`) add `{skill,*,deny}` to the rules, and are `off`, absent
/// from the warning; with no instructions nothing is put.
#[test]
fn oc07_skills_off_adds_the_skill_deny_rule() {
    let case = Case::new(&json!({}));
    let cwd = case.cwd("a");
    let fixture = with_route(
        server(&cwd, None),
        route(
            "POST",
            "/api/session",
            json!([{"status": 200, "json": session_info(&cwd, "default", &rules(true))}]),
        ),
    );
    case.fixture(&fixture);
    run(async {
        let daemon = case.daemon_with(&skills_off());
        let (session, _) = daemon.spawn(&cwd, &json!({})).await;
        setup_succeeded(&daemon.wait(&session, 1).await);
        let creates = case.creates();
        assert_eq!(creates[0]["body"]["permissions"], rules(true));
        assert!(
            case.requests_to("PUT", "/api/experimental/").is_empty(),
            "no instructions, nothing put"
        );
        let status = daemon.status(&session).await;
        assert_eq!(status["inherit"]["skills"], "off", "{status}");
        let warning = status["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .find(|warning| warning["code"] == "config_switch_unverified")
            .unwrap()
            .clone();
        let listed: Vec<&str> = warning["data"]["categories"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| entry["category"].as_str().unwrap())
            .collect();
        assert_eq!(
            listed,
            [
                "hooks",
                "mcp_servers",
                "plugins",
                "agents",
                "instruction_files"
            ],
            "{warning}"
        );
        daemon.stop().await;
    });
    run(case.all_gone());
}

/// OC02 (§4.5, §3): the default request is `unknown` for every category
/// with one `config_switch_unverified` listing all six, and two sessions
/// share one server process; instruction files requested off (another
/// daemon's configuration) are `unknown` too. That both requests have one
/// server key is `opencode_inherit_states`'.
#[test]
fn oc02_inherit_states_and_one_shared_server() {
    let case = Case::new(&json!({}));
    let cwd = case.cwd("a");
    // Independent VIA sessions receive independent vendor IDs on one server.
    let second_id = "ses_via0002";
    let mut second_info = session_info(&cwd, "default", &rules(false));
    second_info["data"]["id"] = json!(second_id);
    let mut fixture = with_route(
        server(&cwd, None),
        route(
            "POST",
            "/api/session",
            json!([
                {"status":200,"json":session_info(&cwd,"default",&rules(false))},
                {"status":200,"json":second_info},
            ]),
        ),
    );
    fixture = with_route(
        fixture,
        route(
            "GET",
            &format!("/api/session/{second_id}"),
            json!([{"status":200,"json":second_info}]),
        ),
    );
    fixture = with_route(
        fixture,
        route(
            "GET",
            &format!("/api/session/{second_id}/inbox"),
            json!([{"status":200,"json":{"data":[]}}]),
        ),
    );
    fixture = with_route(
        fixture,
        route(
            "POST",
            &format!("/api/session/{second_id}/prompt"),
            json!([core_opencode::prompt_response(
                core_opencode::success_events("done")
            )]),
        ),
    );
    case.fixture(&fixture);
    run(async {
        let daemon = case.daemon();
        let (first, _) = daemon.spawn(&cwd, &json!({})).await;
        setup_succeeded(&daemon.wait(&first, 1).await);
        let status = daemon.status(&first).await;
        for category in [
            "hooks",
            "mcp_servers",
            "plugins",
            "skills",
            "agents",
            "instruction_files",
        ] {
            assert_eq!(status["inherit"][category], "unknown", "{status}");
        }
        let warnings: Vec<&Value> = status["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|warning| warning["code"] == "config_switch_unverified")
            .collect();
        assert_eq!(warnings.len(), 1, "{status}");
        assert_eq!(
            warnings[0]["data"]["categories"].as_array().unwrap().len(),
            6
        );
        let (second, _) = daemon.spawn(&cwd, &json!({})).await;
        let second_result = daemon.wait_result(&second, 1).await;
        daemon.stop().await;
        setup_succeeded(&second_result.expect("the second vendor session completes"));
        assert_eq!(case.starts(), 1, "one server for both sessions");
    });
    run(case.all_gone());
    run(async {
        let daemon = case.daemon_with(&json!({"hooks": false, "mcp_servers": false,
            "plugins": true, "skills": true, "agents": true, "instruction_files": false}));
        let (session, _) = daemon.spawn(&cwd, &json!({})).await;
        setup_succeeded(&daemon.wait(&session, 1).await);
        let status = daemon.status(&session).await;
        assert_eq!(
            status["inherit"]["instruction_files"], "unknown",
            "{status}"
        );
        daemon.stop().await;
    });
    run(case.all_gone());
}

/// OC04 (§5, §6 reopen): after a restart the session reopens on the new
/// server by `GET /api/session/{id}` and is never created again; a
/// readback whose permission rules differ from the frozen ones is
/// `SettingsMismatch` (`submit_failed`, `settings_mismatch`), not cached:
/// the same spawn's plan still passes and a fixed server reopens.
#[test]
fn oc04_reopen_reads_back_and_a_mismatch_is_not_cached() {
    let case = Case::new(&json!({}));
    let cwd = case.cwd("a");
    case.fixture(&server(&cwd, None));
    let session = run(async {
        let daemon = case.daemon();
        let (session, _) = daemon.spawn(&cwd, &json!({})).await;
        setup_succeeded(&daemon.wait(&session, 1).await);
        daemon.stop().await;
        session
    });
    run(case.all_gone());
    // The new server reads back other rules.
    let mut tampered = rules(false);
    tampered.as_array_mut().unwrap().pop();
    let fixture = with_route(
        with_session(server(&cwd, None), "ses_via0002", &cwd, None),
        route(
            "GET",
            &format!("/api/session/{SES}"),
            json!([{"status": 200, "json": session_info(&cwd, "default", &tampered)}]),
        ),
    );
    case.fixture(&with_route(
        fixture,
        route(
            "POST",
            "/api/session",
            json!([{"status":200,
            "json":session_info_for("ses_via0002", &cwd, "default", &rules(false))}]),
        ),
    ));
    run(async {
        let daemon = case.daemon();
        daemon.try_resume(&session, &json!({})).await.unwrap();
        let envelope = daemon.wait(&session, 2).await;
        assert_eq!(envelope["state"], "failed", "{envelope}");
        assert_eq!(class(&envelope), "submit_failed", "{envelope}");
        assert_eq!(
            envelope["failure"]["data"],
            json!({"reason": "settings_mismatch"}),
            "{envelope}"
        );
        assert_eq!(case.creates().len(), 1, "never created again");
        assert_eq!(
            case.requests_to("GET", &format!("/api/session/{SES}"))
                .len(),
            2,
            "initial variant readback, then the mismatched reopen readback"
        );
        prompt_count(&case, 1);
        // Not cached: a fresh session's plan passes.
        let (fresh, _) = daemon.spawn(&cwd, &json!({})).await;
        setup_succeeded(&daemon.wait(&fresh, 1).await);
        daemon.stop().await;
    });
    run(case.all_gone());
}

/// OC04 (§6 reopen): a reopened session the vendor no longer has (404) is
/// `resume_mismatch`, never created.
#[test]
fn oc04_a_missing_session_is_resume_mismatch() {
    let case = Case::new(&json!({}));
    let cwd = case.cwd("a");
    case.fixture(&server(&cwd, None));
    let session = run(async {
        let daemon = case.daemon();
        let (session, _) = daemon.spawn(&cwd, &json!({})).await;
        setup_succeeded(&daemon.wait(&session, 1).await);
        daemon.stop().await;
        session
    });
    run(case.all_gone());
    case.fixture(&with_route(
        server(&cwd, None),
        route(
            "GET",
            &format!("/api/session/{SES}"),
            json!([{"status": 404, "json": {"_tag": "SessionNotFoundError"}}]),
        ),
    ));
    run(async {
        let daemon = case.daemon();
        daemon.try_resume(&session, &json!({})).await.unwrap();
        let envelope = daemon.wait(&session, 2).await;
        assert_eq!(envelope["state"], "failed", "{envelope}");
        assert_eq!(class(&envelope), "resume_mismatch", "{envelope}");
        assert_eq!(case.creates().len(), 1);
        prompt_count(&case, 1);
        daemon.stop().await;
    });
    run(case.all_gone());
}

/// OC04 (§5 readback rules): a creation whose permission readback differs
/// from what VIA just sent (skills off, read back without the skill rule)
/// is `handshake_refused` for that turn, cached under the session refusal
/// digest: a second equal session is refused from the cache, before any
/// request of it, while a session whose readback inputs differ (here its
/// instructions) is created and set up; the server stays published. The
/// digest's other inputs are `Settings::refusal_key`'s.
#[test]
fn oc04_a_just_sent_readback_is_refused_under_its_digest() {
    let case = Case::new(&json!({}));
    let cwd = case.cwd("a");
    // The first creation reads back the default rules, the next the
    // skills-off ones.
    case.fixture(&with_route(
        with_session(server(&cwd, Some("Be brief.")), "ses_via0002", &cwd, Some("Be brief.")),
        route(
            "POST",
            "/api/session",
            json!([
                {"status": 200, "json": session_info(&cwd, "default", &rules(false))},
                {"status": 200, "json": session_info_for("ses_via0002", &cwd, "default", &rules(true))},
            ]),
        ),
    ));
    run(async {
        let daemon = case.daemon_with(&skills_off());
        let (first, _) = daemon.spawn(&cwd, &json!({})).await;
        let envelope = daemon.wait(&first, 1).await;
        assert_eq!(class(&envelope), "submit_failed", "{envelope}");
        assert_eq!(
            envelope["failure"]["data"],
            json!({"reason": "handshake_refused"}),
            "{envelope}"
        );
        assert_eq!(case.creates().len(), 1);
        let (second, _) = daemon.spawn(&cwd, &json!({})).await;
        let envelope = daemon.wait(&second, 1).await;
        assert_eq!(
            envelope["failure"]["data"],
            json!({"reason": "handshake_refused"}),
            "{envelope}"
        );
        assert_eq!(
            case.creates().len(),
            1,
            "refused from the cache, nothing created"
        );
        let (other, _) = daemon
            .spawn(&cwd, &json!({"instructions": {"text": "Be brief."}}))
            .await;
        setup_succeeded(&daemon.wait(&other, 1).await);
        assert_eq!(case.creates().len(), 2);
        assert_eq!(case.starts(), 1, "the server stayed published");
        prompt_count(&case, 1);
        daemon.stop().await;
    });
    run(case.all_gone());
}

/// OC04, OC11 (§5 effort): a non-default effort is checked against a fresh
/// catalog at the session's location, then switched and read back as
/// sent; a later turn with effort `"default"` clears it; a vendor
/// that ignores a switch is `handshake_refused` for that turn, nothing
/// submitted only after setup.
#[test]
fn oc04_the_variant_is_switched_read_back_and_cleared() {
    let case = Case::new(&json!({}));
    let cwd = case.cwd("a");
    let info = |variant| json!({"status": 200, "json": session_info(&cwd, variant, &rules(false))});
    let fixture = with_route(
        with_route(
            server(&cwd, None),
            route(
                "GET",
                &format!("/api/session/{SES}"),
                // Each turn reads the variant after admission and reads it
                // again after switching; turn 3's switch is ignored.
                json!([
                    info("default"),
                    info("high"),
                    info("high"),
                    info("default"),
                    info("default"),
                    info("default")
                ]),
            ),
        ),
        route(
            "GET",
            &location_target(&cwd),
            json!([{"status": 200, "json": {"data": [catalog_entry(&["low", "high"])]}}]),
        ),
    );
    case.fixture(&fixture);
    run(async {
        let daemon = case.daemon();
        let (session, _) = daemon.spawn(&cwd, &json!({"effort": "high"})).await;
        setup_succeeded(&daemon.wait(&session, 1).await);
        let switches = case.requests_to("POST", &format!("/api/session/{SES}/model"));
        assert_eq!(
            switches
                .iter()
                .map(|r| r["body"].clone())
                .collect::<Vec<_>>(),
            [json!({"model": {"providerID": "opencode", "id": "big-pickle", "variant": "high"}})]
        );
        assert_eq!(
            case.requests_to("GET", &location_target(&cwd)).len(),
            1,
            "a fresh fetch at the location"
        );
        // C1 P5: an omitted effort inherits `high`; `"default"` clears it.
        daemon
            .try_resume(&session, &json!({"effort": "default"}))
            .await
            .unwrap();
        setup_succeeded(&daemon.wait(&session, 2).await);
        let switches = case.requests_to("POST", &format!("/api/session/{SES}/model"));
        assert_eq!(
            switches[1]["body"],
            json!({"model": {"providerID": "opencode", "id": "big-pickle"}}),
            "cleared"
        );
        assert_eq!(
            case.requests_to("GET", &location_target(&cwd)).len(),
            1,
            "no fetch without an effort"
        );
        daemon
            .try_resume(&session, &json!({"effort": "low"}))
            .await
            .unwrap();
        let envelope = daemon.wait(&session, 3).await;
        assert_eq!(
            envelope["failure"]["data"],
            json!({"reason": "handshake_refused"}),
            "an ignored switch: {envelope}"
        );
        prompt_count(&case, 2);
        daemon.stop().await;
    });
    run(case.all_gone());
}

/// OC11 (§5): two locations on one server whose catalogs give the model
/// different variants: each session's effort is judged by a fresh fetch at
/// its own location in `run_turn`, never by the resume's check; a location
/// whose variant appears only after its first answer refuses the first
/// turn (`invalid_params` naming `effort`, nothing switched or prompted)
/// and admits the retried one.
#[test]
fn oc11_the_variant_is_judged_per_location_in_run_turn() {
    let case = Case::new(&json!({}));
    let (a, b) = (case.cwd("a"), case.cwd("b"));
    let catalog =
        |variants: &[&str]| json!({"status": 200, "json": {"data": [catalog_entry(variants)]}});
    let mut fixture = with_session(server(&a, None), "ses_via0002", &b, None);
    fixture = with_route(
        fixture,
        route("GET", &location_target(&a), json!([catalog(&["high"])])),
    );
    fixture = with_route(
        fixture,
        route(
            "GET",
            &location_target(&b),
            json!([catalog(&["low"]), catalog(&["low", "high"])]),
        ),
    );
    // Distinct identities at each location share one server; readbacks match their own session.
    fixture = with_route(
        fixture,
        route(
            "POST",
            "/api/session",
            json!([
                {"status": 200, "json": session_info(&a, "default", &rules(false))},
                {"status": 200, "json": session_info_for("ses_via0002", &b, "default", &rules(false))},
            ]),
        ),
    );
    fixture = with_route(
        fixture,
        route(
            "GET",
            &format!("/api/session/{SES}"),
            json!([{"status": 200, "json": session_info(&a, "high", &rules(false))}]),
        ),
    );
    fixture = with_route(
        fixture,
        route(
            "GET",
            "/api/session/ses_via0002",
            json!([{"status":200,"json":session_info_for("ses_via0002", &b, "high", &rules(false))}]),
        ),
    );
    case.fixture(&fixture);
    run(async {
        let daemon = case.daemon();
        let (on_a, _) = daemon.spawn(&a, &json!({"effort": "high"})).await;
        setup_succeeded(&daemon.wait(&on_a, 1).await);
        assert_eq!(case.requests_to("GET", &location_target(&a)).len(), 1);
        let (on_b, _) = daemon
            .try_spawn(&b, &json!({"effort": "high"}))
            .await
            .expect("no location-dependent preflight");
        let envelope = daemon.wait(&on_b, 1).await;
        assert_eq!(class(&envelope), "submit_failed", "{envelope}");
        assert_eq!(
            envelope["failure"]["data"],
            json!({"reason": "invalid_param", "field": "effort"}),
            "{envelope}"
        );
        assert_eq!(case.requests_to("GET", &location_target(&b)).len(), 1);
        assert_eq!(
            case.creates().len(),
            1,
            "refused before its session was created"
        );
        daemon
            .try_resume(&on_b, &json!({"effort": "high"}))
            .await
            .expect("check_turn accepts it");
        setup_succeeded(&daemon.wait(&on_b, 2).await);
        assert_eq!(case.requests_to("GET", &location_target(&b)).len(), 2);
        prompt_count(&case, 2);
        daemon.stop().await;
    });
    run(case.all_gone());
}

/// OC11 (C2 §5): `effort: "default"` is omitting it: no catalog fetch, no
/// switch of a variant already clear, even where the location's catalog
/// lists no `default`.
#[test]
fn oc11_effort_default_is_omitted() {
    let case = Case::new(&json!({}));
    let cwd = case.cwd("a");
    case.fixture(&with_route(
        server(&cwd, None),
        route(
            "GET",
            &location_target(&cwd),
            json!([{"status": 200, "json": {"data": [catalog_entry(&["high"])]}}]),
        ),
    ));
    run(async {
        let daemon = case.daemon();
        let (session, _) = daemon.spawn(&cwd, &json!({"effort": "default"})).await;
        setup_succeeded(&daemon.wait(&session, 1).await);
        assert!(case.requests_to("GET", "/api/model?").is_empty());
        assert!(
            case.requests_to("POST", &format!("/api/session/{SES}/model"))
                .is_empty()
        );
        daemon.stop().await;
    });
    run(case.all_gone());
}

/// OC11 (§5, §9): instructions of 262,144 encoded bytes are admitted and
/// put, 262,145 are `invalid_params` naming `instructions` before any
/// receipt, measured on the JSON encoding (escapes count).
#[test]
fn oc11_instruction_size_is_judged_encoded() {
    let case = Case::new(&json!({}));
    let cwd = case.cwd("a");
    // 131,071 newlines encode as 262,142 bytes plus the quotes.
    let at_limit = "\n".repeat(131_071);
    case.fixture(&server(&cwd, Some(&at_limit)));
    run(async {
        let daemon = case.daemon();
        let (session, _) = daemon
            .spawn(&cwd, &json!({"instructions": {"text": at_limit}}))
            .await;
        setup_succeeded(&daemon.wait(&session, 1).await);
        let puts = case.requests_to("PUT", "/api/experimental/");
        assert_eq!(puts.len(), 1);
        for over in ["\n".repeat(131_072), "€".repeat(87_381)] {
            let refused = daemon
                .try_spawn(&cwd, &json!({"instructions": {"text": over}}))
                .await
                .expect_err("over the limit");
            assert_eq!(refused["kind"], "invalid_params", "{refused}");
            assert_eq!(refused["field"], "instructions", "{refused}");
        }
        daemon.stop().await;
    });
    run(case.all_gone());
}

/// OC11 (§9 prompt admission): `json_len(prompt) + json_len(cwd)` up to
/// 1,048,576 − 8,192 is admitted, one byte more is `invalid_params` naming
/// `prompt`, with escaping-heavy text (a control character encodes as six
/// bytes).
#[test]
fn oc11_prompt_admission_on_both_sides() {
    const LIMIT: usize = 1_048_576 - 8_192;
    let case = Case::new(&json!({}));
    let cwd = case.cwd("a");
    case.fixture(&server(&cwd, None));
    let cwd_json = serde_json::to_string(&cwd).unwrap().len();
    // `\u{1}` encodes as `\u0001`; the rest pads with ASCII.
    let prompt = |encoded: usize| {
        let body = encoded - cwd_json - 2;
        let mut text = "\u{1}".repeat(body / 6);
        text.push_str(&"a".repeat(body % 6));
        assert_eq!(
            serde_json::to_string(&text).unwrap().len() + cwd_json,
            encoded
        );
        text
    };
    run(async {
        let daemon = case.daemon();
        let (session, _) = daemon.spawn(&cwd, &json!({"prompt": prompt(LIMIT)})).await;
        setup_succeeded(&daemon.wait(&session, 1).await);
        let refused = daemon
            .try_spawn(&cwd, &json!({"prompt": prompt(LIMIT + 1)}))
            .await
            .expect_err("one byte over");
        assert_eq!(refused["kind"], "invalid_params", "{refused}");
        assert_eq!(refused["field"], "prompt", "{refused}");
        let refused = daemon
            .try_resume(&session, &json!({"prompt": prompt(LIMIT + 1)}))
            .await
            .expect_err("check_turn too");
        assert_eq!(refused["field"], "prompt", "{refused}");
        daemon.stop().await;
    });
    run(case.all_gone());
}

/// OC11 (§2.2, §5, §12): the parameters the route refuses before any
/// receipt or vendor I/O: a non-empty `vendor_args` (`invalid_params`
/// naming it, no kind2), a bound other than `full` with network, extra
/// write dirs, an output schema and a step limit. Nothing starts.
#[test]
fn oc11_refusals_before_any_receipt() {
    let case = Case::new(&json!({}));
    let cwd = case.cwd("a");
    case.fixture(&server(&cwd, None));
    run(async {
        let daemon = case.daemon();
        let cases = [
            (json!({"vendor_args": ["--print-logs"]}), "vendor_args"),
            (
                json!({"bound": {"mode": "full", "extra_write_dirs": [cwd], "network": true}}),
                "bound",
            ),
            (
                json!({"output_schema": {"type": "object"}}),
                "output_schema",
            ),
            (json!({"max_steps": 3}), "max_steps"),
        ];
        for (extra, field) in cases {
            let refused = daemon.try_spawn(&cwd, &extra).await.expect_err(field);
            assert_eq!(refused["kind"], "invalid_params", "{field}: {refused}");
            assert_eq!(refused["field"], field, "{refused}");
            if field == "vendor_args" {
                assert!(refused.get("kind2").is_none(), "{refused}");
            }
        }
        for bound in [
            json!({"mode": "read_only", "extra_write_dirs": [], "network": false}),
            json!({"mode": "full", "extra_write_dirs": [], "network": false}),
        ] {
            let refused = daemon
                .try_spawn(&cwd, &json!({"bound": bound}))
                .await
                .expect_err("bound");
            assert_eq!(refused["kind"], "bound_unsupported", "{refused}");
        }
        assert_eq!(case.starts(), 0, "nothing started");
        daemon.stop().await;
    });
}

/// OC01 (§2.2, §12) at the turn: a fully compatible server reporting
/// `2.0.23` fails the turn `submit_failed`/`handshake_refused`, its message
/// naming the version and the checked set; nothing is created or
/// submitted only after setup. The refusal is cached by binary identity: the next spawn,
/// `allow_untested` or not, is refused at its plan, before any start; the
/// same path replaced by a 2.0.22 program is admitted at once.
#[test]
fn oc01_an_unchecked_version_is_refused_and_cached_by_binary() {
    let case = Case::new(&json!({}));
    let cwd = case.cwd("a");
    let unchecked = with_route(
        server(&cwd, None),
        route(
            "GET",
            "/api/info",
            json!([{"status": 200, "json": {"version": "2.0.23", "pid": "$PID"}}]),
        ),
    );
    case.fixture(&unchecked);
    run(async {
        let daemon = case.daemon();
        let (session, _) = daemon.spawn(&cwd, &json!({})).await;
        let envelope = daemon.wait(&session, 1).await;
        assert_eq!(class(&envelope), "submit_failed", "{envelope}");
        assert_eq!(
            envelope["failure"]["data"],
            json!({"reason": "handshake_refused"}),
            "{envelope}"
        );
        let message = envelope["failure"]["message"].as_str().unwrap();
        assert!(
            message.contains("2.0.23") && message.contains("2.0.22"),
            "{message}"
        );
        assert!(case.creates().is_empty());
        assert_eq!(case.starts(), 1);
        for allow_untested in [false, true] {
            let refused = daemon
                .try_spawn(&cwd, &json!({"allow_untested": allow_untested}))
                .await
                .expect_err("refused from the cache");
            assert_eq!(refused["reason"], "handshake_refused", "{refused}");
        }
        assert_eq!(case.starts(), 1, "nothing started");
        case.fixture(&server(&cwd, None));
        case.replace_program();
        let (admitted, _) = daemon.spawn(&cwd, &json!({})).await;
        setup_succeeded(&daemon.wait(&admitted, 1).await);
        assert_eq!(case.creates().len(), 1);
        daemon.stop().await;
    });
    run(case.all_gone());
}

/// OC01 (§2.2): a transient startup failure (`/api/info` 5xx) is
/// `submit_failed`/`launch_failed` and is not cached: the next spawn is
/// admitted and set up.
#[test]
fn oc01_a_transient_failure_is_not_cached() {
    let case = Case::new(&json!({}));
    let cwd = case.cwd("a");
    case.fixture(&with_route(
        server(&cwd, None),
        route("GET", "/api/info", json!([{"status": 503, "json": {}}])),
    ));
    run(async {
        let daemon = case.daemon();
        let (session, _) = daemon.spawn(&cwd, &json!({})).await;
        let envelope = daemon.wait(&session, 1).await;
        assert_eq!(class(&envelope), "submit_failed", "{envelope}");
        assert_eq!(
            envelope["failure"]["data"],
            json!({"reason": "launch_failed"}),
            "{envelope}"
        );
        case.fixture(&server(&cwd, None));
        let (admitted, _) = daemon.spawn(&cwd, &json!({})).await;
        setup_succeeded(&daemon.wait(&admitted, 1).await);
        daemon.stop().await;
    });
    run(case.all_gone());
}

/// OC02 (§4.3): on a namespace that is not fresh, an integration listing
/// with a stored connection refuses the start (`launch_failed`), the
/// message naming only the integration IDs, never the synthetic value;
/// not cached. A listing of an unknown shape proceeds with
/// `credential_state_unchecked` on the turn.
#[test]
fn oc02_stored_credentials_refuse_naming_integration_ids() {
    const SECRET: &str = "SYNTHETIC-CREDENTIAL-VALUE-77ab";
    let case = Case::new(&json!({}));
    let cwd = case.cwd("a");
    case.fixture(&server(&cwd, None));
    run(async {
        let daemon = case.daemon();
        let (first, _) = daemon.spawn(&cwd, &json!({})).await;
        setup_succeeded(&daemon.wait(&first, 1).await);
        daemon.stop().await;
    });
    run(case.all_gone());
    // The namespace exists now; give it a database.
    let namespace = std::fs::read_dir(case.vendor().join("opencode"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| path.file_name().unwrap() != "probe")
        .unwrap();
    std::fs::create_dir_all(namespace.join("data/opencode")).unwrap();
    std::fs::write(namespace.join("data/opencode/opencode.db"), b"").unwrap();
    case.fixture(&with_route(
        server(&cwd, None),
        route(
            "GET",
            "/api/integration",
            json!([{"status": 200, "json": {"data": [
                {"id": "acme-cloud", "connections": [
                    {"type": "credential", "id": "c1", "label": "k", "method": "api", "key": SECRET}]},
                {"id": "plain", "connections": []},
            ]}}]),
        ),
    ));
    run(async {
        let daemon = case.daemon();
        let (session, _) = daemon.spawn(&cwd, &json!({})).await;
        let envelope = daemon.wait(&session, 1).await;
        assert_eq!(
            envelope["failure"]["data"],
            json!({"reason": "launch_failed"}),
            "{envelope}"
        );
        let message = envelope["failure"]["message"].as_str().unwrap();
        assert!(message.contains("acme-cloud"), "{message}");
        assert!(!message.contains("plain"), "{message}");
        assert!(!envelope.to_string().contains(SECRET));
        daemon.stop().await;
    });
    run(case.all_gone());
    // An integration listing of an unknown shape proceeds, and the turn
    // carries `credential_state_unchecked`.
    case.fixture(&with_route(
        server(&cwd, None),
        route(
            "GET",
            "/api/integration",
            json!([{"status": 200, "json": {"data": [
                {"id": "x", "connections": [{"type": "token"}]}]}}]),
        ),
    ));
    run(async {
        let daemon = case.daemon();
        let (session, _) = daemon.spawn(&cwd, &json!({})).await;
        let envelope = daemon.wait(&session, 1).await;
        setup_succeeded(&envelope);
        let codes: Vec<&Value> = envelope["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .map(|warning| &warning["code"])
            .collect();
        assert!(
            codes.contains(&&json!("credential_state_unchecked")),
            "{envelope}"
        );
        daemon.stop().await;
    });
    run(case.all_gone());
    for (path, bytes) in case.daemon_files() {
        assert!(
            !bytes
                .windows(SECRET.len())
                .any(|window| window == SECRET.as_bytes()),
            "{} holds the secret",
            path.display()
        );
    }
    let _ = MODEL;
}

#[path = "support/core_opencode_turns.rs"]
mod core_opencode_turns;

#[path = "support/core_opencode_controls.rs"]
mod core_opencode_controls;
