//! Reading a thread's input receipts for its queue of follow-ups (ADR 0045 E7).
//!
//! The receipts are not part of the snapshot: each projection reads them again for the
//! thread being shown, with the thread's history. A page that cannot be read is not an empty
//! list and not a finished one: the failure is recorded, shown with a retry, and what was
//! read so far is kept.
use std::sync::{Arc, Mutex};

use workbench_client::WorkbenchClient;
use workbench_protocol::namespaces::{ConversationQuery, ConversationReply};
use workbench_protocol::{
    ids::ThreadId,
    queries::{PageRequest, Query, QueryReply, ThreadQuery},
};

use super::{Shared, failure_reason, update};

/// More pages than a thread can hold at the page size the core allows: a cursor chain that
/// does not end is a failure, not a long list.
const MAX_PAGES: usize = 1024;

pub(super) async fn read_receipts(
    client: &WorkbenchClient,
    shared: &Arc<Mutex<Shared>>,
    ctx: &egui::Context,
    thread: ThreadId,
) {
    let mut cursor = None;
    for _ in 0..MAX_PAGES {
        let reply = client
            .query(Query::Conversation(ConversationQuery::GetInputReceipts(
                ThreadQuery {
                    thread: thread.clone(),
                    page: PageRequest {
                        cursor: cursor.take(),
                        ..Default::default()
                    },
                },
            )))
            .await;
        let Ok(QueryReply::Conversation(ConversationReply::InputReceipts(page))) = reply else {
            let reason = failure_reason(&reply);
            update(shared, ctx, |s| {
                s.cache.receipts_read(thread.as_str(), Err(reason.clone()));
                s.status = format!("Could not read the queued inputs: {reason}");
            });
            return;
        };
        cursor = page.next_cursor;
        update(shared, ctx, |s| {
            for receipt in page.items {
                s.cache.merge_receipt(receipt);
            }
            if cursor.is_none() {
                s.cache.receipts_read(thread.as_str(), Ok(()));
            }
        });
        if cursor.is_none() {
            return;
        }
    }
    update(shared, ctx, |s| {
        s.cache.receipts_read(
            thread.as_str(),
            Err("the list of queued inputs does not end".into()),
        );
    });
}
