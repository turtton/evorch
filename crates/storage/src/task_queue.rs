use crate::entity::{TaskRecord, TaskStatus};
use crate::{Database, StorageError, StorageHandle};
use rusqlite::{Connection, params};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TaskDependencies {
    pub blocks: Vec<String>,
    pub blocked_by: Vec<String>,
}

pub(crate) enum Mutation {
    Enqueue(TaskRecord),
    Link(String, String),
    Start(String),
    Finish(String, bool),
}

impl StorageHandle {
    pub fn enqueue_task(&self, task: &TaskRecord) -> Result<(), StorageError> {
        self.queue_mutation(Mutation::Enqueue(task.clone()))
    }
    pub fn link_tasks(&self, blocker: &str, blocked: &str) -> Result<(), StorageError> {
        self.queue_mutation(Mutation::Link(blocker.into(), blocked.into()))
    }
    pub fn start_task(&self, id: &str) -> Result<(), StorageError> {
        self.queue_mutation(Mutation::Start(id.into()))
    }
    pub fn finish_task(&self, id: &str, success: bool) -> Result<(), StorageError> {
        self.queue_mutation(Mutation::Finish(id.into(), success))
    }
}

impl Database {
    pub fn queued_tasks(&self) -> Result<Vec<TaskRecord>, StorageError> {
        let mut statement = self
            .conn
            .prepare("SELECT id FROM tasks ORDER BY created_at_ns, id LIMIT 100")?;
        let ids = statement
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        ids.iter()
            .map(|id| {
                crate::repo::task::get(&self.conn, id)?.ok_or_else(|| invalid("task disappeared"))
            })
            .collect()
    }

    pub fn task_dependencies(&self, id: &str) -> Result<TaskDependencies, StorageError> {
        let mut blocks = self
            .conn
            .prepare("SELECT blocked_id FROM task_links WHERE blocker_id=?1 ORDER BY blocked_id")?;
        let mut blocked_by = self
            .conn
            .prepare("SELECT blocker_id FROM task_links WHERE blocked_id=?1 ORDER BY blocker_id")?;
        Ok(TaskDependencies {
            blocks: blocks
                .query_map([id], |row| row.get(0))?
                .collect::<Result<_, _>>()?,
            blocked_by: blocked_by
                .query_map([id], |row| row.get(0))?
                .collect::<Result<_, _>>()?,
        })
    }
}

pub(crate) fn apply(conn: &Connection, mutation: &Mutation) -> Result<(), StorageError> {
    let tx = conn.unchecked_transaction()?;
    match mutation {
        Mutation::Enqueue(task) => {
            if task.status != TaskStatus::Pending || task.id.trim().is_empty() {
                return Err(invalid("enqueue requires a pending task with identity"));
            }
            crate::repo::task::create(&tx, task)?;
        }
        Mutation::Link(blocker, blocked) => {
            let waiting: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM tasks WHERE id=?1 AND status IN ('pending','blocked'))", [blocked], |row| row.get(0))?;
            let cycle: bool = tx.query_row("WITH RECURSIVE downstream(id) AS (SELECT ?1 UNION SELECT blocked_id FROM task_links JOIN downstream ON blocker_id=downstream.id) SELECT EXISTS(SELECT 1 FROM downstream WHERE id=?2)", params![blocked,blocker], |row| row.get(0))?;
            if !waiting || cycle {
                return Err(invalid(
                    "dependency requires waiting task and acyclic graph",
                ));
            }
            tx.execute(
                "INSERT INTO task_links(blocker_id,blocked_id) VALUES(?1,?2)",
                params![blocker, blocked],
            )?;
        }
        Mutation::Start(id) => {
            resolve(&tx)?;
            if tx.execute(
                "UPDATE tasks SET status='running' WHERE id=?1 AND status='pending'",
                [id],
            )? != 1
            {
                return Err(invalid("task is not ready"));
            }
        }
        Mutation::Finish(id, success) => {
            let status = if *success { "completed" } else { "failed" };
            if tx.execute(
                "UPDATE tasks SET status=?2 WHERE id=?1 AND status='running'",
                params![id, status],
            )? != 1
            {
                return Err(invalid("task is not running"));
            }
        }
    }
    let (operation, payload) = match mutation {
        Mutation::Enqueue(task) => (
            "enqueue",
            serde_json::json!({"id":task.id,"session_id":task.session_id,"created_at_ns":crate::system_time_to_ns(task.created_at)?,"updated_at_ns":crate::system_time_to_ns(task.updated_at)?}),
        ),
        Mutation::Link(blocker, blocked) => (
            "link",
            serde_json::json!({"blocker":blocker,"blocked":blocked}),
        ),
        Mutation::Start(id) => ("start", serde_json::json!({"id":id})),
        Mutation::Finish(id, success) => ("finish", serde_json::json!({"id":id,"success":success})),
    };
    let payload = payload.to_string();
    crate::entity::SecretGuard::from_env().check_text("task_queue", "payload", &payload)?;
    tx.execute(
        "INSERT INTO task_queue_ledger(operation,payload) VALUES(?1,?2)",
        params![operation, payload],
    )?;
    resolve(&tx)?;
    tx.commit()?;
    Ok(())
}

pub(crate) fn resolve(conn: &Connection) -> Result<(), StorageError> {
    conn.execute("UPDATE tasks SET status=CASE WHEN EXISTS(SELECT 1 FROM task_links JOIN tasks blocker ON blocker.id=task_links.blocker_id WHERE blocked_id=tasks.id AND blocker.status <> 'completed') THEN 'blocked' ELSE 'pending' END WHERE status IN ('pending','blocked')", [])?;
    Ok(())
}

fn invalid(message: &str) -> StorageError {
    StorageError::Serialization(message.into())
}
