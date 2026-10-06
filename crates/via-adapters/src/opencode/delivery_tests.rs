use super::declines::waiting_permission;
use super::*;
use crate::UsageSample;
use via_routes::opencode::declines::{DeclineOutcome, DeclineSettlement};
use via_routes::opencode::events::{Event, ExecutionKind, InteractiveKind, TextKind, ToolKind};
use via_routes::opencode::router::Router;

fn setup() -> (
    Arc<Registration>,
    Arc<Delivery>,
    tokio::sync::mpsc::Receiver<crate::observation::Admitted>,
) {
    let mut router = Router::new();
    let lane = router.attach("ses_delivery");
    let (sink, receiver) = crate::observation::observation_channel();
    let registration = Registration::new(
        lane,
        sink,
        Arc::new(watch::Sender::new(DriverHealth::Open)),
        7,
    );
    let turn = TurnNumber::try_from(1).unwrap();
    let delivery = registration.admit(
        "msg_viaDelivery".into(),
        turn,
        TurnActivity::new(Instant::now()),
        None,
    );
    (registration, delivery, receiver)
}

fn accepted(delivery: &Delivery) -> LaneItem {
    LaneItem::Accepted {
        staging: None,
        owner: delivery.turn,
        input_id: delivery.input.as_str().into(),
        read_order: 1,
        position: 0,
        decoded_at: Instant::now(),
    }
}

fn event(delivery: &Delivery, order: u64, data: EventData) -> LaneItem {
    LaneItem::Event(Box::new(Routed {
        staging: None,
        event: Event {
            id: None,
            session_id: Some("ses_delivery".into()),
            seq: None,
            data,
        },
        owner: Some(delivery.turn),
        read_order: order,
        position: 0,
        decoded_at: Instant::now(),
        late: false,
        joined_steps: Vec::new(),
    }))
}

fn succeeded(delivery: &Delivery, order: u64) -> LaneItem {
    event(
        delivery,
        order,
        EventData::Execution {
            kind: ExecutionKind::Succeeded,
            error: None,
            reason: None,
        },
    )
}

#[tokio::test]
async fn oc08_input_cancel_before_delivery_is_a_native_terminal() {
    let (registration, delivery, mut receiver) = setup();
    assert!(
        registration
            .process(accepted(&delivery))
            .await
            .is_continue()
    );
    drop(receiver.recv().await.unwrap());
    let consumer = {
        let registration = registration.clone();
        let cancelled = event(
            &delivery,
            2,
            EventData::Inbox {
                kind: InboxKind::Cancelled,
                id: delivery.input.as_str().into(),
            },
        );
        tokio::spawn(async move { registration.process(cancelled).await.is_continue() })
    };
    let decision =
        tokio::time::timeout(std::time::Duration::from_millis(100), delivery.decision()).await;
    let sealed = delivery.seal();
    assert!(consumer.await.unwrap());
    assert!(matches!(decision, Ok(Decision::Terminal)));
    let terminal = sealed.terminal.unwrap();
    assert_eq!(terminal.status, VendorTerminalStatus::Interrupted);
    assert_eq!(terminal.stop_reason, crate::StopReason::Other);
    assert_eq!(terminal.vendor_stop_reason, "input_cancelled");
    assert_eq!(
        terminal.vendor_code.as_deref(),
        Some("session.inbox.cancelled")
    );
    assert_eq!(terminal.usage, None);
}

#[tokio::test]
async fn oc08_late_input_cancel_revises_only_a_previously_accepted_turn() {
    let (registration, delivery, mut receiver) = setup();
    assert!(
        registration
            .process(accepted(&delivery))
            .await
            .is_continue()
    );
    drop(receiver.recv().await.unwrap());
    assert!(delivery.seal().accepted);
    assert!(
        registration
            .process(event(
                &delivery,
                2,
                EventData::Inbox {
                    kind: InboxKind::Cancelled,
                    id: delivery.input.as_str().into(),
                },
            ))
            .await
            .is_continue()
    );
    let late = receiver.try_recv().unwrap();
    assert_eq!(late.item.vendor_turn.unwrap().as_str(), "msg_viaDelivery");
    assert!(matches!(
        late.item.observation,
        Observation::LateTerminal(VendorTerminal {
            status: VendorTerminalStatus::Interrupted,
            usage: None,
            ..
        })
    ));
}

#[tokio::test]
async fn oc10_retained_terminal_survives_later_lane_failure() {
    let (registration, delivery, mut receiver) = setup();
    assert!(
        registration
            .process(accepted(&delivery))
            .await
            .is_continue()
    );
    assert!(matches!(
        receiver.recv().await.unwrap().item.observation,
        Observation::Accepted(_)
    ));
    let consumer = {
        let registration = registration.clone();
        let terminal = succeeded(&delivery, 2);
        tokio::spawn(async move { registration.process(terminal).await.is_continue() })
    };
    assert!(matches!(
        tokio::time::timeout(std::time::Duration::from_secs(1), delivery.decision())
            .await
            .unwrap(),
        Decision::Terminal
    ));
    registration.fail(Stop::Lane(LaneFailure::Overflow));
    let sealed = delivery.seal();
    assert_eq!(
        sealed.terminal.unwrap().status,
        VendorTerminalStatus::Completed
    );
    assert!(sealed.accounted);
    assert!(consumer.await.unwrap());
}

#[tokio::test]
async fn oc11_failed_sink_keeps_acceptance_but_invalidates_accounting() {
    let (registration, delivery, receiver) = setup();
    drop(receiver);
    assert!(
        !registration
            .process(accepted(&delivery))
            .await
            .is_continue()
    );
    let sealed = delivery.seal();
    assert!(sealed.accepted);
    assert!(sealed.terminal.is_none());
    assert!(!sealed.accounted);
    assert_eq!(sealed.loss.unwrap().first_unqueued, 1);
    assert!(matches!(
        *registration.health.borrow(),
        DriverHealth::Failed { .. }
    ));
}

#[tokio::test]
async fn oc11_final_text_delivery_loss_keeps_terminal_with_unavailable_usage() {
    use via_routes::opencode::events::StepKind;
    let (registration, delivery, mut receiver) = setup();
    assert!(
        registration
            .process(accepted(&delivery))
            .await
            .is_continue()
    );
    drop(receiver.recv().await.unwrap());
    let step = event(
        &delivery,
        2,
        EventData::Step {
            kind: StepKind::Started,
            assistant_message_id: "assistant_final".into(),
            finish: None,
            tokens: None,
            cost: None,
        },
    );
    assert!(registration.process(step).await.is_continue());
    let text = event(
        &delivery,
        3,
        EventData::Text {
            kind: TextKind::Ended,
            assistant_message_id: "assistant_final".into(),
            ordinal: 0,
            text: "retained final text".into(),
        },
    );
    assert!(registration.process(text).await.is_continue());
    drop(receiver);
    assert!(
        !registration
            .process(succeeded(&delivery, 4))
            .await
            .is_continue()
    );
    assert!(matches!(delivery.decision().await, Decision::Terminal));
    let sealed = delivery.seal();
    assert_eq!(
        sealed.terminal.unwrap().status,
        VendorTerminalStatus::Completed
    );
    assert!(!sealed.accounted);
    assert_eq!(sealed.loss.unwrap().first_unqueued, 4);
}

#[tokio::test]
async fn oc06_early_owned_progress_waits_for_acceptance() {
    let (registration, delivery, mut receiver) = setup();
    let early = event(
        &delivery,
        1,
        EventData::Text {
            kind: TextKind::Delta,
            assistant_message_id: "assistant_early".into(),
            ordinal: 0,
            text: "hello".into(),
        },
    );
    assert!(registration.process(early).await.is_continue());
    assert!(receiver.try_recv().is_err());
    let mut acceptance = accepted(&delivery);
    if let LaneItem::Accepted { read_order, .. } = &mut acceptance {
        *read_order = 2;
    }
    assert!(registration.process(acceptance).await.is_continue());
    assert!(matches!(
        receiver.recv().await.unwrap().item.observation,
        Observation::Accepted(_)
    ));
    assert!(matches!(
        receiver.recv().await.unwrap().item.observation,
        Observation::Progress(_)
    ));
    delivery.seal();
}

#[tokio::test]
async fn oc06_late_terminal_emits_once_only_after_nonterminal_end() {
    let (registration, delivery, mut receiver) = setup();
    assert!(
        registration
            .process(accepted(&delivery))
            .await
            .is_continue()
    );
    let sealed = delivery.seal();
    assert!(sealed.terminal.is_none());
    assert!(
        registration
            .process(succeeded(&delivery, 2))
            .await
            .is_continue()
    );
    assert!(
        registration
            .process(succeeded(&delivery, 3))
            .await
            .is_continue()
    );
    assert!(matches!(
        receiver.recv().await.unwrap().item.observation,
        Observation::Accepted(_)
    ));
    let late = receiver.recv().await.unwrap();
    assert_eq!(late.item.vendor_turn.unwrap().as_str(), "msg_viaDelivery");
    if let Observation::LateTerminal(terminal) = late.item.observation {
        assert_eq!(terminal.usage, Some(UsageSample::default()));
    } else {
        panic!("expected late terminal");
    }
    assert!(receiver.try_recv().is_err());
}

#[test]
fn oc11_usage_requires_terminal_complete_delivery_and_clean_read_prefix() {
    assert!(honest_usage(true, true, 12, None));
    assert!(honest_usage(true, true, 12, Some(13)));
    for (terminal, complete, rejected) in [
        (false, true, None),
        (true, false, None),
        (true, true, Some(11)),
        (true, true, Some(12)),
    ] {
        assert!(!honest_usage(terminal, complete, 12, rejected));
    }
}

#[test]
fn protocol_health_does_not_attribute_failure_to_a_settled_turn() {
    let (registration, delivery, _receiver) = setup();
    delivery.seal();
    registration.health_failure(LaneFailure::Protocol);
    assert!(matches!(
        *registration.health.borrow(),
        DriverHealth::Failed {
            first_cause: DriverFailure::ObservationOverflow
        }
    ));
}

#[test]
fn generation_health_does_not_attribute_failure_to_a_settled_turn() {
    let (registration, delivery, _receiver) = setup();
    delivery.seal();
    registration.generation_health(GenerationEnd::Lost(via_routes::codex::ConnectionLoss {
        cause: LossCause::Protocol,
        cleanup: via_routes::WireCleanup::Quiescent,
        exit: None,
        journal_uncertain: false,
    }));
    assert!(matches!(
        *registration.health.borrow(),
        DriverHealth::Failed {
            first_cause: DriverFailure::ObservationOverflow
        }
    ));
}

fn joined_execution_events(router: &mut Router) {
    use via_routes::opencode::events::{InboxKind, StepKind, Tokens};
    let mut dispatch = |data| {
        router.dispatch(
            Event {
                id: None,
                session_id: Some("ses_joined".into()),
                seq: None,
                data,
            },
            Instant::now(),
        );
    };
    dispatch(EventData::Execution {
        kind: ExecutionKind::Started,
        error: None,
        reason: None,
    });
    for assistant in ["assistant_a", "assistant_b"] {
        dispatch(EventData::Step {
            kind: StepKind::Started,
            assistant_message_id: assistant.into(),
            finish: None,
            tokens: None,
            cost: None,
        });
    }
    dispatch(EventData::Inbox {
        kind: InboxKind::Delivered,
        id: "msg_viaJoined".into(),
    });
    for (assistant, text) in [("assistant_a", "old step"), ("assistant_b", "last step")] {
        dispatch(EventData::Text {
            kind: TextKind::Ended,
            assistant_message_id: assistant.into(),
            ordinal: 0,
            text: text.into(),
        });
    }
    dispatch(EventData::Step {
        kind: StepKind::Ended,
        assistant_message_id: "assistant_b".into(),
        finish: Some("stop".into()),
        tokens: Some(Tokens {
            input: Some(7),
            output: Some(3),
            ..Tokens::default()
        }),
        cost: Some(0.25),
    });
    dispatch(EventData::Execution {
        kind: ExecutionKind::Succeeded,
        error: None,
        reason: None,
    });
}

#[tokio::test]
async fn oc06_joined_execution_keeps_last_preownership_step_and_missing_samples() {
    let mut router = Router::new();
    let lane = router.attach("ses_joined");
    let (sink, mut receiver) = crate::observation::observation_channel();
    let registration = Registration::new(
        lane.clone(),
        sink,
        Arc::new(watch::Sender::new(DriverHealth::Open)),
        1,
    );
    let turn = TurnNumber::try_from(1).unwrap();
    let delivery = registration.admit(
        "msg_viaJoined".into(),
        turn,
        TurnActivity::new(Instant::now()),
        None,
    );
    router.register_turn("ses_joined", "msg_viaJoined".into(), turn);
    router.accepted("ses_joined", "msg_viaJoined", Instant::now());
    joined_execution_events(&mut router);
    let consumer = {
        let registration = registration.clone();
        tokio::spawn(async move {
            while let Some(item) = lane.pop() {
                assert!(registration.process(item).await.is_continue());
            }
        })
    };
    assert!(matches!(
        tokio::time::timeout(std::time::Duration::from_secs(1), delivery.decision())
            .await
            .unwrap(),
        Decision::Terminal
    ));
    let sealed = delivery.seal();
    consumer.await.unwrap();
    let mut final_text = Vec::new();
    while let Ok(admitted) = receiver.try_recv() {
        if let Observation::FinalText(text) = admitted.item.observation {
            final_text.push(text);
        }
    }
    assert_eq!(final_text, ["last step"]);
    assert!(
        sealed.accounted,
        "all attributed observations reached the sink"
    );
    let terminal = sealed.terminal.unwrap();
    assert_eq!(terminal.stop_reason, crate::StopReason::EndTurn);
    assert_eq!(
        terminal.usage.unwrap().input,
        None,
        "preownership assistant A has no call-end sample"
    );
    assert_eq!(terminal.cost, None);
}

fn permission_request(delivery: &Delivery, order: u64) -> LaneItem {
    event(
        delivery,
        order,
        EventData::Interactive {
            kind: InteractiveKind::Permission,
            id: "perm_proof".into(),
            action: Some("bash".into()),
            message_id: Some("assistant_proof".into()),
            call_id: Some("call_proof".into()),
        },
    )
}

fn permission_failure(delivery: &Delivery, order: u64) -> LaneItem {
    event(
        delivery,
        order,
        EventData::Tool {
            kind: ToolKind::Failed,
            assistant_message_id: Some("assistant_proof".into()),
            call_id: "call_proof".into(),
            tool: Some("bash".into()),
            error: Some(via_routes::opencode::events::VendorError {
                code: "permission.rejected".into(),
                status: None,
            }),
        },
    )
}

fn shutdown(delivery: &Delivery, order: u64) -> LaneItem {
    event(
        delivery,
        order,
        EventData::Execution {
            kind: ExecutionKind::Interrupted,
            error: None,
            reason: Some("shutdown".into()),
        },
    )
}

fn permission_declined(delivery: &Delivery, order: u64) -> LaneItem {
    LaneItem::Decline {
        staging: None,
        owner: delivery.turn,
        notice: Box::new(DeclineNotice {
            session_id: "ses_delivery".into(),
            id: "perm_proof".into(),
            kind: InteractiveKind::Permission,
            action: Some("bash".into()),
            call_id: Some("call_proof".into()),
            settlement: DeclineSettlement::Settled,
            outcome: Ok(DeclineOutcome::Declined),
        }),
        read_order: order,
        position: 0,
        decoded_at: Instant::now(),
    }
}

/// §11: a native effect may precede the HTTP proof that classifies it.
#[tokio::test]
async fn oc07_permission_http_proof_releases_deferred_tool_and_shutdown() {
    let (registration, delivery, mut receiver) = setup();
    assert!(
        registration
            .process(accepted(&delivery))
            .await
            .is_continue()
    );
    assert!(
        registration
            .process(permission_request(&delivery, 2))
            .await
            .is_continue()
    );
    assert!(
        registration
            .process(permission_request(&delivery, 3))
            .await
            .is_continue()
    );
    assert!(
        registration
            .process(permission_failure(&delivery, 4))
            .await
            .is_continue()
    );
    assert!(
        registration
            .process(shutdown(&delivery, 5))
            .await
            .is_continue()
    );
    assert!(
        delivery.terminal_at().is_none(),
        "classification awaits HTTP proof"
    );
    let consumer = {
        let registration = registration.clone();
        let notice = permission_declined(&delivery, 6);
        tokio::spawn(async move { registration.process(notice).await.is_continue() })
    };
    assert!(matches!(
        tokio::time::timeout(std::time::Duration::from_secs(1), delivery.decision())
            .await
            .unwrap(),
        Decision::Terminal
    ));
    let sealed = delivery.seal();
    assert!(consumer.await.unwrap());
    let terminal = sealed.terminal.unwrap();
    assert_eq!(terminal.status, VendorTerminalStatus::Completed);
    assert_eq!(terminal.stop_reason, crate::StopReason::Other);
    let mut declines = 0;
    let mut ended = 0;
    while let Ok(item) = receiver.try_recv() {
        match item.item.observation {
            Observation::RequestDeclined(_) => declines += 1,
            Observation::Progress(marks) => ended += marks.tools_ended.len(),
            Observation::ActionDenied(_) => panic!("VIA decline already accounts for this denial"),
            Observation::Accepted(_)
            | Observation::IdentityConfirmed(_)
            | Observation::FinalText(_)
            | Observation::SteerDelivered { .. }
            | Observation::Warning(_)
            | Observation::VendorClosed(_)
            | Observation::ResumeMismatch { .. }
            | Observation::LateTerminal(_) => {}
        }
    }
    assert_eq!((declines, ended), (1, 1));
}

/// §6, §11: even an early decline result must follow the acceptance observation.
#[tokio::test]
async fn oc07_early_permission_proof_preserves_acceptance_first() {
    let (registration, delivery, mut receiver) = setup();
    assert!(
        registration
            .process(permission_request(&delivery, 2))
            .await
            .is_continue()
    );
    assert!(
        registration
            .process(permission_failure(&delivery, 3))
            .await
            .is_continue()
    );
    assert!(
        registration
            .process(shutdown(&delivery, 4))
            .await
            .is_continue()
    );
    assert!(
        registration
            .process(permission_declined(&delivery, 5))
            .await
            .is_continue()
    );
    assert!(receiver.try_recv().is_err(), "nothing precedes acceptance");
    let consumer = {
        let registration = registration.clone();
        let acceptance = accepted(&delivery);
        tokio::spawn(async move { registration.process(acceptance).await.is_continue() })
    };
    assert!(matches!(
        tokio::time::timeout(std::time::Duration::from_secs(1), delivery.decision())
            .await
            .unwrap(),
        Decision::Terminal
    ));
    let sealed = delivery.seal();
    assert!(consumer.await.unwrap());
    assert!(matches!(
        receiver.recv().await.unwrap().item.observation,
        Observation::Accepted(_)
    ));
    assert_eq!(
        sealed.terminal.unwrap().status,
        VendorTerminalStatus::Completed
    );
}

/// §7.3, §10: losing the proof channel cannot erase an admitted native terminal.
#[tokio::test]
async fn oc07_deferred_shutdown_survives_loss_without_decline_proof() {
    let (registration, delivery, mut receiver) = setup();
    assert!(
        registration
            .process(accepted(&delivery))
            .await
            .is_continue()
    );
    assert!(
        registration
            .process(permission_request(&delivery, 2))
            .await
            .is_continue()
    );
    assert!(
        registration
            .process(permission_failure(&delivery, 3))
            .await
            .is_continue()
    );
    assert!(
        registration
            .process(shutdown(&delivery, 4))
            .await
            .is_continue()
    );
    let consumer = {
        let registration = registration.clone();
        tokio::spawn(async move { registration.flush_deferred().await.is_continue() })
    };
    assert!(matches!(
        tokio::time::timeout(std::time::Duration::from_secs(1), delivery.decision())
            .await
            .unwrap(),
        Decision::Terminal
    ));
    let sealed = delivery.seal();
    assert!(consumer.await.unwrap());
    let terminal = sealed.terminal.unwrap();
    assert_eq!(terminal.status, VendorTerminalStatus::Failed);
    assert_eq!(
        terminal.vendor_code.as_deref(),
        Some("interrupted:shutdown")
    );
    let mut denied = 0;
    while let Ok(item) = receiver.try_recv() {
        if matches!(item.item.observation, Observation::ActionDenied(_)) {
            denied += 1;
        }
    }
    assert_eq!(
        denied, 1,
        "missing204 never fabricates a VIA permission decline"
    );
}

/// §11: reversed HTTP completion cannot reopen a proof wait for a deferred ask.
#[tokio::test]
async fn oc07_reversed_permission_proofs_do_not_reopen_deferred_request() {
    let (registration, delivery, mut receiver) = setup();
    assert!(
        registration
            .process(accepted(&delivery))
            .await
            .is_continue()
    );
    assert!(
        registration
            .process(permission_request(&delivery, 2))
            .await
            .is_continue()
    );
    assert!(
        registration
            .process(permission_failure(&delivery, 3))
            .await
            .is_continue()
    );
    let mut second = permission_request(&delivery, 4);
    let LaneItem::Event(second_event) = &mut second else {
        unreachable!()
    };
    let EventData::Interactive { id, call_id, .. } = &mut second_event.event.data else {
        unreachable!()
    };
    *id = "perm_second".into();
    *call_id = Some("call_second".into());
    assert!(registration.process(second).await.is_continue());
    assert!(
        registration
            .process(shutdown(&delivery, 5))
            .await
            .is_continue()
    );
    let mut second_notice = permission_declined(&delivery, 6);
    let LaneItem::Decline { notice, .. } = &mut second_notice else {
        unreachable!()
    };
    notice.id = "perm_second".into();
    notice.call_id = Some("call_second".into());
    assert!(registration.process(second_notice).await.is_continue());
    let consumer = {
        let registration = registration.clone();
        let first_notice = permission_declined(&delivery, 7);
        tokio::spawn(async move { registration.process(first_notice).await.is_continue() })
    };
    assert!(matches!(
        tokio::time::timeout(std::time::Duration::from_secs(1), delivery.decision())
            .await
            .unwrap(),
        Decision::Terminal
    ));
    let sealed = delivery.seal();
    assert!(consumer.await.unwrap());
    assert_eq!(
        sealed.terminal.unwrap().status,
        VendorTerminalStatus::Completed
    );
    assert!(!waiting_permission(&delivery.lock()));
    let mut declines = 0;
    while let Ok(item) = receiver.try_recv() {
        if matches!(item.item.observation, Observation::RequestDeclined(_)) {
            declines += 1;
        }
    }
    assert_eq!(declines, 2);
}

/// §7.4: a post-cutoff native acknowledgement is late, even if retained before wakeup.
#[tokio::test]
async fn oc08_post_cutoff_retained_terminal_becomes_late_without_initial_ack() {
    let (registration, delivery, mut receiver) = setup();
    assert!(
        registration
            .process(accepted(&delivery))
            .await
            .is_continue()
    );
    drop(receiver.recv().await.unwrap());
    delivery.note_interrupt_sent();
    let cutoff = crate::Deadline::at(Instant::now() - std::time::Duration::from_secs(1));
    let consumer = {
        let registration = registration.clone();
        let interrupted = event(
            &delivery,
            2,
            EventData::Execution {
                kind: ExecutionKind::Interrupted,
                error: None,
                reason: Some("user".into()),
            },
        );
        tokio::spawn(async move { registration.process(interrupted).await.is_continue() })
    };
    assert!(matches!(
        tokio::time::timeout(std::time::Duration::from_secs(1), delivery.decision())
            .await
            .unwrap(),
        Decision::Terminal
    ));
    let sealed = delivery.seal_until(Some(cutoff));
    assert!(consumer.await.unwrap());
    assert!(
        sealed.terminal.is_none(),
        "post-cutoff evidence cannot acknowledge the initial turn"
    );
    assert!(!sealed.accounted);
    let late = receiver.try_recv().unwrap();
    assert!(matches!(
        late.item.observation,
        Observation::LateTerminal(VendorTerminal {
            status: VendorTerminalStatus::Interrupted,
            ..
        })
    ));
}

/// §11: native settlement before HTTP claim releases proof without inventing a VIA decline.
#[tokio::test]
async fn oc07_native_settlement_releases_deferred_shutdown_without_via_decline() {
    let (registration, delivery, mut receiver) = setup();
    assert!(
        registration
            .process(accepted(&delivery))
            .await
            .is_continue()
    );
    assert!(
        registration
            .process(permission_request(&delivery, 2))
            .await
            .is_continue()
    );
    assert!(
        registration
            .process(permission_failure(&delivery, 3))
            .await
            .is_continue()
    );
    assert!(
        registration
            .process(shutdown(&delivery, 4))
            .await
            .is_continue()
    );
    let mut native = permission_declined(&delivery, 5);
    let LaneItem::Decline { notice, .. } = &mut native else {
        unreachable!()
    };
    notice.outcome = Ok(DeclineOutcome::NativeSettled);
    let consumer = {
        let registration = registration.clone();
        tokio::spawn(async move { registration.process(native).await.is_continue() })
    };
    assert!(matches!(
        tokio::time::timeout(std::time::Duration::from_secs(1), delivery.decision())
            .await
            .unwrap(),
        Decision::Terminal
    ));
    let sealed = delivery.seal();
    assert!(consumer.await.unwrap());
    assert_eq!(
        sealed.terminal.unwrap().status,
        VendorTerminalStatus::Failed
    );
    let mut declined = 0;
    let mut denied = 0;
    while let Ok(item) = receiver.try_recv() {
        if matches!(item.item.observation, Observation::RequestDeclined(_)) {
            declined += 1;
        }
        if matches!(item.item.observation, Observation::ActionDenied(_)) {
            denied += 1;
        }
    }
    assert_eq!((declined, denied), (0, 1));
}

/// §8–§9: positive HTTP bounds evidence survives the bounded observation queue.
fn limit_owner(number: u32) -> (Router, Arc<Registration>, Arc<Delivery>) {
    let mut router = Router::new();
    let lane = router.attach("ses_delivery");
    let turn = TurnNumber::try_from(number).unwrap();
    let input = format!("msg_viaLimit{number}");
    if number > 1 {
        let old = TurnNumber::try_from(1).unwrap();
        router.register_turn("ses_delivery", "msg_viaOldLimit".into(), old);
        router.settle("ses_delivery", old);
    }
    router.register_turn("ses_delivery", input.clone(), turn);
    let (sink, _receiver) = crate::observation::observation_channel();
    let registration = Registration::new(
        lane,
        sink,
        Arc::new(watch::Sender::new(DriverHealth::Open)),
        7,
    );
    let delivery = registration.admit(input, turn, TurnActivity::new(Instant::now()), None);
    let mut state = delivery.lock();
    state.accepted = true;
    state.terminal = state.normalizer.terminal(
        &EventData::Execution {
            kind: ExecutionKind::Succeeded,
            error: None,
            reason: None,
        },
        Instant::now(),
    );
    drop(state);
    (router, registration, delivery)
}

fn fill_limit_lane(router: &mut Router) {
    for _ in 0..LANE_MESSAGES {
        router.dispatch(
            Event {
                id: None,
                session_id: Some("ses_delivery".into()),
                seq: None,
                data: EventData::Activity {
                    message_id: None,
                    call_id: None,
                },
            },
            Instant::now(),
        );
    }
}

#[test]
fn oc09_response_limit_survives_full_lane_before_outcome_seal() {
    let (mut router, _registration, delivery) = limit_owner(1);
    fill_limit_lane(&mut router);
    assert_eq!(delivery.lane.failure(), None);
    router.response_limit("ses_delivery", delivery.turn, Instant::now());
    assert_eq!(delivery.lane.failure(), Some(LaneFailure::Overflow));
    let sealed = delivery.seal();
    assert!(
        sealed.terminal.is_some(),
        "the raw terminal remains retained"
    );
    assert!(
        matches!(sealed.stop, Some(Stop::ResponseLimit)),
        "a queued marker loss cannot erase positive HTTP bounds evidence"
    );
}

#[test]
fn oc09_late_response_limit_on_old_owner_does_not_fail_successor() {
    let (mut router, _registration, delivery) = limit_owner(2);
    let old = TurnNumber::try_from(1).unwrap();
    fill_limit_lane(&mut router);
    router.response_limit("ses_delivery", old, Instant::now());
    assert!(!matches!(delivery.seal().stop, Some(Stop::ResponseLimit)));
}

#[test]
fn oc09_response_limit_after_force_cutoff_cannot_revise_initial_outcome() {
    let (mut router, _registration, delivery) = limit_owner(1);
    let cutoff = crate::Deadline::at(Instant::now());
    fill_limit_lane(&mut router);
    router.response_limit(
        "ses_delivery",
        delivery.turn,
        cutoff.instant() + std::time::Duration::from_secs(1),
    );
    assert!(!matches!(
        delivery.seal_until(Some(cutoff)).stop,
        Some(Stop::ResponseLimit)
    ));
}

#[test]
fn oc09_text_overflow_is_finite_without_driver_or_lane_failure() {
    let (registration, delivery, _receiver) = setup();
    delivery.text_overflow();
    assert_eq!(*registration.health.borrow(), DriverHealth::Open);
    assert_eq!(delivery.lane.failure(), None);
    let sealed = delivery.seal();
    assert!(matches!(sealed.stop, Some(Stop::TextOverflow)));
    assert!(sealed.loss.is_some());
    assert!(!sealed.accounted);
}

#[test]
fn oc09_text_overflow_usage_cannot_recover_with_a_later_terminal() {
    let (router, _registration, delivery) = limit_owner(1);
    delivery.text_overflow();
    delivery.lock().complete = true;
    let sealed = delivery.seal();
    assert!(matches!(sealed.stop, Some(Stop::TextOverflow)));
    assert!(
        sealed.terminal.is_some(),
        "retained raw evidence is preserved"
    );
    assert!(
        !sealed.accounted,
        "missing text observations prevent honest call usage"
    );
    assert_eq!(router.failure(), None);
    assert!(
        !router.needs_drain(),
        "a text cap affects only its owning turn"
    );
}

#[tokio::test]
async fn oc09_processed_response_limit_after_force_cutoff_is_not_initial_protocol() {
    let (mut router, registration, delivery) = limit_owner(1);
    let cutoff = crate::Deadline::at(Instant::now());
    router.response_limit(
        "ses_delivery",
        delivery.turn,
        cutoff.instant() + std::time::Duration::from_secs(1),
    );
    assert!(
        registration
            .process(delivery.lane.pop().unwrap())
            .await
            .is_continue()
    );
    let sealed = delivery.seal_until(Some(cutoff));
    assert!(
        !matches!(sealed.stop, Some(Stop::ResponseLimit)),
        "processing a post-force cap cannot revise the initial outcome"
    );
}

#[tokio::test]
async fn oc09_processed_late_response_limit_preserves_earlier_text_overflow() {
    let (mut router, registration, delivery) = limit_owner(1);
    delivery.text_overflow();
    let cutoff = crate::Deadline::at(Instant::now());
    router.response_limit(
        "ses_delivery",
        delivery.turn,
        cutoff.instant() + std::time::Duration::from_secs(1),
    );
    assert!(
        registration
            .process(delivery.lane.pop().unwrap())
            .await
            .is_continue()
    );
    let sealed = delivery.seal_until(Some(cutoff));
    assert!(
        matches!(sealed.stop, Some(Stop::TextOverflow)),
        "post-force HTTP bounds evidence cannot erase an earlier finite overflow"
    );
}
