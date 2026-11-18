//! The online maintenance tasks of the core as a CLI sees them (ADR 0045,
//! stage 17d): start one task, follow its operation until it settles and report
//! its progress. `evorch maintenance backfill-events` is the first user.
