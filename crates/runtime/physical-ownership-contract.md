# Physical ownership retention (I-6)

These examples are paired with the executable physical_ownership_retention
fixture: real exec, inherited lifetime OFD, sealed ticket, physical Adopt and
runtime DB Adopt. No test constructor grants proof, evidence or authority.

The positive construction path accepts concrete evidence, and creates limited
authority only after the complete claim pass succeeds:

```no_run
use runtime::{AgentRuntime, DisposalAuthority, OwnedThreadCas, RestartOwnershipEvidence};
use runtime::ownership::OwnerHost;
use storage::recovery::graph_control::GraphControlCas;
fn retained(rt: &AgentRuntime, owner: &OwnerHost, evidence: RestartOwnershipEvidence,
    graph: GraphControlCas, owners: Vec<OwnedThreadCas>) -> Result<DisposalAuthority, Box<dyn std::error::Error>> {
    rt.claim_physical_bootstrap_owners(owner, evidence, || {})?;
    Ok(rt.disposal_authority(graph, owners)?)
}
```

## Arbitrary evidence cannot enter retention

Positive: the concrete-evidence call above type-checks. Ordinary planned claim
accepts the trait, but does not retain (see the executable negative).

```compile_fail,E0308
use runtime::ownership::{AdoptedOwnershipEvidence, OwnerHost, OwnershipError, PlannedHold};
struct Forged;
impl AdoptedOwnershipEvidence for Forged {
    fn validate_hold(&self, _: &PlannedHold) -> Result<(), OwnershipError> { Ok(()) }
}
fn forged(rt: &runtime::AgentRuntime, owner: &OwnerHost) {
    rt.claim_physical_bootstrap_owners(owner, Forged, || {});
}
```

## DB values and ordinary permits are not capabilities

Positive: observations and ordinary permits remain usable as their own types.

```no_run
fn observations(owner: &runtime::ownership::OwnerHost, thread: &str) {
    let row = owner.attach(thread).unwrap();
    let _: Option<Box<runtime::ownership::PlannedHold>> = row.handoff_hold;
    let _: runtime::ownership::OwnerPermit = owner.owned_permit(thread).unwrap();
}
```

```compile_fail,E0277
fn permit(p: runtime::ownership::OwnerPermit) -> runtime::DisposalAuthority { p.into() }
```

```compile_fail,E0277
fn hold(h: runtime::ownership::PlannedHold) -> runtime::DisposalAuthority { h.into() }
```

```compile_fail,E0277
fn recipe(r: runtime::quiesce::RecoveryRecipe) -> runtime::DisposalAuthority { r.into() }
```

```compile_fail,E0277
fn receipt(r: core_instance::handoff::receipt::RestartBootReceipt) -> runtime::DisposalAuthority { r.into() }
```

## Opaque construction cannot bypass the runtime path

The retained function above is the construction-positive counterpart for every
negative below (not merely acceptance of a typed opaque parameter).

```compile_fail
let _ = runtime::DisposalAuthority {};
```

```compile_fail,E0277
let _ = <runtime::DisposalAuthority as Default>::default();
```

```compile_fail,E0277
let _: runtime::DisposalAuthority = serde_json::from_str("{}").unwrap();
```

## Disposal cannot become execution authority

Positive: independent current execution authority supplies an execution config.

```no_run
fn execution(current: runtime::CurrentRecoveryAuthority, run: runtime::RunId) -> runtime::RunConfig {
    current.members.get(&run).unwrap().clone()
}
```

```compile_fail,E0277
fn execution(disposal: runtime::DisposalAuthority) -> runtime::RunConfig { disposal.into() }
```

```compile_fail,E0277
fn current(disposal: runtime::DisposalAuthority) -> runtime::CurrentRecoveryAuthority { disposal.into() }
```
