//! Physical handoff evidence, not reconstructible from durable state.
/// Verified inheritance of a lifetime lock and sealed handoff ticket.
/// No serialization or public production constructor: only the adopting
/// process may authorize planned recovery (D10).
///
/// Durable JSON cannot manufacture execution authority:
/// ```compile_fail
/// let _: core_instance::InheritedHandoffProof = serde_json::from_str("{}").unwrap();
/// ```
/// The physical-evidence constructor is crate-private:
/// ```compile_fail
/// let _ = core_instance::InheritedHandoffProof::new("tx".into(),"config".into(),2,"nonce".into(),"build".into());
/// ```
#[derive(Debug)]
pub struct InheritedHandoffProof {
    restart_id: String,
    config_identity: String,
    successor_epoch: u64,
    nonce_hash: String,
    successor_build: String,
}
impl InheritedHandoffProof {
    // 07b-4a calls this only after validating the inherited descriptors.
    #[allow(dead_code)]
    pub(crate) fn new(
        restart_id: String,
        config_identity: String,
        successor_epoch: u64,
        nonce_hash: String,
        successor_build: String,
    ) -> Self {
        Self {
            restart_id,
            config_identity,
            successor_epoch,
            nonce_hash,
            successor_build,
        }
    }
    pub fn restart_id(&self) -> &str {
        &self.restart_id
    }
    pub fn config_identity(&self) -> &str {
        &self.config_identity
    }
    pub fn successor_epoch(&self) -> u64 {
        self.successor_epoch
    }
    pub fn nonce_hash(&self) -> &str {
        &self.nonce_hash
    }
    pub fn successor_build(&self) -> &str {
        &self.successor_build
    }
}
