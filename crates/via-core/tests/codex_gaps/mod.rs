//! Codex packet §3/§8 gap checks: recovery without resend and a complete
//! catalog surviving server retirement and daemon restart (via-3fc).

use super::*;

/// Packet §8: two accepted threads on one server; EOF or a crash ends
/// that lifetime, and the next lifetime accepts only a fresh turn.
fn recovery_replay(crashed_daemon: bool, accepted: bool) -> (Value, usize, usize) {
    let full = core_codex::replay("c0_server_lost");
    let mut dying = full.clone();
    let through = if accepted { 11 } else { 10 };
    let mut steps = full["steps"].as_array().unwrap()[..through].to_vec();
    let first_gate = steps.len() + 1;
    steps.push(json!({"await_signal":{"signal":"SIGUSR1"}}));
    let echo_step = steps.len() + 2;
    for original in &full["steps"].as_array().unwrap()[6..through] {
        let text = original
            .to_string()
            .replace("000000100001", "000000100002")
            .replace("000000200001", "000000200002");
        let mut step: Value = serde_json::from_str(&text).unwrap();
        if let Some(expect) = step.get_mut("expect").and_then(Value::as_object_mut)
            && expect.contains_key("after_emit")
        {
            expect.insert("after_emit".to_owned(), json!(echo_step));
        }
        steps.push(step);
    }
    let gate = steps.len() + 1;
    steps.push(json!({"await_signal":{"signal":"SIGUSR1"}}));
    if crashed_daemon {
        steps.push(json!({"await_eof":{}}));
    }
    steps.push(json!({"exit":{"code":0,"stderr":""}}));
    dying["steps"] = json!(steps);
    let replay =
        json!({"source":"codex_server_recovery","lifetimes":[dying, echo_copy(C1_PROMPT)]});
    (replay, first_gate, gate)
}

/// C2 §2: acceptance must be committed before simulating daemon loss.
async fn codex_accepted(daemon: &Daemon, session: &SessionId) {
    let by = tokio::time::Instant::now() + Duration::from_secs(30);
    while !events(daemon, session)
        .await
        .iter()
        .any(|e| e["type"] == "turn.started")
    {
        assert!(
            tokio::time::Instant::now() < by,
            "{:?}",
            events(daemon, session).await
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// Packet §8: the fake's signal-consumed log fences its following EOF
/// expectation before daemon loss; sending a signal alone is not a fence.
async fn codex_signal_consumed(root: &Path, name: &str, step: usize) {
    let path = root.join("case").join(format!("{name}.progress"));
    let marker = format!("signalled {step} launch 1");
    let by = tokio::time::Instant::now() + Duration::from_secs(30);
    while !fs::read_to_string(&path)
        .unwrap_or_default()
        .lines()
        .any(|line| line == marker)
    {
        assert!(tokio::time::Instant::now() < by, "{marker}");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// Runtime §5: positive Host group proof is separate from submission
/// certainty; a replay violation must not masquerade as the planned loss.
fn codex_server_absent(root: &Path) {
    // Group death is verified independently of submission certainty.
    let store = rusqlite::Connection::open_with_flags(
        root.join("state/store.sqlite3"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    let proven: i64 = store.query_row(
        "SELECT count(*) FROM anchors WHERE owner_server IS NOT NULL AND absence_time IS NOT NULL",
        [], |row| row.get(0),
    ).unwrap();
    assert_eq!(
        proven, 1,
        "one shared server has positive group-absence evidence"
    );
    drop(store);
    for entry in fs::read_dir(root.join("state/evidence/servers")).unwrap() {
        let log = entry.unwrap().path().join("stderr.log");
        assert!(
            fs::read(&log).unwrap().is_empty(),
            "{:?}",
            fs::read_to_string(log)
        );
    }
}

/// Packet §8 `codex_server_recovery`: two live leases lose their server.
/// Host-confirmed loss fails both `server_lost`; daemon loss leaves both
/// `unknown` at recovery, even with acceptance and positive group proof.
/// Recovery never starts/resumes them; an explicit new session works.
#[test]
fn codex_server_recovery() {
    const NAME: &str = "codex_gaps::codex_server_recovery";
    let Some(root) = child(NAME, &no_fake(), &[]) else {
        return;
    };
    for (name, crashed_daemon, accepted) in [
        ("server", false, true),
        ("daemon", true, true),
        ("pending", true, false),
    ] {
        let trial = root.join(name);
        // Keep the anchor socket root short (runtime §6.1).
        for part in ["", "state", "runtime", "runtime/anchors"] {
            fs::DirBuilder::new()
                .mode(0o700)
                .create(trial.join(part))
                .unwrap();
        }
        let (replay, first_gate, gate) = recovery_replay(crashed_daemon, accepted);
        let case = codex_case(&trial, NAME, replay);
        let sessions = run(async {
            let daemon = Daemon::open_with(&trial, case.config());
            let prompt = "Run the shell command sleep 12. Then say done.";
            let first = daemon.spawn(prompt, &codex_spawn(&case, 60_000)).await;
            case.at_launch(first_gate, 1).await;
            if accepted {
                codex_accepted(&daemon, &first).await;
            }
            let second = daemon.spawn(prompt, &codex_spawn(&case, 60_000)).await;
            case.signal(1);
            case.at_launch(gate, 1).await;
            if accepted {
                // Both acceptance replies are durable before daemon loss.
                for session in [&first, &second] {
                    codex_accepted(&daemon, session).await;
                }
            }
            case.signal(1);
            if crashed_daemon {
                codex_signal_consumed(&trial, NAME, gate).await;
            }
            if !crashed_daemon {
                for session in [&first, &second] {
                    let failed = daemon.wait(session, 1).await;
                    assert_eq!(failed["state"], "failed", "{failed}");
                    assert_eq!(class(&failed), "server_lost", "{failed}");
                    assert!(failed["exit"].is_null(), "{failed}");
                }
                daemon.stop().await;
            }
            // The other path drops the runtime without Engine::shutdown:
            // its tasks and control sockets disappear like a daemon crash.
            [first, second]
        });
        run(async {
            let daemon = Daemon::open_with(&trial, case.config());
            daemon.engine.recover().await.unwrap();
            daemon.engine.hand_off_queued().await.unwrap();
            for session in &sessions {
                let ended = daemon.wait(session, 1).await;
                assert_eq!(
                    ended["state"],
                    if crashed_daemon { "unknown" } else { "failed" },
                    "{ended}"
                );
                assert_eq!(
                    !ended["timestamps"]["accepted_at"].is_null(),
                    accepted,
                    "{ended}"
                );
                if crashed_daemon {
                    assert_eq!(class(&ended), "daemon_restart", "{ended}");
                    assert_eq!(ended["cancel"]["cleanup"], "quiescent", "{ended}");
                }
            }
            codex_server_absent(&trial);
            assert_eq!(case.launches(), 1, "recovery must send nothing to Codex");
            c1_turn(&daemon, &case, 2).await;
            assert_eq!(case.launches(), 2);
            daemon.shutdown().await;
        });
    }
}

/// via-3fc, packet §3: complete discovery remains visible after retirement
/// and an Engine restart, with model-only resolution and no vendor launch.
#[test]
fn codex_catalog_survives_retirement_and_restart() {
    const NAME: &str = "codex_gaps::codex_catalog_survives_retirement_and_restart";
    let Some(root) = child(NAME, &no_fake(), &[]) else {
        return;
    };
    let case = codex_case(&root, NAME, echo_copy(C1_PROMPT));
    let listed = |daemon: &Daemon| {
        let params = serde_json::from_value(json!({"harness":"codex"})).unwrap();
        let models = daemon.engine.models(&params);
        assert_eq!(models["models"].as_array().unwrap().len(), 3, "{models}");
        let params = serde_json::from_value(json!({"model":"gpt-6-luna"})).unwrap();
        let described = daemon.engine.describe(&params).unwrap();
        assert_eq!(described["harness"], "codex", "{described}");
        assert_eq!(described["vendor_version"], "0.159.2", "{described}");
        assert_eq!(case.launches(), 1, "listing and describe are process-free");
    };
    run(async {
        let daemon = Daemon::open_with(&root, case.config());
        c1_turn(&daemon, &case, 1).await;
        listed(&daemon);
        daemon.stop().await;
    });
    run(async {
        let daemon = Daemon::open_with(&root, case.config());
        listed(&daemon);
        daemon.shutdown().await;
    });
}

/// via-3fc, packet §3: a failed paginated discovery cannot publish its
/// partial page over the last complete catalog; a later complete one
/// replaces that catalog, including after another restart.
#[test]
fn codex_catalog_replaces_only_after_complete_discovery() {
    const NAME: &str = "codex_gaps::codex_catalog_replaces_only_after_complete_discovery";
    let Some(root) = child(NAME, &no_fake(), &[]) else {
        return;
    };
    let full = echo_copy(C1_PROMPT);
    let mut partial = full.clone();
    let mut steps = full["steps"].as_array().unwrap()[..6].to_vec();
    let handshake = steps[1]["emit"]["line"].as_str().unwrap();
    steps[1]["emit"]["line"] = json!(handshake.replace("0.159.2", "0.161.0"));
    let line = steps[5]["emit"]["line"]
        .as_str()
        .unwrap()
        .replace("${models}", "999");
    let mut page: Value = serde_json::from_str(&line).unwrap();
    let mut partial_model = page["result"]["data"][0].clone();
    partial_model["id"] = json!("gpt-6-partial");
    partial_model["model"] = json!("gpt-6-partial");
    page["result"]["data"] = json!([partial_model]);
    page["result"]["nextCursor"] = json!("next");
    steps[5]["emit"]["line"] = json!(page.to_string().replace("\"id\":999", "\"id\":${models}"));
    steps.extend([
        json!({"expect":{"line":{"method":"model/list","params":{"cursor":"next"}},"capture":{"page":"/id"}}}),
        json!({"emit":{"line":"{\"id\":${page},\"error\":{\"code\":-32603,\"message\":\"discovery failed\"}}"}}),
        json!({"await_eof":{}}),
    ]);
    partial["steps"] = json!(steps);
    let mut replacement = full.clone();
    let handshake = replacement["steps"][1]["emit"]["line"].as_str().unwrap();
    replacement["steps"][1]["emit"]["line"] = json!(handshake.replace("0.159.2", "0.160.0"));
    // The complete replacement has just Luna. Unknown models still pass
    // through explicitly; this turn's Sol model therefore tests no default.
    let line = full["steps"][5]["emit"]["line"]
        .as_str()
        .unwrap()
        .replace("${models}", "999");
    let mut page: Value = serde_json::from_str(&line).unwrap();
    let mut luna = page["result"]["data"][2].clone();
    luna["isDefault"] = json!(true);
    luna["privateExtra"] = json!("synthetic-catalog-secret");
    page["result"]["data"] = json!([luna]);
    replacement["steps"][5]["emit"]["line"] =
        json!(page.to_string().replace("\"id\":999", "\"id\":${models}"));
    let replay = json!({"source":NAME,"lifetimes":[full, partial, replacement]});
    let case = codex_case(&root, NAME, replay);
    let original = ["gpt-6.1-sol", "gpt-6-sol", "gpt-6-luna"];
    let listed = |daemon: &Daemon, expected: &[&str], version: &str| {
        let params = serde_json::from_value(json!({"harness":"codex"})).unwrap();
        let models = daemon.engine.models(&params);
        let names: Vec<_> = models["models"]
            .as_array()
            .unwrap()
            .iter()
            .map(|model| model["model"].as_str().unwrap())
            .collect();
        assert_eq!(names, expected, "{models}");
        assert!(!models.to_string().contains("synthetic-catalog-secret"));
        let params = serde_json::from_value(json!({"model":"gpt-6-luna"})).unwrap();
        let described = daemon.engine.describe(&params).unwrap();
        assert_eq!(described["vendor_version"], version, "{described}");
    };
    run(async {
        let daemon = Daemon::open_with(&root, case.config());
        c1_turn(&daemon, &case, 1).await;
        listed(&daemon, &original, "0.159.2");
        daemon.stop().await;
    });
    run(async {
        let daemon = Daemon::open_with(&root, case.config());
        listed(&daemon, &original, "0.159.2");
        let session = daemon.spawn(C1_PROMPT, &codex_spawn(&case, 60_000)).await;
        let failed = daemon.wait(&session, 1).await;
        assert_eq!(class(&failed), "protocol", "{failed}");
        listed(&daemon, &original, "0.159.2");
        daemon.stop().await;
    });
    run(async {
        let daemon = Daemon::open_with(&root, case.config());
        listed(&daemon, &original, "0.159.2");
        c1_turn(&daemon, &case, 3).await;
        listed(&daemon, &["gpt-6-luna"], "0.160.0");
        daemon.stop().await;
    });
    run(async {
        let daemon = Daemon::open_with(&root, case.config());
        listed(&daemon, &["gpt-6-luna"], "0.160.0");
        let params = serde_json::from_value(json!({"model":"gpt-6-luna"})).unwrap();
        assert_eq!(
            daemon.engine.describe(&params).unwrap()["vendor_version"],
            "0.160.0"
        );
        let saved = fs::read(root.join("state/vendor/codex/.via-catalog.json")).unwrap();
        assert!(
            !String::from_utf8(saved)
                .unwrap()
                .contains("synthetic-catalog-secret")
        );
        daemon.shutdown().await;
    });
}
