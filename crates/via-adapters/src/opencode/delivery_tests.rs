use super::*;
use via_routes::opencode::events::{Event, ExecutionKind, TextKind};
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
        owner: delivery.turn,
        input_id: delivery.input.as_str().into(),
        read_order: 1,
        position: 0,
        decoded_at: Instant::now(),
    }
}

fn event(delivery: &Delivery, order: u64, data: EventData) -> LaneItem {
    LaneItem::Event(Box::new(Routed {
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
async fn oc10_retained_terminal_survives_later_lane_failure() {
    let (registration, delivery, mut receiver) = setup();
    assert!(registration.process(accepted(&delivery)).await);
    assert!(matches!(
        receiver.recv().await.unwrap().item.observation,
        Observation::Accepted(_)
    ));
    let consumer = {
        let registration = registration.clone();
        let terminal = succeeded(&delivery, 2);
        tokio::spawn(async move { registration.process(terminal).await })
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
    assert!(!registration.process(accepted(&delivery)).await);
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
    assert!(registration.process(accepted(&delivery)).await);
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
    assert!(registration.process(step).await);
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
    assert!(registration.process(text).await);
    drop(receiver);
    assert!(!registration.process(succeeded(&delivery, 4)).await);
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
    assert!(registration.process(early).await);
    assert!(receiver.try_recv().is_err());
    let mut acceptance = accepted(&delivery);
    if let LaneItem::Accepted { read_order, .. } = &mut acceptance {
        *read_order = 2;
    }
    assert!(registration.process(acceptance).await);
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
    assert!(registration.process(accepted(&delivery)).await);
    let sealed = delivery.seal();
    assert!(sealed.terminal.is_none());
    assert!(registration.process(succeeded(&delivery, 2)).await);
    assert!(registration.process(succeeded(&delivery, 3)).await);
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

fn joined_execution_events(router: &mut Router) {
    use via_routes::opencode::events::{InboxKind, StepKind, Tokens};
    let mut dispatch = |data| {
        router
            .dispatch(
                Event {
                    id: None,
                    session_id: Some("ses_joined".into()),
                    seq: None,
                    data,
                },
                Instant::now(),
            )
            .unwrap();
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
                assert!(registration.process(item).await);
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
