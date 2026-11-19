//! Agent-only stream. Renderer FrameStream's fixed-tab validation is unchanged.
use super::*;
use workbench_protocol::namespaces::browser::observation::{data::AgentFrameStart, *};
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentFrame {
    pub start: AgentFrameStart,
    pub rgba: Vec<u8>,
}
pub struct AgentFrameStream {
    link: Link,
    scope: ObservationScope,
    order: SequenceGuard,
}
impl DataChannel {
    pub async fn bind_agent_frames(
        self,
        control: &WorkbenchClient,
        scope: ObservationScope,
        subscription: ObservationId,
        token: ObservationId,
    ) -> Result<AgentFrameStream, DataClientError> {
        if scope.core_epoch != control.server_hello().instance_epoch.0 {
            return Err(DataError::OutOfOrder("agent epoch mismatch").into());
        }
        let command = Command::Browser(BrowserCommand::BindAgentData {
            channel_id: self.channel.clone(),
            subscription,
            token,
        });
        let stream = self.bind(control, command).await?;
        Ok(AgentFrameStream {
            link: Link::new(stream),
            scope,
            order: SequenceGuard::default(),
        })
    }
}
impl AgentFrameStream {
    pub async fn next_frame(&mut self) -> Result<Option<AgentFrame>, DataClientError> {
        let mut stream = self.link.take()?;
        let result = read_agent(&mut stream, &self.scope, &mut self.order).await;
        self.link.give_back(stream, result)
    }
}
async fn read_agent(
    stream: &mut UnixStream,
    scope: &ObservationScope,
    order: &mut SequenceGuard,
) -> Result<Option<AgentFrame>, DataClientError> {
    let Some(head) = read_head(stream).await? else {
        return Ok(None);
    };
    if head.kind != Kind::AgentFrameStart {
        return Err(DataError::OutOfOrder("expected agent frame").into());
    }
    let start = AgentFrameStart::decode(&head.meta)?;
    if start.scope != *scope {
        return Err(DataError::OutOfOrder("agent scope mismatch").into());
    }
    order.accept(start.sequence)?;
    let mut rgba = Vec::with_capacity(start.total_bytes as usize);
    let mut transfer = Transfer::new(start.sequence, start.total_bytes);
    loop {
        let head = read_head(stream).await?.ok_or(DataClientError::Truncated)?;
        match head.kind {
            Kind::Chunk => {
                transfer.chunk(ChunkMeta::decode(&head.meta)?, head.body_len)?;
                read_body(stream, &mut rgba, head.body_len).await?;
            }
            Kind::End => {
                transfer.end(EndMeta::decode(&head.meta)?)?;
                break;
            }
            _ => return Err(DataError::OutOfOrder("inside agent frame").into()),
        }
    }
    send_ack(stream, start.sequence).await?;
    Ok(Some(AgentFrame { start, rgba }))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn start(tab: &str, sequence: u64) -> AgentFrameStart {
        AgentFrameStart {
            scope: ObservationScope {
                project: workbench_protocol::ids::ProjectId::new("p").unwrap(),
                thread: workbench_protocol::ids::ThreadId::new("t").unwrap(),
                run: workbench_protocol::ids::RunId::new("r").unwrap(),
                incarnation: workbench_protocol::ids::RunId::new("i").unwrap(),
                core_epoch: 7,
                browser_generation: 1,
                page_generation: 1,
            },
            tab: ObservationId::new(tab.repeat(32)).unwrap(),
            sequence,
            width: 1,
            height: 1,
            stride: 4,
            total_bytes: 4,
        }
    }
    async fn send_frame(stream: &mut UnixStream, start: AgentFrameStart) {
        stream.write_all(&start.encode().unwrap()).await.unwrap();
        stream
            .write_all(
                &ChunkMeta {
                    id: start.sequence,
                    offset: 0,
                }
                .encode_head(4),
            )
            .await
            .unwrap();
        stream.write_all(&[1, 2, 3, 255]).await.unwrap();
        stream
            .write_all(
                &EndMeta {
                    id: start.sequence,
                    total_bytes: 4,
                }
                .encode(),
            )
            .await
            .unwrap();
        let meta = expect_record(stream, Kind::Ack).await.unwrap();
        assert_eq!(AckMeta::decode(&meta).unwrap().id, start.sequence);
    }
    #[tokio::test]
    async fn agent_wire_keeps_scope_and_increasing_sequence_but_follows_tabs() {
        let (mut server, client) = UnixStream::pair().unwrap();
        let mut reader = AgentFrameStream {
            link: Link::new(client),
            scope: start("a", 1).scope,
            order: SequenceGuard::default(),
        };
        let producer = async {
            send_frame(&mut server, start("a", 1)).await;
            send_frame(&mut server, start("b", 2)).await;
        };
        let consumer = async {
            let first = reader.next_frame().await.unwrap().unwrap();
            let second = reader.next_frame().await.unwrap().unwrap();
            assert_ne!(first.start.tab, second.start.tab);
            assert_eq!(second.rgba, [1, 2, 3, 255]);
        };
        tokio::join!(producer, consumer);
    }
    #[tokio::test]
    async fn old_incarnation_and_renderer_records_close_agent_stream() {
        for record in [
            {
                let mut old = start("a", 1);
                old.scope.incarnation = workbench_protocol::ids::RunId::new("old").unwrap();
                old.encode().unwrap()
            },
            FrameStart {
                core_epoch: 7,
                generation: 1,
                tab: RendererId::new("a".repeat(32)).unwrap(),
                sequence: 1,
                width: 1,
                height: 1,
                stride: 4,
                total_bytes: 4,
            }
            .encode(),
        ] {
            let (mut server, client) = UnixStream::pair().unwrap();
            let mut reader = AgentFrameStream {
                link: Link::new(client),
                scope: start("a", 1).scope,
                order: SequenceGuard::default(),
            };
            server.write_all(&record).await.unwrap();
            assert!(reader.next_frame().await.is_err());
            assert!(matches!(
                reader.next_frame().await,
                Err(DataClientError::Closed)
            ));
            assert_eq!(server.read(&mut [0; 1]).await.unwrap(), 0);
        }
    }
}
