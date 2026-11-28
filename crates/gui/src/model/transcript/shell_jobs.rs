//! Asynchronous shell jobs seen by one transcript: what each job runs, its
//! state, and its output log.
//!
//! Live output events carry the redacted stream with offsets and are not
//! persisted. Restored history only has the start and poll results the agent
//! received, so those cursor-ordered deltas form the fallback log.
