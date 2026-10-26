use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use rusqlite::{Connection, OpenFlags};
use serde_json::{Value, json};

use super::{
    Result,
    corpus::{self, Case, Corpus},
    require,
};

pub fn seed_identity(seed: &Path) -> Result<String> {
    let conn = Connection::open_with_flags(seed, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    // Stream source/authority metadata: no multi-GiB materialization or index state.
    let mut digest = sha2::Sha256::default();
    use sha2::Digest;
    for query in [
        "SELECT * FROM run_contexts ORDER BY run_id",
        "SELECT * FROM run_context_provenance ORDER BY run_id",
        "SELECT * FROM run_ledger ORDER BY seq",
        "SELECT * FROM threads ORDER BY thread_id",
        "SELECT * FROM thread_runs ORDER BY run_id",
        "SELECT * FROM review_evidence ORDER BY rowid",
        "SELECT run_id,parent_run_id FROM usage_requests ORDER BY rowid",
    ] {
        digest.update(query.as_bytes());
        let mut statement = conn.prepare(query)?;
        let columns = statement.column_count();
        let mut rows = statement.query([])?;
        while let Some(row) = rows.next()? {
            for column in 0..columns {
                use rusqlite::types::ValueRef;
                match row.get_ref(column)? {
                    ValueRef::Null => digest.update([0]),
                    ValueRef::Integer(value) => {
                        digest.update([1]);
                        digest.update(value.to_be_bytes());
                    }
                    ValueRef::Real(value) => {
                        digest.update([2]);
                        digest.update(value.to_be_bytes());
                    }
                    ValueRef::Text(value) | ValueRef::Blob(value) => {
                        digest.update([3]);
                        digest.update((value.len() as u64).to_be_bytes());
                        digest.update(value);
                    }
                }
            }
        }
    }
    Ok(format!("{:x}", digest.finalize()))
}

pub fn cell_id(case: &Case, cold: bool, backfill: bool) -> String {
    format!("{}-{cold}-{backfill}", case.class)
}

pub struct Manifest {
    path: PathBuf,
    pub conditions: String,
    pub expected: BTreeMap<String, Value>,
    value: Value,
}

impl Manifest {
    pub fn open(
        work: &Path,
        seed: &Path,
        project: &Path,
        corpus: &Corpus,
        cases: &[Case],
    ) -> Result<Self> {
        let mut expected = BTreeMap::new();
        for backfill in [false, true] {
            for cold in [false, true] {
                for case in cases {
                    let hits = corpus.expected(case)?;
                    expected.insert(cell_id(case, cold, backfill), json!({
                        "query_sha256": case.hash(), "canonical_sha256": corpus::set_hash(&hits),
                        "canonical_threads": hits.len(), "canonical_hits": hits.values().sum::<u64>(),
                    }));
                }
            }
        }
        use sha2::Digest;
        let mut file = std::fs::File::open(std::env::current_exe()?)?;
        let mut digest = sha2::Sha256::default();
        std::io::copy(&mut file, &mut digest)?;
        let binary_hash = format!("{:x}", digest.finalize());
        let protocol = json!({
            "version": 2, "seed_source_sha256": seed_identity(seed)?,
            "harness_binary_sha256": binary_hash,
            "scope_sha256": corpus::hash(project.to_string_lossy().as_bytes()),
            "repetitions": super::REPETITIONS, "warmups": super::WARMUPS,
            "page_limit": super::PAGE_LIMIT, "latency_policy": "advisory",
            "latency_reference_us_strict": super::LATENCY_REFERENCE_US,
            "extractor_version": storage::thread_index::EXTRACTOR_VERSION,
            "accounting_version": storage::thread_index::ACCOUNTING_VERSION,
            "timing": "zero_wait_tool_use_to_complete_entry_v1",
            "classification": corpus.classification, "cells": expected,
        });
        let conditions = corpus::hash(&serde_json::to_vec(&protocol)?);
        let path = work.join("manifest.json");
        let value = if path.exists() {
            let value: Value = serde_json::from_slice(&std::fs::read(&path)?)?;
            require(
                value["conditions_sha256"] == conditions && value["protocol"] == protocol,
                "resume_conditions_mismatch",
            )?;
            value
        } else {
            json!({"conditions_sha256": conditions, "protocol": protocol, "results": {}})
        };
        let result = Self {
            path,
            conditions,
            expected,
            value,
        };
        result.save()?;
        Ok(result)
    }

    fn save(&self) -> Result<()> {
        let temporary = self.path.with_extension("json.new");
        std::fs::write(&temporary, serde_json::to_vec_pretty(&self.value)?)?;
        std::fs::rename(temporary, &self.path)?;
        Ok(())
    }

    pub fn done(&self, id: &str) -> bool {
        !self.value["results"][id].is_null()
    }

    pub fn passed(&self, id: &str) -> bool {
        let result = &self.value["results"][id]["result"];
        // Only fresh cells that completed every blocking check can pass.
        // Legacy imports remain historical evidence, regardless of latency.
        result["status"] == "passed" && result["import_kind"].is_null()
    }

    pub fn record(&mut self, id: &str, result: Value) -> Result<()> {
        require(!self.done(id), "cell_already_recorded")?;
        require(self.expected.contains_key(id), "unexpected_cell_id")?;
        self.value["results"][id] = json!({
            "conditions_sha256": self.conditions, "result": result,
        });
        self.save()
    }

    pub fn import(&mut self, source: &Path) -> Result<()> {
        // A completed pre-manifest cell may be imported only after independent
        // source/scope/query/hit/repetition checks, never by timing alone.
        let text = std::fs::read_to_string(source)?;
        let entries = text
            .lines()
            .map(serde_json::from_str::<Value>)
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let preflight = entries
            .iter()
            .find(|entry| entry["stage"] == "preflight")
            .ok_or("import_preflight_missing")?;
        require(
            preflight["scope_sha256"] == self.value["protocol"]["scope_sha256"]
                && preflight["classification"] == self.value["protocol"]["classification"]
                && preflight["repetitions_per_cell"] == super::REPETITIONS
                && preflight["warmups_per_warm_cell"] == super::WARMUPS,
            "import_scope_or_protocol_mismatch",
        )?;
        for entry in entries {
            if entry["stage"] != "cell" || entry["result"].is_null() {
                continue;
            }
            let value = &entry["result"];
            // Old summaries have no binary/condition fingerprint. Preserve an
            // observed failure, but never promote one to acceptance evidence.
            require(
                value["page_p95_lt_300ms"] == false,
                "legacy_import_cannot_grant_pass",
            )?;
            let class = value["class"].as_str().ok_or("import_class_missing")?;
            let cold = value["cache"] == "new_sqlite_connection_not_os_cold";
            let backfill = value["backfill"]
                .as_bool()
                .ok_or("import_backfill_missing")?;
            let id = format!("{class}-{cold}-{backfill}");
            let expected = self.expected.get(&id).ok_or("import_unknown_cell")?;
            require(
                value["repetitions"] == super::REPETITIONS
                    && value["warmup_drains"] == (if cold { 0 } else { super::WARMUPS })
                    && value["first_page"]["count"] == super::REPETITIONS
                    && value["full_drain"]["count"] == super::REPETITIONS,
                "import_protocol_mismatch",
            )?;
            for key in [
                "query_sha256",
                "canonical_sha256",
                "canonical_threads",
                "canonical_hits",
            ] {
                require(value[key] == expected[key], "import_canonical_mismatch")?;
            }
            if !self.done(&id) {
                let mut imported = value.clone();
                imported["import_summary_sha256"] = json!(corpus::hash(text.as_bytes()));
                imported["import_kind"] = json!("pre_manifest_fixed_protocol_explicit_import");
                self.record(&id, imported)?;
            }
        }
        Ok(())
    }

    pub fn summary(&self) -> Value {
        let results = self.value["results"].as_object().expect("results");
        let missing = self
            .expected
            .keys()
            .filter(|id| !results.contains_key(*id))
            .cloned()
            .collect::<Vec<_>>();
        let failed = results.keys().filter(|id| !self.passed(id)).count();
        let latency_reference_exceeded = results
            .values()
            .filter(|entry| entry["result"]["page_p95_lt_300ms"] == false)
            .count();
        json!({
            "stage": "aggregate", "conditions_sha256": self.conditions,
            "completed_cells": results.len(), "missing_cells": missing,
            "latency_policy": "advisory",
            "latency_reference_us_strict": super::LATENCY_REFERENCE_US,
            "latency_reference_exceeded_cells": latency_reference_exceeded,
            "failed_cells": failed, "complete": missing.is_empty(),
            "s8_pass": missing.is_empty() && failed == 0,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slow_validated_cells_pass_only_when_all_cells_are_complete() {
        let temp = tempfile::tempdir().unwrap();
        let mut manifest = Manifest {
            path: temp.path().join("manifest.json"),
            conditions: "fixture".into(),
            expected: BTreeMap::from([("a".into(), json!({})), ("b".into(), json!({}))]),
            value: json!({"results":{}}),
        };
        assert_eq!(manifest.summary()["s8_pass"], false);
        manifest
            .record("a", json!({"status":"passed","page_p95_lt_300ms":true}))
            .unwrap();
        assert_eq!(manifest.summary()["complete"], false);
        assert!(manifest.record("a", json!({})).is_err());
        manifest
            .record("b", json!({"status":"passed","page_p95_lt_300ms":false}))
            .unwrap();
        assert!(manifest.passed("b"));
        assert_eq!(manifest.summary()["complete"], true);
        assert_eq!(manifest.summary()["s8_pass"], true);
        assert_eq!(manifest.summary()["failed_cells"], 0);
        assert_eq!(manifest.summary()["latency_reference_exceeded_cells"], 1);
        assert!(manifest.record("unknown", json!({})).is_err());
    }

    #[test]
    fn blocking_failures_and_unvalidated_evidence_never_pass() {
        for result in [
            json!({"status":"failed","error_class":"canonical_hit_set_mismatch"}),
            json!({"status":"failed","error_class":"seed_current_coverage_missing"}),
            json!({"status":"failed","error_class":"insufficient_copy_capacity"}),
            json!({"status":"failed","page_p95_lt_300ms":true}),
            json!({"page_p95_lt_300ms":true}),
            json!({"page_p95_lt_300ms":false}),
            json!({"status":"passed","page_p95_lt_300ms":true,
                "import_kind":"pre_manifest_fixed_protocol_explicit_import"}),
        ] {
            let temp = tempfile::tempdir().unwrap();
            let mut manifest = Manifest {
                path: temp.path().join("manifest.json"),
                conditions: "fixture".into(),
                expected: BTreeMap::from([("a".into(), json!({}))]),
                value: json!({"results":{}}),
            };
            manifest.record("a", result.clone()).unwrap();
            assert!(!manifest.passed("a"), "{result}");
            assert_eq!(manifest.summary()["complete"], true, "{result}");
            assert_eq!(manifest.summary()["s8_pass"], false, "{result}");
            assert_eq!(manifest.summary()["failed_cells"], 1, "{result}");
        }
    }
}
