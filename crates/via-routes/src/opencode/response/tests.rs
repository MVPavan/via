//! Loopback HTTP limit fixtures (`opencode.md` §8, §9); no process is launched.

use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::time::Instant;
use via_wire::http::{HttpClient, HttpError, HttpFailure, Sent};
use via_wire::json_limits;

use crate::Deadline;
use crate::opencode::{declines, events::InteractiveKind, session, turn};

#[derive(Clone, Copy, Debug)]
enum Request {
    Prompt,
    Interrupt,
    InputCancel,
    Inbox,
    Create,
    Get,
    PutInstructions,
    Instructions,
    SwitchModel,
    Catalog,
    Permission,
    Form,
}

const REQUESTS: [Request; 12] = [
    Request::Prompt,
    Request::Interrupt,
    Request::InputCancel,
    Request::Inbox,
    Request::Create,
    Request::Get,
    Request::PutInstructions,
    Request::Instructions,
    Request::SwitchModel,
    Request::Catalog,
    Request::Permission,
    Request::Form,
];

async fn reply(status: u16, body: Vec<u8>) -> (HttpClient, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let client = HttpClient::new(
        listener.local_addr().unwrap().port(),
        "opencode",
        "synthetic",
    );
    let peer = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut head = Vec::new();
        let mut byte = [0; 1];
        while !head.ends_with(b"\r\n\r\n") {
            socket.read_exact(&mut byte).await.unwrap();
            head.push(byte[0]);
        }
        let length = String::from_utf8_lossy(&head)
            .lines()
            .find_map(|line| line.strip_prefix("Content-Length: "))
            .map_or(0, |length| length.parse::<usize>().unwrap());
        let mut request_body = vec![0; length];
        socket.read_exact(&mut request_body).await.unwrap();
        let head = format!(
            "HTTP/1.1 {status} Test\r\nContent-Length: {}\r\n\r\n",
            body.len()
        );
        socket.write_all(head.as_bytes()).await.unwrap();
        // The bounded client may close as soon as its cap is known.
        let _closed_after_limit = socket.write_all(&body).await;
    });
    (client, peer)
}

fn setup_http(error: &session::SetupError) -> Option<HttpError> {
    match error {
        session::SetupError::Http(error) => Some(*error),
        session::SetupError::Status { .. } | session::SetupError::Malformed => None,
    }
}

async fn perform(client: &HttpClient, request: Request) -> Option<HttpError> {
    let by = Deadline::at(Instant::now() + Duration::from_secs(2));
    let model = session::ModelRef {
        provider_id: "synthetic".into(),
        id: "fixture".into(),
        variant: None,
    };
    match request {
        Request::Prompt => turn::prompt(client, "ses_one", "msg_one", "fixture", by)
            .await
            .err(),
        Request::Interrupt => turn::interrupt(client, "ses_one", by).await.err(),
        Request::InputCancel => turn::cancel_input(client, "ses_one", "msg_one", by)
            .await
            .err(),
        Request::Inbox => turn::inbox(client, "ses_one", by)
            .await
            .err()
            .and_then(|error| setup_http(&error)),
        Request::Create => session::create(
            client,
            session::NewSession {
                model: &model,
                agent: "build",
                directory: "/tmp",
                permissions: &[],
            },
            by,
        )
        .await
        .err()
        .and_then(|error| setup_http(&error)),
        Request::Get => session::get(client, "ses_one", by)
            .await
            .err()
            .and_then(|error| setup_http(&error)),
        Request::PutInstructions => session::put_instructions(client, "ses_one", "fixture", by)
            .await
            .err()
            .and_then(|error| setup_http(&error)),
        Request::Instructions => session::instructions(client, "ses_one", by)
            .await
            .err()
            .and_then(|error| setup_http(&error)),
        Request::SwitchModel => session::switch_model(client, "ses_one", &model, by)
            .await
            .err()
            .and_then(|error| setup_http(&error)),
        Request::Catalog => session::catalog_at(client, "/tmp", by)
            .await
            .err()
            .and_then(|error| setup_http(&error)),
        Request::Permission | Request::Form => declines::decline(
            client,
            "ses_one",
            "request_one",
            match request {
                Request::Permission => InteractiveKind::Permission,
                Request::Form => InteractiveKind::Form,
                Request::Prompt
                | Request::Interrupt
                | Request::InputCancel
                | Request::Inbox
                | Request::Create
                | Request::Get
                | Request::PutInstructions
                | Request::Instructions
                | Request::SwitchModel
                | Request::Catalog => unreachable!(),
            },
            by,
        )
        .await
        .err(),
    }
}

fn nested(depth: usize) -> Vec<u8> {
    format!("{}null{}", "[".repeat(depth), "]".repeat(depth)).into_bytes()
}

fn nodes(count: usize) -> Vec<u8> {
    format!("[{}]", vec!["null"; count - 1].join(",")).into_bytes()
}

#[tokio::test]
async fn oc09_every_request_kind_preserves_json_depth_limit_evidence() {
    let mut erased = Vec::new();
    for request in REQUESTS {
        let (client, peer) = reply(200, nested(json_limits::MAX_DEPTH + 1)).await;
        let failure = perform(&client, request).await;
        peer.await.unwrap();
        if failure.is_none() {
            erased.push(request);
        }
    }
    assert!(
        erased.is_empty(),
        "request kinds erasing depth-limit evidence: {erased:?}"
    );
}

#[tokio::test]
async fn oc09_every_request_kind_preserves_json_node_limit_evidence() {
    let mut erased = Vec::new();
    for request in REQUESTS {
        let (client, peer) = reply(409, nodes(json_limits::MAX_NODES + 1)).await;
        let failure = perform(&client, request).await;
        peer.await.unwrap();
        if failure.is_none() {
            erased.push(request);
        }
    }
    assert!(
        erased.is_empty(),
        "request kinds erasing node-limit evidence: {erased:?}"
    );
}

#[tokio::test]
async fn oc09_exact_json_limits_and_ordinary_malformed_are_not_limit_failures() {
    for body in [
        nested(json_limits::MAX_DEPTH),
        nodes(json_limits::MAX_NODES),
        b"not-json".to_vec(),
    ] {
        for request in REQUESTS {
            let (client, peer) = reply(200, body.clone()).await;
            let failure = perform(&client, request).await;
            peer.await.unwrap();
            assert!(
                failure.is_none(),
                "{request:?} classified bounded input as an HTTP limit"
            );
        }
    }
}

#[tokio::test]
async fn oc09_401_precedes_json_response_limits_for_every_request_kind() {
    for request in REQUESTS {
        let (client, peer) = reply(401, nested(json_limits::MAX_DEPTH + 1)).await;
        let failure = perform(&client, request).await;
        peer.await.unwrap();
        assert!(
            failure.is_none(),
            "{request:?} erased the decisive 401 status"
        );
    }
}

#[test]
fn oc09_http_limit_evidence_keeps_401_priority_and_other_statuses() {
    for kind in [
        HttpFailure::HeadersTooLarge,
        HttpFailure::BodyTooLarge { length: None },
        HttpFailure::JsonLimit(json_limits::LimitError::Depth),
        HttpFailure::JsonLimit(json_limits::LimitError::Nodes),
    ] {
        for status in [None, Some(200), Some(401), Some(500)] {
            let error = HttpError {
                sent: Sent::Maybe,
                kind,
                response_status: status,
            };
            let checked = super::checked(Err(error));
            if status == Some(401) {
                assert_eq!(checked.unwrap().status, 401);
            } else {
                assert_eq!(checked.unwrap_err(), error);
            }
        }
    }
}
