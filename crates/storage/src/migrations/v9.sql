CREATE TABLE user_questions (
 id TEXT PRIMARY KEY, run_id TEXT NOT NULL, root_run_id TEXT NOT NULL,
 root_name TEXT NOT NULL, payload TEXT NOT NULL, answered INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX user_questions_run ON user_questions(run_id);
CREATE INDEX user_questions_root ON user_questions(root_name, answered);
CREATE INDEX user_questions_pending ON user_questions(answered);
CREATE TABLE user_question_links (
 question_id TEXT NOT NULL REFERENCES user_questions(id), run_id TEXT NOT NULL,
 PRIMARY KEY(question_id, run_id)
);
CREATE INDEX user_question_links_run ON user_question_links(run_id);
