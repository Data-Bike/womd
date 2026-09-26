/**
 * Fetch GitHub auth status and repository metadata independently.
 *
 * `github_repo_metadata` legitimately fails whenever the open file is not
 * inside a GitHub-backed repository — a very common state — and that failure
 * must NOT report an authenticated user as logged out (and vice versa).
 * Each call has its own fallback value.
 */
export async function fetchGitHubStatus(invokeFn) {
  let auth;
  try {
    auth = await invokeFn('github_auth_status');
  } catch (e) {
    // Not an error: gh missing / unauthenticated is a normal state — keep it
    // out of the error console while still leaving a diagnostic trail.
    console.warn('github auth status:', e);
    auth = { authenticated: false, user: '' };
  }

  let repo;
  try {
    repo = await invokeFn('github_repo_metadata');
  } catch (e) {
    console.warn('github repo metadata:', e);
    repo = { full_name: '', default_branch: '', html_url: '' };
  }

  return { auth, repo };
}
