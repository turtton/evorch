//! Snapshot-bound header seeks and page-local descriptor restoration.
use std::{cmp::Ordering, collections::BinaryHeap};

use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde::Serialize;
use sha2::{Digest, Sha256};

use super::{Role, index, meter};
use crate::{Database, StorageError};

fn invalid() -> StorageError {
    StorageError::Io("unavailable history projection".into())
}
fn hash<T: Serialize>(digest: &mut Sha256, value: &T) -> Result<(), StorageError> {
    let value =
        serde_json::to_vec(value).map_err(|e| StorageError::Serialization(e.to_string()))?;
    digest.update((value.len() as u64).to_be_bytes());
    digest.update(value);
    Ok(())
}
fn encoded_size(value: &impl Serialize) -> Result<usize, StorageError> {
    struct Count(usize);
    impl std::io::Write for Count {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0 += bytes.len();
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut count = Count(0);
    serde_json::to_writer(&mut count, value)
        .map_err(|e| StorageError::Serialization(e.to_string()))?;
    Ok(count.0)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Forward,
    Backward,
}

/// The legacy complete order. A trailing input uses MAX even if an event has that ID.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Order(pub i64, pub bool, pub i64, pub String);

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct InputBody {
    pub id: String,
    pub bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Descriptor {
    pub run: String,
    pub index: u64,
    pub role: Role,
    pub parts: Vec<(String, bool)>,
    pub input: Option<InputBody>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool: Option<super::ToolRow>,
}

#[derive(Debug)]
pub struct HistoryPage {
    pub items: Vec<Descriptor>,
    /// Exclusive boundary, absent only when no further header exists.
    pub next: Option<Order>,
    pub oversized: bool,
}

pub struct HistorySnapshot<'a> {
    tx: Transaction<'a>,
    pub runs: Vec<String>,
    pub latest: i64,
    membership: Vec<(String, String, String, u32)>,
    thread: String,
    run: Option<String>,
}

#[derive(Debug, Clone)]
struct Input {
    id: String,
    at: i64,
    anchor: Option<i64>,
    index: u64,
    bytes: u64,
    encoded_len: usize,
}
impl Input {
    fn order(&self) -> Order {
        Order(
            self.anchor.unwrap_or(i64::MAX),
            false,
            self.at,
            self.id.clone(),
        )
    }
}
struct Run {
    id: String,
    first_prompt: Option<i64>,
    suppress_prompt: bool,
    inputs: Vec<Input>,
}
struct ReadyState {
    phase: i64,
    source: Option<i64>,
    applied: Option<i64>,
    generation: Option<i64>,
    prompt: Option<i64>,
    merge: Option<i64>,
}
pub struct PreparedHistory<'a> {
    conn: &'a Connection,
    runs: Vec<Run>,
    high_water: i64,
    pub freeze: [u8; 32],
}
pub enum Availability<'a> {
    Indexed(PreparedHistory<'a>),
    Legacy(&'static str),
}

impl<'a> HistorySnapshot<'a> {
    pub(crate) fn open(
        db: &'a Database,
        thread: &str,
        run: Option<&str>,
    ) -> Result<Option<Self>, StorageError> {
        let tx = db.conn.unchecked_transaction()?;
        if crate::repo::threads::get(&tx, thread)?.is_none() {
            return Ok(None);
        }
        let members = crate::repo::threads::runs_of_thread(&tx, thread)?;
        let members: Vec<_> = members
            .into_iter()
            .filter(|r| run.is_none_or(|id| id == r.run_id))
            .collect();
        if run.is_some() && members.is_empty() {
            return Ok(None);
        }
        let runs = members.iter().map(|r| r.run_id.clone()).collect();
        let membership = members
            .into_iter()
            .map(|r| (r.run_id, r.thread_id, r.root_run_id, r.depth))
            .collect();
        let latest = tx.query_row("SELECT COALESCE(MAX(id),0) FROM events", [], |r| r.get(0))?;
        Ok(Some(Self {
            tx,
            runs,
            latest,
            membership,
            thread: thread.into(),
            run: run.map(str::to_owned),
        }))
    }

    /// No event payloads or unbounded input bodies are read by preparation.
    pub fn prepare(
        &self,
        high_water: i64,
        direction: Direction,
    ) -> Result<Availability<'_>, StorageError> {
        if !index::version_ok(&self.tx)? {
            return Ok(Availability::Legacy("version"));
        }
        if crate::repo::event::select::has_unbackfilled(&self.tx)? {
            return Ok(Availability::Legacy("unbackfilled"));
        }
        let (generation, migration_max): (i64, i64) = self.tx.query_row(
            "SELECT generation,migration_max FROM history_control WHERE id=1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        let mut digest = Sha256::new();
        hash(
            &mut digest,
            &(
                &self.thread,
                &self.run,
                high_water,
                matches!(direction, Direction::Backward),
                &self.membership,
                generation,
            ),
        )?;
        let mut runs = Vec::with_capacity(self.runs.len());
        for run in &self.runs {
            let mut statement = self.tx.prepare_cached(
                "SELECT phase,source,applied,generation,first_prompt_id,first_prompt_merge_max
                 FROM history_run_state WHERE run_id=?1",
            )?;
            let state = statement
                .query_row([run], |r| {
                    Ok(ReadyState {
                        phase: r.get(0)?,
                        source: r.get(1)?,
                        applied: r.get(2)?,
                        generation: r.get(3)?,
                        prompt: r.get(4)?,
                        merge: r.get(5)?,
                    })
                })
                .optional()?;
            meter::statement(&self.tx, &statement);
            let (generation, first_prompt, first_merge) = if let Some(ReadyState {
                phase,
                source,
                applied,
                generation,
                prompt,
                merge,
            }) = state
            {
                if phase != 2 || source.is_none() || source != applied || generation.is_none() {
                    return Ok(Availability::Legacy("dirty"));
                }
                (
                    generation.unwrap(),
                    prompt.filter(|id| *id <= high_water),
                    merge,
                )
            } else {
                // A committed insert after migration would have created a state row.
                let first: Option<i64> = self.tx.query_row(
                    "SELECT MIN(id) FROM events WHERE run_id=?1",
                    [run],
                    |r| r.get(0),
                )?;
                if first.is_some_and(|id| id <= migration_max) {
                    return Ok(Availability::Legacy("unbuilt"));
                }
                (0, None, None)
            };
            let mut statement = self.tx.prepare_cached(
                "SELECT event_id,after_count,merge_max FROM history_event_cuts
                 WHERE run_id=?1 AND event_id<=?2 ORDER BY event_id DESC LIMIT 1",
            )?;
            let cut: Option<(i64, u64, i64)> = statement
                .query_row(params![run, high_water], |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?))
                })
                .optional()?;
            meter::statement(&self.tx, &statement);
            meter::record(&self.tx, |c| c.cut_headers += u64::from(cut.is_some()));
            hash(&mut digest, &(run, generation, cut))?;
            let (mut inputs, suppress_prompt) = inputs(
                &self.tx,
                run,
                high_water,
                cut,
                first_prompt,
                first_merge,
                &mut digest,
            )?;
            if suppress_prompt {
                for input in &mut inputs {
                    if input
                        .anchor
                        .is_none_or(|a| first_prompt.is_some_and(|f| f < a))
                    {
                        input.encoded_len = adjust_index(
                            input.encoded_len,
                            input.index,
                            input.index.checked_sub(1).ok_or_else(invalid)?,
                        )?;
                        input.index -= 1;
                    }
                }
            }
            runs.push(Run {
                id: run.clone(),
                first_prompt,
                suppress_prompt,
                inputs,
            });
        }
        Ok(Availability::Indexed(PreparedHistory {
            conn: &self.tx,
            runs,
            high_water,
            freeze: digest.finalize().into(),
        }))
    }
}

const INPUTS: &str = "SELECT prompt_id,CASE WHEN octet_length(text)<=16384 THEN text END,
    octet_length(text),accepted_at_ns,
    prompt_id=(SELECT prompt_id FROM host_input_receipts WHERE run_id=?1
        AND json_extract(provenance_json,'$.kind')='user'
        ORDER BY acceptance_seq,accepted_at_ns,prompt_id LIMIT 1)
    FROM host_input_receipts WHERE run_id=?1 AND status='delivered'
    AND json_extract(provenance_json,'$.kind')='user'
    AND run_id NOT IN (SELECT run_id FROM run_tombstones)
    AND root_run_id NOT IN (SELECT run_id FROM run_tombstones)
    ORDER BY accepted_at_ns,prompt_id";

fn inputs(
    conn: &Connection,
    run: &str,
    high_water: i64,
    cut: Option<(i64, u64, i64)>,
    first_prompt: Option<i64>,
    first_merge: Option<i64>,
    digest: &mut Sha256,
) -> Result<(Vec<Input>, bool), StorageError> {
    let mut statement = conn.prepare_cached(INPUTS)?;
    let mut result = Vec::new();
    let mut suppress = false;
    {
        let mut rows = statement.query([run])?;
        while let Some(row) = rows.next()? {
            let id: String = row.get(0)?;
            let text: Option<String> = row.get(1)?;
            let bytes: u64 = row.get(2)?;
            let at: i64 = row.get(3)?;
            let first: bool = row.get(4)?;
            meter::record(conn, |c| {
                c.receipt_headers += 1;
                c.fingerprint_bytes += text.as_ref().map_or(0, |s| s.len() as u64);
            });
            hash(digest, &(&id, run, bytes, at, first, &text))?;
            suppress |= first && first_prompt.is_some() && first_merge.is_some_and(|m| at <= m);
            let anchor = if cut.is_some_and(|(_, _, merge)| at <= merge) {
                // merge_max is monotonic in event ID. Ordering by the merge index
                // avoids the planner choosing a linear event-ID walk to skip early rows.
                let mut seek = conn.prepare_cached(
                    "SELECT event_id,before_count FROM history_event_cuts
                     WHERE run_id=?1 AND merge_max>=?2 AND event_id<=?3
                     ORDER BY merge_max,event_id LIMIT 1",
                )?;
                let anchor = seek.query_row(params![run, at, high_water], |r| {
                    Ok((r.get::<_, i64>(0)?, r.get::<_, u64>(1)?))
                })?;
                meter::statement(conn, &seek);
                meter::record(conn, |c| c.cut_headers += 1);
                Some(anchor)
            } else {
                None
            };
            let index = anchor
                .map_or(cut.map_or(0, |c| c.1), |a| a.1)
                .checked_add(result.len() as u64)
                .ok_or_else(invalid)?;
            let descriptor = Descriptor {
                run: run.into(),
                index,
                role: Role::User,
                parts: text
                    .as_ref()
                    .map_or_else(Vec::new, |s| vec![(s.clone(), false)]),
                input: text.is_none().then(|| InputBody {
                    id: id.clone(),
                    bytes,
                }),
                tool: None,
            };
            result.push(Input {
                id,
                at,
                anchor: anchor.map(|a| a.0),
                index,
                bytes,
                encoded_len: encoded_size(&descriptor)?,
            });
        }
    }
    meter::statement(conn, &statement);
    Ok((result, suppress))
}

fn digits(index: u64) -> usize {
    index.checked_ilog10().map_or(1, |n| n as usize + 1)
}
fn adjust_index(bytes: usize, old: u64, new: u64) -> Result<usize, StorageError> {
    bytes
        .checked_sub(digits(old))
        .and_then(|n| n.checked_add(digits(new)))
        .ok_or_else(invalid)
}

#[derive(Clone)]
struct EventHeader {
    id: i64,
    index: u64,
    role: Role,
    encoded_len: usize,
}
enum Choice {
    Event(EventHeader),
    Input(usize),
}
struct Candidate {
    order: Order,
    run: usize,
    choice: Choice,
    backwards: bool,
}
impl PartialEq for Candidate {
    fn eq(&self, other: &Self) -> bool {
        self.order == other.order
    }
}
impl Eq for Candidate {}
impl PartialOrd for Candidate {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for Candidate {
    fn cmp(&self, other: &Self) -> Ordering {
        if self.backwards {
            self.order.cmp(&other.order)
        } else {
            other.order.cmp(&self.order)
        }
    }
}

impl PreparedHistory<'_> {
    fn event(
        &self,
        run: usize,
        boundary: Option<&Order>,
        direction: Direction,
    ) -> Result<Option<Candidate>, StorageError> {
        let meta = &self.runs[run];
        let backwards = direction == Direction::Backward;
        let mut bound = boundary.cloned();
        loop {
            let sql = match (&bound, backwards) {
                (None, false) => {
                    "SELECT output_event_id,ordinal,role,encoded_len FROM history_messages WHERE run_id=?1 AND output_event_id<=?2 AND ?3 IS NULL ORDER BY output_event_id LIMIT 1"
                }
                (None, true) => {
                    "SELECT output_event_id,ordinal,role,encoded_len FROM history_messages WHERE run_id=?1 AND output_event_id<=?2 AND ?3 IS NULL ORDER BY output_event_id DESC LIMIT 1"
                }
                (Some(Order(_, false, ..)), false) => {
                    "SELECT output_event_id,ordinal,role,encoded_len FROM history_messages WHERE run_id=?1 AND output_event_id<=?2 AND output_event_id>=?3 ORDER BY output_event_id LIMIT 1"
                }
                (Some(_), false) => {
                    "SELECT output_event_id,ordinal,role,encoded_len FROM history_messages WHERE run_id=?1 AND output_event_id<=?2 AND output_event_id>?3 ORDER BY output_event_id LIMIT 1"
                }
                (Some(_), true) => {
                    "SELECT output_event_id,ordinal,role,encoded_len FROM history_messages WHERE run_id=?1 AND output_event_id<=?2 AND output_event_id<?3 ORDER BY output_event_id DESC LIMIT 1"
                }
            };
            let mut statement = self.conn.prepare_cached(sql)?;
            let row: Option<(i64, u64, String, usize)> = statement
                .query_row(
                    params![meta.id, self.high_water, bound.as_ref().map(|b| b.0)],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
                )
                .optional()?;
            meter::statement(self.conn, &statement);
            meter::record(self.conn, |c| c.message_headers += u64::from(row.is_some()));
            let Some((id, ordinal, role, size)) = row else {
                return Ok(None);
            };
            let order = Order(id, true, 0, String::new());
            if meta.suppress_prompt && meta.first_prompt == Some(id) {
                bound = Some(order);
                continue;
            }
            let count = meta
                .inputs
                .partition_point(|input| input.anchor.is_some_and(|a| a <= id))
                as u64;
            let index = ordinal
                .checked_add(count)
                .and_then(|i| {
                    i.checked_sub(u64::from(
                        meta.suppress_prompt && meta.first_prompt.is_some_and(|f| f < id),
                    ))
                })
                .ok_or_else(invalid)?;
            let role = Role::parse(&role).ok_or_else(invalid)?;
            return Ok(Some(Candidate {
                order,
                run,
                backwards,
                choice: Choice::Event(EventHeader {
                    id,
                    index,
                    role,
                    encoded_len: adjust_index(size, ordinal, index)?,
                }),
            }));
        }
    }

    /// Header heap is O(R+I); only selected sources are decoded. `byte_limit` uses
    /// the legacy RawMessage JSON measure, including the 256-byte reply allowance.
    pub fn page(
        &self,
        direction: Direction,
        boundary: Option<&Order>,
        limit: usize,
        byte_limit: usize,
    ) -> Result<HistoryPage, StorageError> {
        let mut heap = BinaryHeap::new();
        for (run, meta) in self.runs.iter().enumerate() {
            if let Some(event) = self.event(run, boundary, direction)? {
                heap.push(event);
            }
            for (input, value) in meta.inputs.iter().enumerate() {
                let order = value.order();
                if boundary.is_none_or(|b| {
                    if direction == Direction::Forward {
                        order > *b
                    } else {
                        order < *b
                    }
                }) {
                    heap.push(Candidate {
                        order,
                        run,
                        choice: Choice::Input(input),
                        backwards: direction == Direction::Backward,
                    });
                }
            }
        }
        let mut selected = Vec::with_capacity(limit.min(256));
        let mut bytes = 256usize;
        let mut next = None;
        while let Some(candidate) = heap.pop() {
            meter::record(self.conn, |c| c.merge_candidates += 1);
            let size = match &candidate.choice {
                Choice::Event(header) => header.encoded_len,
                Choice::Input(i) => self.runs[candidate.run].inputs[*i].encoded_len,
            };
            let used = bytes.saturating_add(size).saturating_add(1);
            if selected.len() == limit || used > byte_limit {
                if selected.is_empty() {
                    return Ok(HistoryPage {
                        items: vec![],
                        next: None,
                        oversized: true,
                    });
                }
                next = selected.last().map(|c: &Candidate| c.order.clone());
                break;
            }
            bytes = used;
            if matches!(candidate.choice, Choice::Event(_))
                && let Some(event) = self.event(candidate.run, Some(&candidate.order), direction)?
            {
                heap.push(event);
            }
            selected.push(candidate);
        }
        if direction == Direction::Backward {
            selected.reverse();
        }
        let items = selected
            .into_iter()
            .map(|item| self.descriptor(item))
            .collect::<Result<_, _>>()?;
        Ok(HistoryPage {
            items,
            next,
            oversized: false,
        })
    }

    fn descriptor(&self, candidate: Candidate) -> Result<Descriptor, StorageError> {
        let run = &self.runs[candidate.run];
        match candidate.choice {
            Choice::Input(i) => {
                let input = &run.inputs[i];
                let text: Option<String> = self.conn.query_row(
                    "SELECT CASE WHEN octet_length(text)<=16384 THEN text END FROM host_input_receipts WHERE prompt_id=?1",
                    [&input.id], |r| r.get(0))?;
                Ok(Descriptor {
                    run: run.id.clone(),
                    index: input.index,
                    role: Role::User,
                    parts: text
                        .as_ref()
                        .map_or_else(Vec::new, |s| vec![(s.clone(), false)]),
                    input: text.is_none().then(|| InputBody {
                        id: input.id.clone(),
                        bytes: input.bytes,
                    }),
                    tool: None,
                })
            }
            Choice::Event(header) => {
                let mut statement = self.conn.prepare_cached(
                    "SELECT event_id FROM history_message_sources WHERE run_id=?1 AND output_event_id=?2 ORDER BY part_index")?;
                let sources = statement
                    .query_map(params![run.id, header.id], |r| r.get::<_, i64>(0))?
                    .take(4097)
                    .collect::<Result<Vec<_>, _>>()?;
                meter::statement(self.conn, &statement);
                meter::record(self.conn, |c| c.source_references += sources.len() as u64);
                if sources.is_empty() || sources.len() > 4096 {
                    return Err(invalid());
                }
                let (first, tokenized) = index::source_kind(self.conn, sources[0])?;
                let message =
                    if let event_bus::EventKind::Message(event_bus::MessageEvent::MessagePart {
                        ..
                    }) = &first
                    {
                        let mut parts = Vec::with_capacity(sources.len());
                        let mut bytes = 0usize;
                        for (position, source) in sources.iter().enumerate() {
                            let (kind, tokenized) = if position == 0 {
                                (first.clone(), tokenized)
                            } else {
                                index::source_kind(self.conn, *source)?
                            };
                            let event_bus::EventKind::Message(
                                event_bus::MessageEvent::MessagePart { text, .. },
                            ) = &kind
                            else {
                                return Err(invalid());
                            };
                            bytes = bytes.saturating_add(text.len());
                            if bytes > index::MAX_DESCRIPTOR_BYTES {
                                return Err(invalid());
                            }
                            parts.push((text.clone(), tokenized));
                        }
                        super::Reducer::default().message(header.role, parts, false, false)
                    } else {
                        // A row of one event, whose tool row also names the start that carried its input.
                        let start = match sources.get(1) {
                            Some(source) => {
                                let (kind, format) = index::source_kind(self.conn, *source)?;
                                super::rows::tool_start(&kind, format)
                                    .ok_or_else(invalid)?
                                    .2
                            }
                            None => None,
                        };
                        let row = super::rows::committed(&first, tokenized, start)
                            .filter(|row| row.role == header.role)
                            .ok_or_else(invalid)?;
                        super::Reducer::default().assemble(row)
                    }
                    .ok_or_else(invalid)?;
                Ok(Descriptor {
                    run: run.id.clone(),
                    index: header.index,
                    role: header.role,
                    parts: message.parts,
                    input: None,
                    tool: message.tool,
                })
            }
        }
    }
}
