//! A single background worker that only ever runs the newest request.
//!
//! The thread search modal can be closed and reopened while a storage read is
//! still running. Submitting a job replaces the one waiting to start (dropping
//! it, and with it any channel it holds, which tells its receiver the work was
//! superseded), and at most one job runs at a time.
