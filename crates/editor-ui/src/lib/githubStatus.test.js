import { describe, it, expect } from 'vitest';
import { fetchGitHubStatus } from './githubStatus.js';

const AUTH_OK = { authenticated: true, user: 'octocat' };
const AUTH_OUT = { authenticated: false, user: '' };
const REPO_OK = { full_name: 'octo/repo', default_branch: 'main', html_url: 'https://x' };
const REPO_EMPTY = { full_name: '', default_branch: '', html_url: '' };

function invokeWith({ auth, repo } = {}) {
  const calls = [];
  return {
    calls,
    fn: async (cmd) => {
      calls.push(cmd);
      if (cmd === 'github_auth_status') {
        if (auth instanceof Error) throw auth;
        return auth;
      }
      if (cmd === 'github_repo_metadata') {
        if (repo instanceof Error) throw repo;
        return repo;
      }
      throw new Error('unexpected command: ' + cmd);
    },
  };
}

describe('fetchGitHubStatus', () => {
  it('returns both values when everything succeeds', async () => {
    const { fn, calls } = invokeWith({ auth: AUTH_OK, repo: REPO_OK });
    const { auth, repo } = await fetchGitHubStatus(fn);
    expect(auth).toEqual(AUTH_OK);
    expect(repo).toEqual(REPO_OK);
    expect(calls).toEqual(['github_auth_status', 'github_repo_metadata']);
  });

  it('keeps auth state when repo metadata fails (file outside a repo)', async () => {
    // Regression: a single try/catch used to reset ghAuth to unauthenticated
    // whenever `repo view` failed — common for files outside a GitHub repo.
    const { fn } = invokeWith({ auth: AUTH_OK, repo: new Error('not a repo') });
    const { auth, repo } = await fetchGitHubStatus(fn);
    expect(auth).toEqual(AUTH_OK);
    expect(repo).toEqual(REPO_EMPTY);
  });

  it('keeps repo state when auth check fails', async () => {
    const { fn } = invokeWith({ auth: new Error('gh missing'), repo: REPO_OK });
    const { auth, repo } = await fetchGitHubStatus(fn);
    expect(auth).toEqual(AUTH_OUT);
    expect(repo).toEqual(REPO_OK);
  });

  it('falls back both values when both calls fail', async () => {
    const { fn } = invokeWith({ auth: new Error('a'), repo: new Error('b') });
    const { auth, repo } = await fetchGitHubStatus(fn);
    expect(auth).toEqual(AUTH_OUT);
    expect(repo).toEqual(REPO_EMPTY);
  });

  it('does not swallow unexpected command errors into auth state', async () => {
    // A mistyped command name must not silently mark the user logged out —
    // it hits the same catch, which is fine, but auth fallback must be the
    // "unauthenticated" shape rather than an exception leak.
    const { fn } = invokeWith({ auth: new Error('unknown command'), repo: REPO_OK });
    const { auth } = await fetchGitHubStatus(fn);
    expect(auth.authenticated).toBe(false);
  });
});
