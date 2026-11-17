//! Linux test-binary entry shim. libtest creates threads before invoking a test;
//! a mask installed inside a #[test] cannot represent production's entry policy.
//! This opt-in constructor runs before libtest and only in the selected child.
//! The parent and all ordinary tests keep their existing signal policy.
use core_instance::handoff::BlockedShutdownSignals;
static ENTRY_MASK: std::sync::OnceLock<std::sync::Arc<BlockedShutdownSignals>> =
    std::sync::OnceLock::new();
#[used]
#[unsafe(link_section = ".init_array")]
static BLOCK_AT_PROCESS_ENTRY: extern "C" fn() = {
    extern "C" fn entry() {
        if std::env::var_os("EVORCH_MASKED_TEST_CHILD").is_some() {
            let mask = BlockedShutdownSignals::at_entry().expect("single-threaded test entry");
            assert!(ENTRY_MASK.set(std::sync::Arc::new(mask)).is_ok());
        }
    }
    entry
};
pub fn mask() -> &'static std::sync::Arc<BlockedShutdownSignals> {
    ENTRY_MASK.get().expect("opt-in child entry")
}
