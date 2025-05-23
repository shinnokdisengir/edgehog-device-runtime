// Copyright 2024 SECO Mind Srl
// SPDX-License-Identifier: Apache-2.0

use edgehog_device_forwarder_proto::http::Message as HttpMessage;
use edgehog_device_forwarder_proto::{
    http::Response as HttpResponse, message::Protocol, prost::Message, Http,
    Message as ProtoMessage,
};
use edgehog_device_runtime_forwarder::test_utils::create_http_req;
use futures::{SinkExt, StreamExt};

#[tokio::test]
async fn test_connect() {
    use edgehog_device_runtime_forwarder::test_utils::TestConnections;

    // TODO: in the feature this will change, for now just set the default to make the tests pass
    // Set default crypto provider
    if rustls::crypto::CryptoProvider::get_default().is_none() {
        let _ = rustls::crypto::aws_lc_rs::default_provider()
            .install_default()
            .inspect_err(|_| tracing::error!("couldn't install default crypto provider"));
    }

    let test_connections = TestConnections::<httpmock::MockServer>::init().await;
    let endpoint = test_connections.endpoint();

    let test_url = test_connections
        .mock_server
        .url("/remote-terminal?session=abcd");

    let mut ws = test_connections.mock_ws_server().await;

    let request_id = "3647edbb-6747-4827-a3ef-dbb6239e3326".as_bytes().to_vec();
    let http_req = create_http_req(request_id, &test_url, Vec::new());

    ws.send(http_req.clone())
        .await
        .expect("failed to send over ws");

    // send again another request with the same ID. This should cause an IdAlreadyUsed error
    // which is internally handled (print in debug mode)
    ws.send(http_req).await.expect("failed to send over ws");

    // the 1st request is correctly handled
    let http_res = ws
        .next()
        .await
        .expect("ws already closed")
        .expect("failed to receive from ws")
        .into_data();

    let proto_res =
        ProtoMessage::decode(http_res).expect("failed to decode tung message into protobuf");

    assert!(matches!(
        proto_res,
        ProtoMessage {
            protocol: Some(Protocol::Http(Http {
                message: Some(HttpMessage::Response(HttpResponse {
                    status_code: 200,
                    ..
                })),
                ..
            })),
        }
    ));

    ws.close(None).await.expect("failed to close ws");

    endpoint.assert();
    test_connections.assert().await;
}

#[tokio::test]
async fn test_max_sizes() {
    use edgehog_device_runtime_forwarder::test_utils::TestConnections;

    let test_connections = TestConnections::<httpmock::MockServer>::init().await;
    let mut ws = test_connections.mock_ws_server().await;

    let request_id = "3647edbb-6747-4827-a3ef-dbb6239e3326".as_bytes().to_vec();
    let test_url = test_connections
        .mock_server
        .url("/remote-terminal?session=abcd");
    // create an HTTP request with a big body
    let http_req = create_http_req(request_id.clone(), &test_url, vec![0u8; 16777216]);

    // sending a frame bigger than the maximum frame size will cause a connection reset error.
    let res = ws.send(http_req.clone()).await;
    assert!(res.is_err(), "expected error {res:?}");
}
