//! Reserved evaluation namespace. Not implemented: every type is an empty
//! enum, so nothing can be sent, answered or advertised until a later PR gives
//! this module real operations.

reserved_namespace!(
    EvaluationCommand,
    EvaluationResult,
    EvaluationQuery,
    EvaluationReply,
    EvaluationEvent
);
