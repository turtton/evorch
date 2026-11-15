//! The Unix socket transport against a real socket pair.

mod support;

use support::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use workbench_client::{TransportError, UnixTransport, WorkbenchClient};
use workbench_protocol::envelope::*;
use workbench_protocol::framing::{FrameDecoder, decode_payload, encode_frame};
use workbench_protocol::handshake::*;

/// A minimal core: answers the handshake, then every command with the same reply.
async fn serve(mut stream: UnixStream, hello: ServerHello) {
    let mut decoder = FrameDecoder::new(hello.limits.max_frame_bytes);
    let mut buffer = [0u8; 4096];
    let mut accepted = false;
    loop {
        while let Some(payload) = decoder.next_payload().unwrap() {
            if !accepted {
                let _: ClientHello = decode_payload(&payload).expect("hello first");
                let reply =
                    encode_frame(&HandshakeReply::Accepted(hello.clone()), u32::MAX).unwrap();
                stream.write_all(&reply).await.unwrap();
                accepted = true;
                continue;
            }
            let request: RequestEnvelope = decode_payload(&payload).unwrap();
            let reply = ServerFrame::Reply(ReplyEnvelope {
                request_id: request.request_id,
                result: ok(ReplyPayload::Command(command_reply())),
            });
            let frame = encode_frame(&reply, u32::MAX).unwrap();
            // One byte per write: the client must reassemble a real stream.
            for byte in frame {
                stream.write_all(&[byte]).await.unwrap();
                stream.flush().await.unwrap();
            }
        }
        let read = stream.read(&mut buffer).await.unwrap();
        if read == 0 {
            return;
        }
        decoder.push(&buffer[..read]);
    }
}

#[tokio::test]
async fn commands_round_trip_over_a_socket_pair() {
    let (near, far) = UnixStream::pair().unwrap();
    let core = tokio::spawn(serve(far, server_hello(WireLimits::default())));
    let client = WorkbenchClient::connect(UnixTransport::new(near), config())
        .await
        .unwrap();
    for name in ["a", "b", "c"] {
        assert_eq!(
            client.command(stop_chat(name)).await.unwrap(),
            command_reply()
        );
    }
    drop(client);
    core.await.unwrap();
}

#[tokio::test]
async fn connecting_to_a_socket_path_works_and_a_missing_one_is_a_disconnect() {
    let dir = test_socket_dir::SocketDir::new();
    let path = dir.path().join("core.sock");
    assert_eq!(
        UnixTransport::connect(&path).await.err(),
        Some(TransportError::Disconnected)
    );

    let listener = UnixListener::bind(&path).unwrap();
    let core = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        serve(stream, server_hello(WireLimits::default())).await;
    });
    let transport = UnixTransport::connect(&path).await.unwrap();
    let client = WorkbenchClient::connect(transport, config()).await.unwrap();
    client.command(stop_chat("t")).await.unwrap();
    drop(client);
    core.await.unwrap();
}

#[tokio::test]
async fn a_peer_that_closes_the_socket_is_a_disconnect() {
    let (near, far) = UnixStream::pair().unwrap();
    let core = tokio::spawn(async move {
        let mut far = far;
        // Take the hello, then hang up instead of answering.
        let mut buffer = [0u8; 4096];
        let _ = far.read(&mut buffer).await.unwrap();
    });
    let result = WorkbenchClient::connect(UnixTransport::new(near), config()).await;
    core.await.unwrap();
    assert_eq!(
        result.err(),
        Some(workbench_client::ClientError::Transport(
            TransportError::Disconnected
        ))
    );
}
