//! Run-owned evidence of rule bodies actually appended to the current history.
use super::{
    render::{RenderCoverage, render_sections},
    types::ResolvedRule,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashSet};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Coverage {
    Full,
    Truncated,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Injection {
    fingerprint: String,
    coverage: Coverage,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub(crate) struct ProjectRuleInjections(BTreeMap<String, Injection>);

pub(crate) struct PreparedRuleInjection {
    pub text: String,
    records: Vec<(String, Injection)>,
}

impl ProjectRuleInjections {
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
    pub fn forget_history(&mut self) {
        self.0.clear();
    }
    /// Call only in the synchronous interval that appends the prepared text.
    pub fn commit(&mut self, prepared: PreparedRuleInjection) {
        self.0.extend(prepared.records);
    }
}

pub(super) fn prepare(
    rules: Vec<ResolvedRule>,
    markers: Vec<String>,
    budget: u64,
    injected: &ProjectRuleInjections,
) -> Option<PreparedRuleInjection> {
    let fingerprints: BTreeMap<_, _> = rules
        .iter()
        .map(|r| {
            (
                r.source.canonical_path.clone(),
                format!("{:x}", Sha256::digest(r.body.as_bytes())),
            )
        })
        .collect();
    // A canonical path owns identity; traversal-dependent display aliases must
    // not turn the same body into alternating versions.
    let same = |path: &std::path::Path, coverage| {
        path.to_str()
            .and_then(|p| injected.0.get(p))
            .is_some_and(|i| i.coverage == coverage && i.fingerprint == fingerprints[path])
    };
    let rules = rules
        .into_iter()
        .filter(|r| !same(&r.source.canonical_path, Coverage::Full))
        .collect();
    let mut rendered = render_sections("[project-rules]", rules, budget);
    rendered.sections.retain(|s| {
        s.coverage != RenderCoverage::Truncated || !same(&s.canonical_path, Coverage::Truncated)
    });
    let records = rendered
        .sections
        .iter()
        .filter_map(|s| {
            let coverage = match s.coverage {
                RenderCoverage::Full => Coverage::Full,
                RenderCoverage::Truncated => Coverage::Truncated,
                RenderCoverage::Omitted => return None,
            };
            // Path's serde representation requires UTF-8. Such rules still render,
            // but cannot make a later snapshot fail merely by being encountered.
            let path = s.canonical_path.to_str()?.to_owned();
            Some((
                path,
                Injection {
                    fingerprint: fingerprints[&s.canonical_path].clone(),
                    coverage,
                },
            ))
        })
        .collect();
    let mut text = rendered.text();
    let mut seen = HashSet::new();
    let markers = markers
        .into_iter()
        .filter(|m| seen.insert(m.clone()))
        .collect::<Vec<_>>();
    if !markers.is_empty() {
        if text.is_empty() {
            text.push_str("[project-rules]");
        }
        text.push('\n');
        text.push_str(&markers.join("\n"));
    }
    (!text.is_empty()).then_some(PreparedRuleInjection { text, records })
}

#[cfg(test)]
mod tests;
