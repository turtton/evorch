// What the writer may do for a pull request event. Pure functions: the workflow's job
// conditions say the same in YAML, and these are the tested statement of it.

/**
 * Only a pull request whose head lives in this repository may write (its token can; a
 * fork's is read-only). A head repository that is missing (a deleted fork) or named
 * differently is not this repository.
 */
export function canWrite(pullRequest, repository) {
  const head = pullRequest?.head?.repo;
  if (!head || typeof head.full_name !== 'string') return false;
  if (head.full_name !== repository) return false;
  return head.fork !== true;
}

/**
 * `cleanup`: the PR was closed (merged or not); it never depends on the capture result
 *            or on which paths changed.
 * `irrelevant`: the PR changes nothing the screens depend on.
 * `publish`: the capture result is shown.
 */
export function decideMode(action, relevant) {
  if (action === 'closed') return 'cleanup';
  if (relevant === false) return 'irrelevant';
  return 'publish';
}
