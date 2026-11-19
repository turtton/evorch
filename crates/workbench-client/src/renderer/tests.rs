use super::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixListener;
use workbench_protocol::namespaces::browser::data::{BlobSource, CHUNK_BYTES, MediaType};

fn id(byte: char) -> RendererId {
    RendererId::new(byte.to_string().repeat(32)).unwrap()
}

fn frame_start(sequence: u64, width: u32, height: u32) -> FrameStart {
    FrameStart {
        core_epoch: 1,
        generation: 2,
        tab: id('a'),
        sequence,
        width,
        height,
        stride: width * 4,
        total_bytes: u64::from(width) * u64::from(height) * 4,
    }
}

fn chunks(id: u64, bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    for (index, chunk) in bytes.chunks(CHUNK_BYTES).enumerate() {
        let meta = ChunkMeta {
            id,
            offset: (index * CHUNK_BYTES) as u64,
        };
        out.extend(meta.encode(chunk));
    }
    out.extend(
        EndMeta {
            id,
            total_bytes: bytes.len() as u64,
        }
        .encode(),
    );
    out
}

fn frame_stream(stream: UnixStream) -> FrameStream {
    FrameStream::new(stream)
}

async fn blob_stream(stream: UnixStream) -> Result<BlobStream, DataClientError> {
    BlobStream::open(stream).await
}

fn blob_start(total: u64, source: BlobSource) -> BlobStart {
    BlobStart {
        core_epoch: 1,
        transfer_id: 9,
        source,
        media_type: MediaType::Png,
        total_bytes: total,
        digest: if source == BlobSource::Artifact {
            [7; 32]
        } else {
            [0; 32]
        },
    }
}

async fn read_ack(far: &mut UnixStream) -> u64 {
    let mut prefix = [0u8; PREFIX_LEN];
    far.read_exact(&mut prefix).await.unwrap();
    let prefix = Prefix::parse(&prefix).unwrap();
    assert_eq!(prefix.kind, Kind::Ack);
    let mut meta = vec![0u8; prefix.meta_len];
    far.read_exact(&mut meta).await.unwrap();
    AckMeta::decode(&meta).unwrap().id
}

#[tokio::test]
async fn connect_sends_the_hello_and_reads_the_offer() {
    let dir = test_socket_dir::SocketDir::new();
    let path = dir.path().join("core.sock");
    let listener = UnixListener::bind(&path).unwrap();
    let core = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut hello = [0u8; 4 + PREFIX_LEN];
        stream.read_exact(&mut hello).await.unwrap();
        assert_eq!(hello.as_slice(), encode_hello().as_slice());
        let offer = ChannelOffer {
            channel: id('c'),
            bind_window_ms: 30_000,
        };
        stream.write_all(&offer.encode()).await.unwrap();
    });
    let channel = DataChannel::connect(&path).await.unwrap();
    assert_eq!(channel.channel_id(), &id('c'));
    assert_eq!(channel.bind_window_ms(), 30_000);
    core.await.unwrap();
}

#[tokio::test]
async fn an_error_instead_of_an_offer_names_the_reason() {
    let dir = test_socket_dir::SocketDir::new();
    let path = dir.path().join("core.sock");
    let listener = UnixListener::bind(&path).unwrap();
    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut hello = [0u8; 4 + PREFIX_LEN];
        stream.read_exact(&mut hello).await.unwrap();
        let error = ErrorMeta {
            code: ErrorCode::Busy,
        };
        stream.write_all(&error.encode()).await.unwrap();
    });
    let error = DataChannel::connect(&path).await.err().unwrap();
    assert!(matches!(error, DataClientError::Remote(ErrorCode::Busy)));
}

#[tokio::test]
async fn frames_are_rebuilt_exactly_and_acknowledged_one_by_one() {
    let (near, mut far) = UnixStream::pair().unwrap();
    let mut frames = frame_stream(near);
    let first: Vec<u8> = (0..4 * 3 * 2).map(|i| i as u8).collect();
    let second = vec![9u8; 4 * 3 * 2];
    let core = tokio::spawn(async move {
        for (sequence, bytes) in [(5u64, first.clone()), (8, second.clone())] {
            far.write_all(&frame_start(sequence, 3, 2).encode())
                .await
                .unwrap();
            far.write_all(&chunks(sequence, &bytes)).await.unwrap();
            assert_eq!(read_ack(&mut far).await, sequence);
        }
        (first, second)
    });
    let one = frames.next_frame().await.unwrap().unwrap();
    let two = frames.next_frame().await.unwrap().unwrap();
    let (first, second) = core.await.unwrap();
    assert_eq!((one.start.sequence, one.bgra), (5, first));
    assert_eq!((two.start.sequence, two.bgra), (8, second));
    assert!(frames.next_frame().await.unwrap().is_none());
}

#[tokio::test]
async fn a_frame_larger_than_the_limit_is_refused_before_any_allocation() {
    let (near, mut far) = UnixStream::pair().unwrap();
    let mut frames = frame_stream(near);
    let mut huge = frame_start(1, 4000, 2200);
    huge.total_bytes = u64::MAX;
    far.write_all(&huge.encode()).await.unwrap();
    let mut bytes = Vec::new();
    let error = frames.next_frame_into(&mut bytes).await.err().unwrap();
    assert!(matches!(error, DataClientError::Protocol(_)), "{error:?}");
    assert_eq!(bytes.capacity(), 0, "nothing was reserved");
}

#[tokio::test]
async fn a_repeated_or_older_sequence_ends_the_stream() {
    let (near, mut far) = UnixStream::pair().unwrap();
    let mut frames = frame_stream(near);
    let bytes = vec![1u8; 4];
    for sequence in [4u64, 4] {
        far.write_all(&frame_start(sequence, 1, 1).encode())
            .await
            .unwrap();
        far.write_all(&chunks(sequence, &bytes)).await.unwrap();
    }
    assert!(frames.next_frame().await.unwrap().is_some());
    let _ = read_ack(&mut far).await;
    assert!(matches!(
        frames.next_frame().await,
        Err(DataClientError::Protocol(DataError::OutOfOrder(_)))
    ));
}

#[tokio::test]
async fn a_gap_a_foreign_chunk_or_a_short_end_is_a_violation() {
    let bytes = [3u8; 8];
    let gap = ChunkMeta { id: 1, offset: 4 }.encode(&bytes[..4]);
    let foreign = ChunkMeta { id: 2, offset: 0 }.encode(&bytes[..4]);
    let short = [
        ChunkMeta { id: 1, offset: 0 }.encode(&bytes[..4]),
        EndMeta {
            id: 1,
            total_bytes: 8,
        }
        .encode(),
    ]
    .concat();
    for wire in [gap, foreign, short] {
        let (near, mut far) = UnixStream::pair().unwrap();
        let mut frames = frame_stream(near);
        far.write_all(&frame_start(1, 2, 1).encode()).await.unwrap();
        far.write_all(&wire).await.unwrap();
        assert!(matches!(
            frames.next_frame().await,
            Err(DataClientError::Protocol(_))
        ));
    }
}

#[tokio::test]
async fn a_connection_that_ends_inside_a_frame_is_truncated() {
    let (near, mut far) = UnixStream::pair().unwrap();
    let mut frames = frame_stream(near);
    far.write_all(&frame_start(1, 2, 1).encode()).await.unwrap();
    drop(far);
    assert!(matches!(
        frames.next_frame().await,
        Err(DataClientError::Truncated)
    ));
}

#[tokio::test]
async fn a_remote_error_ends_the_frames_with_its_code() {
    let (near, mut far) = UnixStream::pair().unwrap();
    let mut frames = frame_stream(near);
    let error = ErrorMeta {
        code: ErrorCode::Closed,
    };
    far.write_all(&error.encode()).await.unwrap();
    assert!(matches!(
        frames.next_frame().await,
        Err(DataClientError::Remote(ErrorCode::Closed))
    ));
}

#[tokio::test]
async fn a_blob_arrives_in_bounded_chunks_and_is_acknowledged_once_whole() {
    let (near, mut far) = UnixStream::pair().unwrap();
    let image: Vec<u8> = (0..CHUNK_BYTES * 2 + 17).map(|i| (i % 251) as u8).collect();
    let wire = [
        blob_start(image.len() as u64, BlobSource::Artifact).encode(),
        chunks(9, &image),
    ]
    .concat();
    let core = tokio::spawn(async move {
        far.write_all(&wire).await.unwrap();
        read_ack(&mut far).await
    });
    let mut blob = blob_stream(near).await.unwrap();
    assert_eq!(blob.start().total_bytes, image.len() as u64);
    let mut got = Vec::new();
    while let Some(chunk) = blob.next_chunk().await.unwrap() {
        assert!(chunk.len() <= CHUNK_BYTES);
        got.extend_from_slice(chunk);
    }
    assert_eq!(got, image);
    assert_eq!(core.await.unwrap(), 9);
    assert!(blob.next_chunk().await.unwrap().is_none());
}

#[tokio::test]
async fn read_to_end_refuses_a_blob_above_the_callers_limit() {
    let (near, mut far) = UnixStream::pair().unwrap();
    far.write_all(&blob_start(1000, BlobSource::Capture).encode())
        .await
        .unwrap();
    let blob = blob_stream(near).await.unwrap();
    assert!(matches!(
        blob.read_to_end(999).await,
        Err(DataClientError::TooLarge {
            len: 1000,
            max: 999
        })
    ));
}

#[tokio::test]
async fn a_blob_header_that_breaks_a_limit_or_a_rule_is_refused() {
    let mut empty = blob_start(1, BlobSource::Capture);
    empty.total_bytes = 0;
    let mut digestless = blob_start(10, BlobSource::Artifact);
    digestless.digest = [0; 32];
    for start in [empty, digestless] {
        let (near, mut far) = UnixStream::pair().unwrap();
        // The encoder does not validate, the receiver must.
        far.write_all(&start.encode()).await.unwrap();
        assert!(matches!(
            blob_stream(near).await,
            Err(DataClientError::Protocol(_))
        ));
    }
}

#[tokio::test]
async fn a_denied_bind_surfaces_the_core_error_not_a_blob() {
    let (near, mut far) = UnixStream::pair().unwrap();
    let error = ErrorMeta {
        code: ErrorCode::Denied,
    };
    far.write_all(&error.encode()).await.unwrap();
    assert!(matches!(
        blob_stream(near).await,
        Err(DataClientError::Remote(ErrorCode::Denied))
    ));
}

#[tokio::test]
async fn a_chunk_that_breaks_the_transfer_is_refused_before_its_body_is_read() {
    let (near, mut far) = UnixStream::pair().unwrap();
    let mut frames = frame_stream(near);
    // The frame has 4 bytes; the chunk announces 64 KiB and its body never comes.
    far.write_all(&frame_start(1, 1, 1).encode()).await.unwrap();
    far.write_all(&ChunkMeta { id: 1, offset: 0 }.encode_head(CHUNK_BYTES))
        .await
        .unwrap();
    let mut bytes = Vec::new();
    let error = poll_once(frames.next_frame_into(&mut bytes))
        .await
        .expect("refused without waiting for the body")
        .err()
        .unwrap();
    assert!(matches!(error, DataClientError::Protocol(_)), "{error:?}");
    assert!(bytes.capacity() < CHUNK_BYTES, "{}", bytes.capacity());

    let (near, mut far) = UnixStream::pair().unwrap();
    let mut blob = blob_stream_for(&mut far, near, 4).await;
    far.write_all(&ChunkMeta { id: 9, offset: 0 }.encode_head(CHUNK_BYTES))
        .await
        .unwrap();
    let error = poll_once(blob.next_chunk())
        .await
        .expect("refused without waiting for the body")
        .err()
        .unwrap();
    assert!(matches!(error, DataClientError::Protocol(_)), "{error:?}");
    assert!(blob.body.capacity() < CHUNK_BYTES);
}

async fn blob_stream_for(far: &mut UnixStream, near: UnixStream, total: u64) -> BlobStream {
    far.write_all(&blob_start(total, BlobSource::Capture).encode())
        .await
        .unwrap();
    blob_stream(near).await.unwrap()
}

#[tokio::test]
async fn a_frame_of_another_core_generation_or_tab_is_refused() {
    let variants: [fn(&mut FrameStart); 3] = [
        |start| start.core_epoch = 9,
        |start| start.generation = 3,
        |start| start.tab = id('b'),
    ];
    for change in variants {
        let (near, mut far) = UnixStream::pair().unwrap();
        let mut frames = frame_stream(near);
        let bytes = [1u8; 4];
        far.write_all(&frame_start(1, 1, 1).encode()).await.unwrap();
        far.write_all(&chunks(1, &bytes)).await.unwrap();
        assert!(frames.next_frame().await.unwrap().is_some());
        let _ = read_ack(&mut far).await;
        let mut other = frame_start(2, 1, 1);
        change(&mut other);
        far.write_all(&other.encode()).await.unwrap();
        far.write_all(&chunks(2, &bytes)).await.unwrap();
        assert!(matches!(
            frames.next_frame().await,
            Err(DataClientError::Protocol(DataError::OutOfOrder(_)))
        ));
    }
}

#[tokio::test]
async fn a_stream_is_closed_by_an_error_and_by_a_cancelled_read() {
    // A violation closes the connection: the next valid frame is not read.
    let (near, mut far) = UnixStream::pair().unwrap();
    let mut frames = frame_stream(near);
    let mut bad = frame_start(1, 1, 1);
    bad.stride = 8;
    far.write_all(&bad.encode()).await.unwrap();
    assert!(frames.next_frame().await.is_err());
    // The connection is closed, so these writes may already fail.
    let _ = far.write_all(&frame_start(2, 1, 1).encode()).await;
    let _ = far.write_all(&chunks(2, &[1u8; 4])).await;
    assert!(matches!(
        frames.next_frame().await,
        Err(DataClientError::Closed)
    ));
    // The core's side of the connection sees it end.
    let mut rest = Vec::new();
    far.read_to_end(&mut rest).await.unwrap();
    assert!(rest.is_empty(), "nothing is acknowledged");

    // A read that is cancelled half way leaves a position nobody can trust.
    let (near, mut far) = UnixStream::pair().unwrap();
    let mut frames = frame_stream(near);
    far.write_all(&frame_start(1, 1, 1).encode()).await.unwrap();
    let cancelled = poll_once(frames.next_frame()).await;
    assert!(cancelled.is_none(), "the read was waiting for the chunks");
    ended_by_the_other_side(&mut far).await;
    let _ = far.write_all(&chunks(1, &[1u8; 4])).await;
    assert!(matches!(
        frames.next_frame().await,
        Err(DataClientError::Closed)
    ));

    // The same for a blob.
    let (near, mut far) = UnixStream::pair().unwrap();
    let mut blob = blob_stream_for(&mut far, near, 4).await;
    let cancelled = poll_once(blob.next_chunk()).await;
    assert!(cancelled.is_none());
    ended_by_the_other_side(&mut far).await;
    let _ = far.write_all(&chunks(9, &[1u8; 4])).await;
    assert!(matches!(
        blob.next_chunk().await,
        Err(DataClientError::Closed)
    ));
}

/// The peer sees the connection end (an end of stream or a reset) without the
/// stream being used again: dropping a read's future closed it. The wait is
/// bounded by `poll_once`, so a regression fails instead of hanging the test.
async fn ended_by_the_other_side(far: &mut UnixStream) {
    let mut rest = Vec::new();
    let ended = poll_once(far.read_to_end(&mut rest))
        .await
        .expect("the cancelled read left its connection open");
    if ended.is_ok() {
        assert!(rest.is_empty());
    }
}

/// Polls `future` while the runtime has other work to do and drops it if it is
/// still waiting after that: the read is then cancelled.
async fn poll_once<F: std::future::Future>(future: F) -> Option<F::Output> {
    let mut future = std::pin::pin!(future);
    for _ in 0..1000 {
        let polled =
            std::future::poll_fn(|cx| std::task::Poll::Ready(future.as_mut().poll(cx))).await;
        if let std::task::Poll::Ready(value) = polled {
            return Some(value);
        }
        tokio::task::yield_now().await;
    }
    None
}
