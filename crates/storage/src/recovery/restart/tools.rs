//! Per-incarnation ordered tool evidence. Comparison is pure and fail-closed.
use super::*;
use serde_json::Value;
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolSnapshot {
    /// Exact effective wire schemas, in wire order.
    pub effective: Vec<Value>,
    /// Compiled built-ins only. MCP/config-injected tools are never in this catalog.
    pub builtins: Vec<Value>,
    /// All non-tool provider input and execution authority, including permissions.
    pub stable_context: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RestartToolsEvidence {
    pub run_id: String,
    pub incarnation: String,
    pub old: ToolSnapshot,
    pub new: ToolSnapshot,
    pub old_build: String,
    pub new_build: String,
    pub old_catalog_fingerprint: String,
    pub new_catalog_fingerprint: String,
    pub old_effective_fingerprint: String,
    pub new_effective_fingerprint: String,
    pub boundary: bool,
}
fn names(tools: &[Value]) -> Result<Vec<&str>, StorageError> {
    if tools.len() > MAX_BUILTIN_TOOL_NAMES {
        return Err(invalid("too many tools"));
    }
    let names = tools
        .iter()
        .map(|v| {
            v.get("name")
                .and_then(Value::as_str)
                .filter(|n| !n.is_empty() && n.len() <= MAX_BUILTIN_TOOL_NAME_BYTES)
                .ok_or_else(|| invalid("invalid tool identity"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    if names.iter().collect::<BTreeSet<_>>().len() != names.len() {
        return Err(invalid("duplicate tool name"));
    }
    Ok(names)
}
impl RestartToolsEvidence {
    pub fn compare(
        run_id: String,
        incarnation: String,
        old_build: String,
        new_build: String,
        old: ToolSnapshot,
        new: ToolSnapshot,
    ) -> Result<Self, StorageError> {
        let old_names = names(&old.effective)?;
        let new_names = names(&new.effective)?;
        let old_builtin = names(&old.builtins)?;
        let new_builtin = names(&new.builtins)?;
        if old.stable_context != new.stable_context {
            return Err(invalid("restart changed non-tool context"));
        }
        let boundary = old.effective != new.effective;
        if boundary {
            if old_build == new_build || old.builtins == new.builtins {
                return Err(invalid("unexplained tool change"));
            }
            // External tools must retain exact schema and relative order. A compiled-name
            // collision may not silently turn an old external tool into a built-in.
            let external = |s: &ToolSnapshot, names: &Vec<&str>| {
                s.effective
                    .iter()
                    .filter(|v| !names.contains(&v["name"].as_str().unwrap()))
                    .cloned()
                    .collect::<Vec<_>>()
            };
            if external(&old, &old_builtin) != external(&new, &new_builtin) {
                return Err(invalid("external tool change"));
            }
            for (s, catalog) in [(&old, &old_builtin), (&new, &new_builtin)] {
                for tool in &s.effective {
                    if let Some(i) = catalog
                        .iter()
                        .position(|n| Some(*n) == tool["name"].as_str())
                        && s.builtins[i] != *tool
                    {
                        return Err(invalid("effective built-in not from compiled catalog"));
                    }
                }
                // Only compiled catalog ordering may explain a built-in reorder.
                let positions = s
                    .effective
                    .iter()
                    .filter_map(|tool| {
                        catalog
                            .iter()
                            .position(|n| Some(*n) == tool["name"].as_str())
                    })
                    .collect::<Vec<_>>();
                if positions.windows(2).any(|p| p[0] >= p[1]) {
                    return Err(invalid("built-in order differs from catalog"));
                }
            }
            for name in old_builtin.iter().filter(|n| new_builtin.contains(n)) {
                if old_names.contains(name) != new_names.contains(name) {
                    return Err(invalid("configuration changed built-in availability"));
                }
            }
        }
        Ok(Self {
            run_id,
            incarnation,
            old_build,
            new_build,
            old_catalog_fingerprint: restart_digest(&old.builtins)?,
            new_catalog_fingerprint: restart_digest(&new.builtins)?,
            old_effective_fingerprint: restart_digest(&old.effective)?,
            new_effective_fingerprint: restart_digest(&new.effective)?,
            old,
            new,
            boundary,
        })
    }
}
pub(super) fn capture(
    conn: &Connection,
    tx: &RestartTransaction,
    run: &str,
    incarnation: &str,
    snapshot: &ToolSnapshot,
) -> Result<(), StorageError> {
    phase(tx.phase, RestartPhase::Capturing)?;
    names(&snapshot.effective)?;
    names(&snapshot.builtins)?;
    if !tx
        .expected
        .iter()
        .any(|m| m.run_id == run && m.incarnation == incarnation)
        || names(&snapshot.builtins)?
            .into_iter()
            .map(str::to_string)
            .collect::<BTreeSet<_>>()
            != tx.metadata.old_builtin_tool_names
    {
        return Err(invalid("tool snapshot capture mismatch"));
    }
    let json = encode(snapshot)?;
    exact_json(&json)?;
    let prior:Option<String>=conn.query_row("SELECT old_snapshot_json FROM restart_run_tools WHERE restart_id=?1 AND run_id=?2 AND incarnation=?3",params![tx.metadata.restart_id,run,incarnation],|r|r.get(0)).optional()?;
    if let Some(prior) = prior {
        if prior == json {
            return Ok(());
        }
        return Err(invalid("tool snapshot already fixed"));
    }
    conn.execute("INSERT INTO restart_run_tools(restart_id,run_id,incarnation,old_snapshot_json) VALUES(?1,?2,?3,?4)",params![tx.metadata.restart_id,run,incarnation,json])?;
    Ok(())
}
impl Database {
    pub fn restart_tool_snapshot(
        &self,
        id: &str,
        run: &str,
        incarnation: &str,
    ) -> Result<Option<ToolSnapshot>, StorageError> {
        let raw:Option<String>=self.conn.query_row("SELECT old_snapshot_json FROM restart_run_tools WHERE restart_id=?1 AND run_id=?2 AND incarnation=?3",params![id,run,incarnation],|r|r.get(0)).optional()?;
        raw.map(|s| decode(&s)).transpose()
    }
    pub fn restart_tools_evidence(
        &self,
        id: &str,
        run: &str,
        incarnation: &str,
    ) -> Result<Option<(RestartToolsEvidence, bool)>, StorageError> {
        let raw:Option<(String,bool)>=self.conn.query_row("SELECT evidence_json,consumed FROM restart_run_tools WHERE evidence_json IS NOT NULL AND restart_id=?1 AND run_id=?2 AND incarnation=?3",params![id,run,incarnation],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
        raw.map(|(s, consumed)| Ok((decode(&s)?, consumed)))
            .transpose()
    }
}
pub(super) fn save(
    conn: &Connection,
    tx: &RestartTransaction,
    e: &RestartToolsEvidence,
) -> Result<(), StorageError> {
    phase(tx.phase, RestartPhase::Adopted)?;
    if e.old_build != tx.metadata.old_build
        || e.new_build != tx.metadata.successor_build
        || !tx
            .expected
            .iter()
            .any(|m| m.run_id == e.run_id && m.incarnation == e.incarnation)
        || tx
            .components
            .iter()
            .any(|c| c.disposition != RestartDisposition::RetryPending)
        || RestartToolsEvidence::compare(
            e.run_id.clone(),
            e.incarnation.clone(),
            e.old_build.clone(),
            e.new_build.clone(),
            e.old.clone(),
            e.new.clone(),
        )? != *e
    {
        return Err(invalid("tool evidence does not match adopted capture"));
    }
    if names(&e.old.builtins)?
        .into_iter()
        .map(str::to_string)
        .collect::<BTreeSet<_>>()
        != tx.metadata.old_builtin_tool_names
    {
        return Err(invalid("old compiled tool names mismatch"));
    }
    let json = encode(e)?;
    exact_json(&json)?;
    let (old,prior):(String,Option<String>)=conn.query_row("SELECT old_snapshot_json,evidence_json FROM restart_run_tools WHERE restart_id=?1 AND run_id=?2 AND incarnation=?3",params![tx.metadata.restart_id,e.run_id,e.incarnation],|r|Ok((r.get(0)?,r.get(1)?)))?;
    if decode::<ToolSnapshot>(&old)? != e.old {
        return Err(invalid("predecessor tool snapshot mismatch"));
    }
    if let Some(prior) = prior {
        if prior == json {
            return Ok(());
        }
        return Err(invalid("tool evidence already fixed"));
    }
    conn.execute("UPDATE restart_run_tools SET evidence_json=?4 WHERE restart_id=?1 AND run_id=?2 AND incarnation=?3",params![tx.metadata.restart_id,e.run_id,e.incarnation,json])?;
    Ok(())
}
pub(super) fn consume(
    conn: &Connection,
    tx: &RestartTransaction,
    run: &str,
    incarnation: &str,
) -> Result<(), StorageError> {
    phase(tx.phase, RestartPhase::Adopted)?;
    if tx.ready_at_ms.is_none() {
        return Err(invalid("tool boundary before Ready"));
    }
    if conn.execute("UPDATE restart_run_tools SET consumed=1 WHERE restart_id=?1 AND run_id=?2 AND incarnation=?3 AND consumed=0 AND json_extract(evidence_json,'$.boundary')=1",params![tx.metadata.restart_id,run,incarnation])?!=1 {return Err(invalid("tool boundary unavailable or already consumed"));}
    Ok(())
}
