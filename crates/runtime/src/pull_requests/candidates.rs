//! Finding pull requests in command output, locally (ADR 0043).
//!
//! A person ran `gh pr create` (or an agent did for them) and the output names the new pull
//! request: this module reads github.com pull request URLs out of such text. Nothing here
//! fetches, links or stores anything; a found reference only becomes a *candidate* that the
//! person confirms or dismisses. The tool result and the history are never changed.
//!
//! Output arrives whole (a finished tool call) or in chunks (an asynchronous shell job), and a
//! URL can be cut by a chunk boundary, so [`StreamScanner`] holds back a URL that touches the
//! end of what it has seen until more text, or the end of the stream, settles it.

use storage::pull_requests::PullRequestKey;

use super::locator::parse_locator;

/// Most distinct pull requests one output yields. A listing of a hundred pull requests is not a
/// pull request the person just made.
pub const MAX_CANDIDATES_PER_OUTPUT: usize = 8;

/// Most bytes of one output that are searched.
pub const MAX_SCANNED_BYTES: usize = 1024 * 1024;

/// How much of the end of a chunk is kept so that a URL cut by the next chunk is still whole.
/// The longest accepted reference (`https://github.com/<39>/<100>/pull/<10>/checks`) is shorter.
const TAIL_BYTES: usize = 320;

const URL_START: &str = "https://github.com/";

/// Tools whose output can hold a pull request the person just made: the ones that run commands.
#[must_use]
pub fn is_command_tool(tool_name: &str) -> bool {
    matches!(tool_name, "shell" | "run_js")
}

/// One accepted reference and whether text followed it (so it cannot grow any more).
struct Found {
    key: PullRequestKey,
    terminated: bool,
}

fn is_token_end(ch: char) -> bool {
    ch.is_whitespace()
        || ch.is_control()
        || matches!(
            ch,
            '"' | '\'' | '<' | '>' | ')' | ']' | '}' | '`' | ',' | ';' | '|' | '\\'
        )
}

fn is_boundary_before(previous: Option<char>) -> bool {
    previous.is_none_or(|ch| !(ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '_' | '@')))
}

/// Every pull request URL in `text`, in order of appearance (repeats included).
fn find(text: &str) -> Vec<Found> {
    let mut found = Vec::new();
    let mut from = 0;
    while let Some(relative) = text[from..].find(URL_START) {
        let start = from + relative;
        from = start + URL_START.len();
        if !is_boundary_before(text[..start].chars().next_back()) {
            continue;
        }
        let rest = &text[start..];
        let end = rest.find(is_token_end);
        let terminated = end.is_some();
        let token = &rest[..end.unwrap_or(rest.len())];
        let token = token.trim_end_matches(['.', ':', '!', '?', '*']);
        if let Ok(key) = parse_locator(token) {
            found.push(Found { key, terminated });
        }
    }
    found
}

fn push_unique(into: &mut Vec<PullRequestKey>, key: PullRequestKey) {
    if into.len() < MAX_CANDIDATES_PER_OUTPUT && !into.contains(&key) {
        into.push(key);
    }
}

/// The pull requests named in a complete `text`, without repeats, at most
/// [`MAX_CANDIDATES_PER_OUTPUT`].
#[must_use]
pub fn scan_text(text: &str) -> Vec<PullRequestKey> {
    let mut keys = Vec::new();
    for found in find(floor_boundary(text, MAX_SCANNED_BYTES)) {
        push_unique(&mut keys, found.key);
    }
    keys
}

fn floor_boundary(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// Reads the chunks of one asynchronous shell job.
#[derive(Debug, Default)]
pub struct StreamScanner {
    tail: String,
    expected_offset: Option<u64>,
    seen_bytes: usize,
    keys: Vec<PullRequestKey>,
}

impl StreamScanner {
    /// An empty scanner.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Takes the chunk that starts at `offset` of the job's output. A chunk that does not
    /// continue the previous one (the output in between expired) drops the held-back tail, so
    /// two pieces are never joined into a URL that was not there.
    pub fn push(&mut self, offset: u64, chunk: &str) {
        if self.expected_offset != Some(offset) {
            self.tail.clear();
        }
        self.expected_offset = Some(offset.saturating_add(chunk.len() as u64));
        if self.seen_bytes >= MAX_SCANNED_BYTES {
            return;
        }
        self.seen_bytes = self.seen_bytes.saturating_add(chunk.len());
        let mut text = std::mem::take(&mut self.tail);
        text.push_str(chunk);
        for found in find(&text) {
            if found.terminated {
                push_unique(&mut self.keys, found.key);
            }
        }
        let mut cut = text.len().saturating_sub(TAIL_BYTES);
        while !text.is_char_boundary(cut) {
            cut += 1;
        }
        self.tail = text.split_off(cut);
    }

    /// The stream ended: what was held back can not grow any more.
    #[must_use]
    pub fn finish(mut self) -> Vec<PullRequestKey> {
        for found in find(&self.tail) {
            push_unique(&mut self.keys, found.key);
        }
        self.keys
    }
}

/// Whether a command line is one whose end can change the state of a pull request: a
/// `git push` or any `gh pr …`. Looks at each simple command of a `&&`/`||`/`;`/`|` chain and
/// of every line, ignoring leading `VAR=value` words and `sudo`/`env`; it does not run or
/// expand anything.
#[must_use]
pub fn is_publishing_command(command: &str) -> bool {
    command
        .split(['\n', ';', '|', '&'])
        .any(|segment| segment_is_publishing(segment.split_whitespace()))
}

fn segment_is_publishing<'a>(words: impl Iterator<Item = &'a str>) -> bool {
    let mut words = words
        .map(|word| word.trim_matches(['"', '\'', '(', '{']))
        .filter(|word| !word.is_empty())
        .skip_while(|word| word.contains('=') && !word.starts_with('-'))
        .skip_while(|word| matches!(*word, "sudo" | "env" | "command" | "exec" | "time"))
        .skip_while(|word| word.contains('=') && !word.starts_with('-'));
    let program = words
        .next()
        .map(|word| word.rsplit('/').next().unwrap_or(word));
    match program {
        Some("gh") => words.next() == Some("pr"),
        Some("git") => {
            while let Some(word) = words.next() {
                match word {
                    "-C" | "-c" | "--git-dir" | "--work-tree" | "--namespace" => {
                        words.next();
                    }
                    "push" => return true,
                    word if word.starts_with('-') => {}
                    _ => return false,
                }
            }
            false
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(repository: &str, number: u64) -> PullRequestKey {
        PullRequestKey::github(repository, number).unwrap()
    }

    // Given: the usual output of `gh pr create` / When: scanned / Then: the pull request is found
    #[test]
    fn the_output_of_gh_pr_create_names_the_pull_request() {
        let output = "Creating pull request for feat into main in turtton/evorch\n\nhttps://github.com/turtton/evorch/pull/486\n";

        assert_eq!(scan_text(output), vec![key("turtton/evorch", 486)]);
    }

    // Given: URLs in prose, quotes, markdown and with a trailing page / When: scanned / Then:
    // each reference is read exactly once, whatever surrounds it
    #[test]
    fn urls_are_read_through_punctuation_and_deduplicated() {
        let output = "see (https://github.com/o/r/pull/1), \"https://github.com/o/r/pull/2/files\" and \
                      [x](https://github.com/o/r/pull/3#discussion_r1). Again https://github.com/o/r/pull/1.";

        assert_eq!(
            scan_text(output),
            vec![key("o/r", 1), key("o/r", 2), key("o/r", 3)]
        );
    }

    // Given: lookalike hosts, other pages and a URL glued to a word / When: scanned / Then: none
    #[test]
    fn only_exact_github_dot_com_pull_request_urls_count() {
        for text in [
            "https://github.com.evil.com/o/r/pull/1",
            "https://github.enterprise.example/o/r/pull/1",
            "http://github.com/o/r/pull/1",
            "https://github.com/o/r/issues/1",
            "https://github.com/o/r/pull/0",
            "https://github.com/o/r/pull/abc",
            "xhttps://github.com/o/r/pull/1",
            "user@https://github.com/o/r/pull/1",
            "https://api.github.com/repos/o/r/pulls/1",
        ] {
            assert_eq!(scan_text(text), Vec::new(), "{text}");
        }
    }

    // Given: a listing of many pull requests / When: scanned / Then: only the first few are kept
    #[test]
    fn a_long_listing_is_cut_to_the_first_candidates() {
        let output: String = (1..=50)
            .map(|n| format!("https://github.com/o/r/pull/{n}\n"))
            .collect();

        let keys = scan_text(&output);

        assert_eq!(keys.len(), MAX_CANDIDATES_PER_OUTPUT);
        assert_eq!(keys[0], key("o/r", 1));
    }

    // Given: a URL cut at every possible byte / When: the chunks are streamed / Then: the
    // pull request is found once, with the full number
    #[test]
    fn a_url_cut_by_any_chunk_boundary_is_found_whole() {
        let output = "done\nhttps://github.com/turtton/evorch/pull/1234\nbye\n";
        for cut in 0..=output.len() {
            if !output.is_char_boundary(cut) {
                continue;
            }
            let mut scanner = StreamScanner::new();
            scanner.push(0, &output[..cut]);
            scanner.push(cut as u64, &output[cut..]);

            assert_eq!(
                scanner.finish(),
                vec![key("turtton/evorch", 1234)],
                "cut at {cut}"
            );
        }
    }

    // Given: the number is cut in two chunks / When: the first chunk alone is seen / Then: no
    // premature pull request is named (the URL may still grow)
    #[test]
    fn a_url_at_the_end_of_the_seen_text_is_not_final_until_more_arrives() {
        let mut scanner = StreamScanner::new();
        scanner.push(0, "https://github.com/o/r/pull/1");
        assert!(scanner.keys.is_empty());
        scanner.push(29, "23 \n");

        assert_eq!(scanner.finish(), vec![key("o/r", 123)]);
    }

    // Given: a stream that ends right after a URL / When: finished / Then: the URL counts
    #[test]
    fn the_end_of_the_stream_settles_the_last_url() {
        let mut scanner = StreamScanner::new();
        scanner.push(0, "https://github.com/o/r/pull/9");

        assert_eq!(scanner.finish(), vec![key("o/r", 9)]);
    }

    // Given: chunks with expired output between them / When: streamed / Then: the halves are
    // not joined into a URL that was never printed
    #[test]
    fn a_gap_in_the_stream_never_joins_two_pieces() {
        let mut scanner = StreamScanner::new();
        scanner.push(0, "https://github.com/o/r/pu");
        scanner.push(500, "ll/7\n");

        assert_eq!(scanner.finish(), Vec::new());
    }

    // Given: multi-byte text around the cut / When: streamed in tiny chunks / Then: no panic
    #[test]
    fn multibyte_text_does_not_break_the_tail() {
        let mut scanner = StreamScanner::new();
        let text = format!("{}https://github.com/o/r/pull/5 完了\n", "あ".repeat(400));
        let mut offset = 0_u64;
        for piece in text.as_bytes().chunks(7) {
            // Chunks come from a UTF-8 aware producer; emulate with lossy pieces only at
            // char boundaries.
            if let Ok(piece) = std::str::from_utf8(piece) {
                scanner.push(offset, piece);
                offset += piece.len() as u64;
            } else {
                scanner.expected_offset = None;
            }
        }
        let _ = scanner.finish();
    }

    // Given: commands that publish / When: classified / Then: pushes and `gh pr` count and
    // other commands, including text that merely mentions them, do not
    #[test]
    fn publishing_commands_are_pushes_and_gh_pr() {
        for yes in [
            "git push",
            "git push origin feat",
            "git -C /work/repo push --force-with-lease",
            "git -c core.x=1 push",
            "cd repo && gh pr create --fill",
            "GH_TOKEN=x gh pr merge 12",
            "sudo git push",
            "echo hi; gh pr checks",
            "/usr/bin/gh pr view 3",
            "make && \\\n git push",
        ] {
            assert!(is_publishing_command(yes), "{yes}");
        }
        for no in [
            "git status",
            "git commit -m 'git push'",
            "echo git push",
            "gh issue create",
            "gh auth status",
            "cargo test",
            "",
        ] {
            assert!(!is_publishing_command(no), "{no}");
        }
    }
}
