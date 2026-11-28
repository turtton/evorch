//! Prompt cache retention: how much of the previous request's measured cache
//! the next request read back. A request's first send can never hit, so this
//! stays near 100% for a healthy cache regardless of session length, unlike
//! the billed `cache_read / input` ratio.
