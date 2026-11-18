use super::*;

fn id(c: char) -> RendererId {
    RendererId::new(c.to_string().repeat(32)).unwrap()
}

fn split(record: &[u8]) -> (Prefix, &[u8], &[u8]) {
    let prefix = Prefix::parse(record[..PREFIX_LEN].try_into().unwrap()).unwrap();
    let meta = &record[PREFIX_LEN..PREFIX_LEN + prefix.meta_len];
    let body = &record[PREFIX_LEN + prefix.meta_len..];
    assert_eq!(body.len(), prefix.body_len);
    (prefix, meta, body)
}

fn frame_start() -> FrameStart {
    FrameStart {
        core_epoch: 7,
        generation: 3,
        tab: id('a'),
        sequence: 9,
        width: 1280,
        height: 720,
        stride: 1280 * 4,
        total_bytes: 1280 * 720 * 4,
    }
}

fn blob(source: BlobSource) -> BlobStart {
    BlobStart {
        core_epoch: 7,
        transfer_id: 2,
        source,
        media_type: MediaType::Png,
        total_bytes: 100,
        digest: if source == BlobSource::Artifact {
            [9; 32]
        } else {
            [0; 32]
        },
    }
}

#[test]
fn hello_is_recognised_and_json_is_not() {
    let hello = encode_hello();
    assert_eq!(&hello[..4], &(PREFIX_LEN as u32).to_be_bytes());
    assert!(is_hello(&hello[4..]));
    assert!(!is_hello(br#"{"type":"client_hello"}"#));
    assert!(!is_hello(&[]));
    let mut wrong_kind = hello[4..].to_vec();
    wrong_kind[6] = Kind::End as u8;
    assert!(!is_hello(&wrong_kind));
}

#[test]
fn every_record_round_trips_with_golden_bytes() {
    let channel = ChannelOffer {
        channel: id('c'),
        bind_window_ms: BIND_WINDOW_MS,
    };
    let record = channel.encode();
    let (prefix, meta, _) = split(&record);
    assert_eq!(prefix.kind, Kind::Channel);
    assert_eq!(ChannelOffer::decode(meta).unwrap(), channel);
    assert_eq!(
        &record[..16],
        b"EVRD\x01\x00\x02\x00\x14\x00\x00\x00\x00\x00\x00\x00"
    );
    assert_eq!(&record[16..32], &[0xcc; 16]);

    let start = frame_start();
    let record = start.encode();
    let (prefix, meta, _) = split(&record);
    assert_eq!(prefix.kind, Kind::FrameStart);
    assert_eq!(FrameStart::decode(meta).unwrap(), start);
    start.validate().unwrap();

    for source in [BlobSource::Artifact, BlobSource::Capture] {
        let start = blob(source);
        let record = start.encode();
        let (prefix, meta, _) = split(&record);
        assert_eq!(prefix.kind, Kind::BlobStart);
        assert_eq!(BlobStart::decode(meta).unwrap(), start);
    }

    let chunk = ChunkMeta {
        id: 9,
        offset: 65536,
    };
    let record = chunk.encode(&[1, 2, 3]);
    let (prefix, meta, body) = split(&record);
    assert_eq!((prefix.kind, body), (Kind::Chunk, &[1, 2, 3][..]));
    assert_eq!(ChunkMeta::decode(meta).unwrap(), chunk);

    let end = EndMeta {
        id: 9,
        total_bytes: 5,
    };
    let record = end.encode();
    assert_eq!(EndMeta::decode(split(&record).1).unwrap(), end);

    let ack = AckMeta { id: 9 };
    let record = ack.encode();
    assert_eq!(AckMeta::decode(split(&record).1).unwrap(), ack);

    for code in [
        ErrorCode::Denied,
        ErrorCode::Expired,
        ErrorCode::Busy,
        ErrorCode::Closed,
        ErrorCode::Invalid,
        ErrorCode::TimedOut,
    ] {
        let record = ErrorMeta { code }.encode();
        assert_eq!(ErrorMeta::decode(split(&record).1).unwrap().code, code);
    }
}

#[test]
fn prefix_is_checked_before_anything_is_allocated() {
    let good = prefix_bytes(Kind::Chunk, ChunkMeta::LEN, 10);
    assert!(Prefix::parse(&good).is_ok());

    let mut bad = good;
    bad[0] = b'X';
    assert_eq!(Prefix::parse(&bad), Err(DataError::BadMagic));

    let mut bad = good;
    bad[4] = 2;
    assert_eq!(Prefix::parse(&bad), Err(DataError::UnsupportedVersion(2)));

    let mut bad = good;
    bad[6] = 99;
    assert_eq!(Prefix::parse(&bad), Err(DataError::UnknownKind(99)));

    let mut bad = good;
    bad[8..12].copy_from_slice(&(MAX_META_BYTES as u32 + 1).to_le_bytes());
    assert!(matches!(
        Prefix::parse(&bad),
        Err(DataError::TooLarge { field: "meta", .. })
    ));

    let mut bad = good;
    bad[12..16].copy_from_slice(&u32::MAX.to_le_bytes());
    assert!(matches!(
        Prefix::parse(&bad),
        Err(DataError::TooLarge { field: "body", .. })
    ));

    let largest = prefix_bytes(Kind::Chunk, ChunkMeta::LEN, CHUNK_BYTES);
    assert!(Prefix::parse(&largest).is_ok());

    let bad = prefix_bytes(Kind::End, EndMeta::LEN - 1, 0);
    assert_eq!(
        Prefix::parse(&bad),
        Err(DataError::UnexpectedLength("meta"))
    );
    let bad = prefix_bytes(Kind::End, EndMeta::LEN, 1);
    assert_eq!(
        Prefix::parse(&bad),
        Err(DataError::UnexpectedLength("body"))
    );
}

#[test]
fn frame_sizes_are_validated_in_wide_arithmetic() {
    frame_start().validate().unwrap();

    let mut start = frame_start();
    start.stride -= 4;
    assert_eq!(start.validate(), Err(DataError::Malformed("frame stride")));

    let mut start = frame_start();
    start.total_bytes += 1;
    assert_eq!(start.validate(), Err(DataError::Malformed("frame total")));

    let mut start = frame_start();
    start.width = 0;
    assert_eq!(start.validate(), Err(DataError::Malformed("frame size")));

    let mut start = frame_start();
    start.width = u32::MAX;
    start.stride = u32::MAX;
    assert_eq!(start.validate(), Err(DataError::Malformed("frame size")));

    let full = FrameStart {
        width: 3840,
        height: 2160,
        stride: 3840 * 4,
        total_bytes: MAX_FRAME_BYTES,
        ..frame_start()
    };
    full.validate().unwrap();

    let mut meta = frame_start().encode()[PREFIX_LEN..].to_vec();
    meta[52] = 2;
    assert_eq!(
        FrameStart::decode(&meta),
        Err(DataError::Malformed("frame format"))
    );
}

#[test]
fn blob_limits_follow_the_source() {
    let mut start = blob(BlobSource::Artifact);
    start.total_bytes = MAX_IMAGE_BLOB_BYTES;
    start.validate().unwrap();
    start.total_bytes += 1;
    assert!(matches!(start.validate(), Err(DataError::TooLarge { .. })));

    let mut start = blob(BlobSource::Artifact);
    start.digest = [0; 32];
    assert_eq!(start.validate(), Err(DataError::Malformed("blob digest")));

    let mut start = blob(BlobSource::Capture);
    start.total_bytes = MAX_CAPTURE_BLOB_BYTES;
    start.validate().unwrap();
    start.digest = [1; 32];
    assert_eq!(start.validate(), Err(DataError::Malformed("blob digest")));

    let mut start = blob(BlobSource::Capture);
    start.media_type = MediaType::Jpeg;
    assert_eq!(start.validate(), Err(DataError::Malformed("blob digest")));

    let mut start = blob(BlobSource::Artifact);
    start.total_bytes = 0;
    assert_eq!(start.validate(), Err(DataError::Malformed("empty blob")));

    let mut meta = blob(BlobSource::Artifact).encode()[PREFIX_LEN..].to_vec();
    meta[24..32].copy_from_slice(&u64::MAX.to_le_bytes());
    assert!(BlobStart::decode(&meta).is_err());
}

#[test]
fn media_types_map_to_mime() {
    for media in [
        MediaType::Png,
        MediaType::Jpeg,
        MediaType::Gif,
        MediaType::Webp,
    ] {
        assert_eq!(MediaType::from_mime(media.mime()), Some(media));
    }
    assert_eq!(MediaType::from_mime("image/svg+xml"), None);
    assert_eq!(MediaType::from_mime("text/html"), None);
}

#[test]
fn artifact_digest_must_be_sha256_hex() {
    assert_eq!(
        BlobStart::artifact_digest(&"ab".repeat(32)).unwrap(),
        [0xab; 32]
    );
    assert!(BlobStart::artifact_digest("zz").is_err());
    assert!(BlobStart::artifact_digest(&"g".repeat(64)).is_err());
}

#[test]
fn transfer_rejects_duplicate_missing_reordered_and_mixed_chunks() {
    let total = (CHUNK_BYTES * 2 + 10) as u64;
    let chunk = |id, offset| ChunkMeta { id, offset };
    let end = |id, total_bytes| EndMeta { id, total_bytes };

    let mut transfer = Transfer::new(5, total);
    transfer.chunk(chunk(5, 0), CHUNK_BYTES).unwrap();
    transfer
        .chunk(chunk(5, CHUNK_BYTES as u64), CHUNK_BYTES)
        .unwrap();
    assert_eq!(
        transfer.end(end(5, total)),
        Err(DataError::OutOfOrder("end before every byte"))
    );
    transfer
        .chunk(chunk(5, 2 * CHUNK_BYTES as u64), 10)
        .unwrap();
    transfer.end(end(5, total)).unwrap();
    assert_eq!(transfer.received(), total);

    let mut transfer = Transfer::new(5, total);
    transfer.chunk(chunk(5, 0), CHUNK_BYTES).unwrap();
    assert_eq!(
        transfer.chunk(chunk(5, 0), CHUNK_BYTES),
        Err(DataError::OutOfOrder("chunk offset"))
    );
    assert_eq!(
        transfer.chunk(chunk(5, CHUNK_BYTES as u64 + 1), 1),
        Err(DataError::OutOfOrder("chunk offset"))
    );
    assert_eq!(
        transfer.chunk(chunk(6, CHUNK_BYTES as u64), 1),
        Err(DataError::OutOfOrder("chunk of another transfer"))
    );

    let mut transfer = Transfer::new(1, 10);
    assert!(matches!(
        transfer.chunk(chunk(1, 0), 11),
        Err(DataError::TooLarge { .. })
    ));
    assert_eq!(
        transfer.chunk(chunk(1, 0), 0),
        Err(DataError::UnexpectedLength("chunk body"))
    );
    transfer.chunk(chunk(1, 0), 10).unwrap();
    assert!(transfer.chunk(chunk(1, 10), 1).is_err());
    assert_eq!(
        transfer.end(end(1, 11)),
        Err(DataError::OutOfOrder("end of another transfer"))
    );
}

#[test]
fn sequences_only_move_forward() {
    let mut guard = SequenceGuard::default();
    guard.accept(4).unwrap();
    guard.accept(9).unwrap();
    assert!(guard.accept(9).is_err());
    assert!(guard.accept(3).is_err());
    guard.accept(10).unwrap();
}

#[test]
fn limits_match_the_plan() {
    assert_eq!(MAX_FRAME_SUBSCRIPTIONS, 8);
    assert_eq!(MAX_FRAME_TABS, 4);
    assert_eq!(MAX_BLOB_TRANSFERS, 4);
    assert_eq!(BIND_WINDOW_MS, 30_000);
    assert_eq!(NO_PROGRESS_MS, 5_000);
    assert_eq!(CHUNK_BYTES, 64 * 1024);
    assert_eq!(MAX_FRAME_BYTES, 33_177_600);
}
