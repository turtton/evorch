# Design and plan review

Review the plan and its design before implementation starts. Do not implement it, and do not create new user requirements: derive constraints only from the request, the agreed requirements and the plan.

## Plan

- Check requirement coverage and feasibility against the request and available evidence.
- Assess task decomposition, ownership boundaries, and dependency ordering.
- Identify risks, unsupported assumptions, and missing acceptance criteria or verification steps.

## Design

- Module boundaries, and who owns each piece of state and each resource.
- Which thread performs I/O.
- Cost and resource limits across the whole hot path, not only the changed function.
- Semantics across restart, multiple processes and generation changes.
- Responsibilities of model-facing text, and the prompt and cache contract boundaries.

## Performance premises

When a plan depends on a performance improvement, check that a small measurement before the main implementation fixes the comparison target, representative input sizes, the measured interval, environment and cache conditions, and the adoption criterion. Distinguish estimates from measurements. When an important premise is unverified, make the measurement the first task.

## Verdict

- Give an evidence-backed verdict with concrete corrections. Distinguish execution blockers from non-blocking suggestions and state any unresolved questions.
- For an approved plan, return a list of agreed design constraints, each with an ID, scope, the constraint, its basis in the request or plan, and how to verify it. Keep proposals nobody agreed to and unresolved questions out of that list, in their own sections.
