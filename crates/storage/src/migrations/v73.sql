CREATE TABLE thread_goal_usage_current (
    goal_id TEXT PRIMARY KEY NOT NULL,
    model_requests BLOB NOT NULL CHECK (typeof(model_requests) = 'blob' AND length(model_requests) = 8 AND model_requests <= x'00000000FFFFFFFF'),
    input_tokens BLOB NOT NULL CHECK (typeof(input_tokens) = 'blob' AND length(input_tokens) = 8),
    output_tokens BLOB NOT NULL CHECK (typeof(output_tokens) = 'blob' AND length(output_tokens) = 8)
);
