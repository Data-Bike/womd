<template>
  <div id="git-panel" :class="{ hidden: !visible }" :style="{ width: `${width}px` }">
    <div id="git-panel-tabs">
      <button
        v-for="t in tabs"
        :key="t.key"
        class="git-tab"
        :class="{ active: activeTab === t.key }"
        @click="activeTab = t.key"
      >
        {{ t.label }}
      </button>
    </div>
    <div id="git-panel-content">
      <!-- Changes -->
      <div v-if="activeTab === 'changes'" class="git-section">
        <div v-if="status" class="git-section-title">
          Branch: {{ status.branch || 'none' }} {{ status.dirty ? '(dirty)' : '(clean)' }}
        </div>
        <div v-if="status?.staged?.length" class="git-section-title">Staged</div>
        <div
          v-for="f in status?.staged"
          :key="f.path + ':staged'"
          class="git-file-entry"
          @click="showFileDiff(f)"
        >
          <span class="git-file-status git-status-M">{{ statusLetter(f.status) }}</span>
          <span class="git-file-path">{{ f.path }}</span>
          <button class="git-mini-btn" @click.stop="unstage(f.path)">−</button>
        </div>
        <div v-if="status?.changes?.length" class="git-section-title">Changes</div>
        <div
          v-for="f in status?.changes"
          :key="f.path + ':changes'"
          class="git-file-entry"
          @click="showFileDiff(f)"
        >
          <span class="git-file-status" :class="statusClass(f.status)">{{ statusLetter(f.status) }}</span>
          <span class="git-file-path">{{ f.path }}</span>
          <button class="git-mini-btn" @click.stop="stage(f.path)">+</button>
          <button class="git-mini-btn git-mini-danger" @click.stop="discard(f.path)">x</button>
        </div>
        <div v-if="status?.untracked?.length" class="git-section-title">Untracked</div>
        <div
          v-for="f in status?.untracked"
          :key="f.path + ':untracked'"
          class="git-file-entry"
        >
          <span class="git-file-status git-status-Untracked">?</span>
          <span class="git-file-path">{{ f.path }}</span>
          <button class="git-mini-btn git-mini-danger" @click.stop="removeUntracked(f.path)">x</button>
        </div>
        <div v-if="status?.conflicted?.length" class="git-section-title">Conflicted</div>
        <div
          v-for="f in status?.conflicted"
          :key="f.path + ':conflict'"
          class="git-file-entry"
        >
          <span class="git-file-status git-status-Untracked">!</span>
          <span class="git-file-path">{{ f.path }}</span>
        </div>
        <div v-if="!hasAnyChanges" class="file-tree-empty">No changes</div>
        <div class="git-inline-form" style="margin-top: 12px;">
          <input v-model="commitMessage" placeholder="Commit message" />
          <button class="git-action-btn" @click="doCommit">Commit</button>
        </div>

        <!-- Inline diff for selected file -->
        <div v-if="selectedFileDiff" class="git-file-diff-inline">
          <div class="git-file-diff-header" @click="selectedFileDiff = null">
            <span class="chevron">▶</span>
            <span>{{ selectedFileDiff.path }}</span>
          </div>
          <div>
            <div v-for="(hunk, hi) in selectedFileDiff.hunks" :key="hi" class="diff-hunk">
              <div class="diff-hunk-header">@@ -{{ hunk.old_start }} +{{ hunk.new_start }} @@</div>
              <div
                v-for="(line, li) in hunk.lines"
                :key="li"
                class="diff-line"
                :class="line.kind"
              >
                <span class="diff-line-num">{{ lineNo(line) }}</span>
                <span class="diff-line-sign">{{ sign(line.kind) }}</span>
                <span class="diff-line-content">{{ line.text }}</span>
              </div>
            </div>
            <div v-if="!selectedFileDiff.hunks?.length" class="file-tree-empty">No diff</div>
          </div>
        </div>
      </div>

      <!-- Branches -->
      <div v-if="activeTab === 'branches'" class="git-section">
        <div class="git-section-title">Branches</div>
        <div
          v-for="b in branches"
          :key="b.name"
          class="git-branch-entry"
          :class="{ current: b.is_current }"
        >
          <div class="git-branch-row">
            <span class="git-branch-icon">{{ b.is_remote ? '[R]' : '[L]' }}</span>
            <span class="git-branch-name">{{ b.name }}</span>
            <span v-if="b.ahead" class="git-branch-ahead">+{{ b.ahead }}</span>
            <span v-if="b.behind" class="git-branch-behind">−{{ b.behind }}</span>
          </div>
          <div class="git-branch-actions">
            <button v-if="!b.is_current" class="git-mini-btn" @click="checkoutBranch(b.name)">Checkout</button>
            <button v-if="!b.is_current" class="git-mini-btn git-mini-danger" @click="deleteBranch(b.name)">Del</button>
          </div>
        </div>
        <div class="git-inline-form">
          <input v-model="newBranchName" placeholder="New branch name" />
          <button class="git-action-btn" @click="createBranch">Create</button>
        </div>
      </div>

      <!-- History -->
      <div v-if="activeTab === 'history'" class="git-section">
        <div class="git-section-title">History</div>
        <div
          v-for="c in log"
          :key="c.sha"
          class="git-commit-entry"
        >
          <div class="git-commit-row">
            <div class="git-commit-sha" @click="showCommitDiff(c.sha)">{{ c.sha.substring(0, 8) }}</div>
            <div class="git-commit-msg">{{ c.message }}</div>
            <div class="git-commit-meta">{{ c.author }} — {{ c.date }}</div>
          </div>
          <div class="git-commit-actions">
            <button class="git-mini-btn" @click="checkoutCommit(c.sha)">Checkout</button>
          </div>
        </div>
      </div>

      <!-- Diff -->
      <div v-if="activeTab === 'diff'" class="git-section">
        <div class="git-diff-viewer-form">
          <select v-model="diffMode">
            <option value="working">Working tree vs HEAD</option>
            <option value="commit">Working tree vs commit</option>
            <option value="commits">Between two commits</option>
          </select>
          <input v-if="diffMode === 'commit'" v-model="diffCommit" placeholder="Commit SHA" />
          <input v-if="diffMode === 'commits'" v-model="diffCommitA" placeholder="From SHA" />
          <input v-if="diffMode === 'commits'" v-model="diffCommitB" placeholder="To SHA" />
          <button class="git-action-btn" @click="loadDiff">Load diff</button>
        </div>
        <div
          v-for="(file, fi) in diff"
          :key="fi"
          class="diff-file-header"
        >
          {{ file.path }}
        </div>
        <div v-for="(file, fi) in diff" :key="'body' + fi">
          <div v-for="(hunk, hi) in file.hunks" :key="hi" class="diff-hunk">
            <div class="diff-hunk-header">@@ -{{ hunk.old_start }} +{{ hunk.new_start }} @@</div>
            <div
              v-for="(line, li) in hunk.lines"
              :key="li"
              class="diff-line"
              :class="line.kind"
            >
              <span class="diff-line-num">{{ lineNo(line) }}</span>
              <span class="diff-line-sign">{{ sign(line.kind) }}</span>
              <span class="diff-line-content">{{ line.text }}</span>
            </div>
          </div>
        </div>
        <div v-if="!diff?.length" class="file-tree-empty">Select a diff mode and click Load diff</div>
      </div>

      <!-- Stash -->
      <div v-if="activeTab === 'stash'" class="git-section">
        <div class="git-section-title">Stashes</div>
        <div
          v-for="s in stash"
          :key="s.index"
          class="git-stash-entry"
        >
          <div class="git-stash-info">
            <span class="git-stash-idx">stash@&#123;{{ s.index }}&#125;</span>
            <span class="git-stash-msg">{{ s.message }}</span>
            <span class="git-commit-meta">{{ s.branch }}</span>
          </div>
          <div class="git-stash-actions">
            <button class="git-mini-btn" @click="stashPop(s.index)">Pop</button>
            <button class="git-mini-btn" @click="stashApply(s.index)">Apply</button>
            <button class="git-mini-btn git-mini-danger" @click="stashDrop(s.index)">Drop</button>
          </div>
        </div>
        <div class="git-inline-form">
          <input v-model="stashMessage" placeholder="Stash message" />
          <button class="git-action-btn" @click="stashPush">Push stash</button>
        </div>
        <div v-if="!stash?.length" class="file-tree-empty">No stashes</div>
      </div>

      <!-- Tags -->
      <div v-if="activeTab === 'tags'" class="git-section">
        <div class="git-section-title">Tags</div>
        <div
          v-for="t in tags"
          :key="t.name"
          class="git-tag-entry"
        >
          <span class="git-tag-icon">T</span>
          <span class="git-tag-name">{{ t.name }}</span>
          <span class="git-tag-target">{{ t.target.substring(0, 8) }}</span>
          <span class="git-tag-msg">{{ t.message }}</span>
          <button class="git-mini-btn git-mini-danger" @click="deleteTag(t.name)">Del</button>
        </div>
        <div class="git-inline-form">
          <input v-model="newTagName" placeholder="Tag name" />
          <input v-model="newTagMessage" placeholder="Tag message (optional)" />
          <button class="git-action-btn" @click="createTag">Create</button>
        </div>
        <div v-if="!tags?.length" class="file-tree-empty">No tags</div>
      </div>

      <!-- Remotes -->
      <div v-if="activeTab === 'remotes'" class="git-section">
        <div class="git-section-title">Remotes</div>
        <div
          v-for="r in remotes"
          :key="r.name"
          class="git-remote-entry"
        >
          <div class="git-remote-info">
            <span class="git-remote-name">{{ r.name }}</span>
            <span class="git-remote-url">{{ r.url }}</span>
          </div>
          <div class="git-remote-actions">
            <button class="git-mini-btn" @click="fetchRemote(r.name)">Fetch</button>
            <button class="git-mini-btn" @click="pullRemote(r.name)">Pull</button>
            <button class="git-mini-btn" @click="pushRemote(r.name)">Push</button>
          </div>
        </div>
        <div class="git-inline-form">
          <input v-model="newRemoteName" placeholder="Remote name" />
          <input v-model="newRemoteUrl" placeholder="Remote URL" />
          <button class="git-action-btn" @click="addRemote">Add</button>
        </div>
        <div v-if="!remotes?.length" class="file-tree-empty">No remotes</div>
      </div>
    </div>
  </div>
</template>

<script setup>
import { ref, computed, watch, onMounted } from 'vue';
import { invoke } from '@tauri-apps/api/core';

const props = defineProps({
  visible: Boolean,
  activeFile: String,
  width: { type: Number, default: 380 },
});

const tabs = [
  { key: 'changes', label: 'Changes' },
  { key: 'branches', label: 'Branches' },
  { key: 'history', label: 'History' },
  { key: 'diff', label: 'Diff' },
  { key: 'stash', label: 'Stash' },
  { key: 'tags', label: 'Tags' },
  { key: 'remotes', label: 'Remotes' },
];

const activeTab = ref('changes');
const status = ref(null);
const branches = ref([]);
const log = ref([]);
const history = ref([]);
const diff = ref([]);
const stash = ref([]);
const tags = ref([]);
const remotes = ref([]);

const commitMessage = ref('');
const newBranchName = ref('');
const stashMessage = ref('');
const newTagName = ref('');
const newTagMessage = ref('');
const newRemoteName = ref('');
const newRemoteUrl = ref('');

const selectedFileDiff = ref(null);
const diffMode = ref('working');
const diffCommit = ref('');
const diffCommitA = ref('');
const diffCommitB = ref('');

const hasAnyChanges = computed(() => {
  if (!status.value) return false;
  return status.value.staged?.length || status.value.changes?.length || status.value.untracked?.length || status.value.conflicted?.length;
});

function statusLetter(s) {
  return { Modified: 'M', Added: 'A', Deleted: 'D', Renamed: 'R', Untracked: '?', Conflicted: 'C', Unmodified: ' ' }[s] || '?';
}
function statusClass(s) {
  return {
    Modified: 'git-status-M',
    Added: 'git-status-A',
    Deleted: 'git-status-D',
    Renamed: 'git-status-R',
    Untracked: 'git-status-Untracked',
    Conflicted: 'git-status-Untracked',
  }[s] || 'git-status-Untracked';
}
function sign(kind) {
  if (kind === 'insert') return '+';
  if (kind === 'delete') return '−';
  return ' ';
}
function lineNo(line) {
  if (line.kind === 'delete') return line.old_no ?? '';
  if (line.kind === 'insert') return line.new_no ?? '';
  return line.new_no ?? '';
}

async function refreshAll() {
  if (!props.visible || !props.activeFile) return;
  try {
    status.value = await invoke('get_git_status');
    branches.value = await invoke('git_branches');
    log.value = await invoke('git_log');
    history.value = await invoke('git_file_history');
    diff.value = [];
    stash.value = await invoke('git_stash_list');
    tags.value = await invoke('git_tags');
    remotes.value = await invoke('git_remotes');
  } catch (e) {
    console.error('refreshAll:', e);
  }
}

async function showFileDiff(file) {
  try {
    const d = await invoke('git_diff_file', { filePath: file.path });
    selectedFileDiff.value = d;
    activeTab.value = 'changes';
  } catch (e) {
    console.error('showFileDiff:', e);
  }
}

async function stage(path) {
  try { await invoke('git_stage_file', { filePath: path }); await refreshAll(); }
  catch (e) { alert('Stage failed: ' + e); }
}
async function unstage(path) {
  try { await invoke('git_unstage_file', { filePath: path }); await refreshAll(); }
  catch (e) { alert('Unstage failed: ' + e); }
}
async function discard(path) {
  if (!confirm(`Discard changes to ${path}?`)) return;
  try { await invoke('git_discard_file', { filePath: path }); await refreshAll(); }
  catch (e) { alert('Discard failed: ' + e); }
}
async function removeUntracked(path) {
  if (!confirm(`Delete untracked file ${path}?`)) return;
  try { await invoke('git_remove_untracked', { filePath: path }); await refreshAll(); }
  catch (e) { alert('Remove failed: ' + e); }
}
async function doCommit() {
  if (!commitMessage.value.trim()) { alert('Enter a commit message'); return; }
  try { await invoke('git_commit', { message: commitMessage.value }); commitMessage.value = ''; await refreshAll(); }
  catch (e) { alert('Commit failed: ' + e); }
}

async function checkoutBranch(name) {
  if (!confirm(`Checkout branch "${name}"?`)) return;
  try { await invoke('git_checkout', { branch: name }); await refreshAll(); }
  catch (e) { alert('Checkout failed: ' + e); }
}
async function deleteBranch(name) {
  if (!confirm(`Delete branch "${name}"?`)) return;
  try { await invoke('git_delete_branch', { name, force: false }); await refreshAll(); }
  catch (e) { alert('Delete branch failed: ' + e); }
}
async function createBranch() {
  if (!newBranchName.value.trim()) { alert('Enter a branch name'); return; }
  try { await invoke('git_create_branch', { name: newBranchName.value }); newBranchName.value = ''; await refreshAll(); }
  catch (e) { alert('Create branch failed: ' + e); }
}

async function checkoutCommit(sha) {
  if (!confirm(`Checkout commit ${sha.substring(0,8)}? This detaches HEAD.`)) return;
  try { await invoke('git_checkout', { branch: sha }); await refreshAll(); }
  catch (e) { alert('Checkout failed: ' + e); }
}
async function showCommitDiff(sha) {
  try { diff.value = await invoke('git_diff_vs_commit', { commit: sha }); activeTab.value = 'diff'; diffMode.value = 'commit'; diffCommit.value = sha; }
  catch (e) { console.error('showCommitDiff:', e); }
}

async function loadDiff() {
  try {
    if (diffMode.value === 'working') {
      diff.value = await invoke('git_diff');
    } else if (diffMode.value === 'commit') {
      if (!diffCommit.value.trim()) { alert('Enter a commit SHA'); return; }
      diff.value = await invoke('git_diff_vs_commit', { commit: diffCommit.value });
    } else if (diffMode.value === 'commits') {
      if (!diffCommitA.value.trim() || !diffCommitB.value.trim()) { alert('Enter both commits'); return; }
      diff.value = await invoke('git_diff_commits', { commitA: diffCommitA.value, commitB: diffCommitB.value });
    }
  } catch (e) { console.error('loadDiff:', e); }
}

async function stashPush() {
  try { await invoke('git_stash_push', { message: stashMessage.value || null }); stashMessage.value = ''; await refreshAll(); }
  catch (e) { alert('Stash push failed: ' + e); }
}
async function stashPop(index) {
  try { await invoke('git_stash_pop', { index }); await refreshAll(); }
  catch (e) { alert('Stash pop failed: ' + e); }
}
async function stashApply(index) {
  try { await invoke('git_stash_apply', { index }); await refreshAll(); }
  catch (e) { alert('Stash apply failed: ' + e); }
}
async function stashDrop(index) {
  if (!confirm(`Drop stash@{${index}}?`)) return;
  try { await invoke('git_stash_drop', { index }); await refreshAll(); }
  catch (e) { alert('Stash drop failed: ' + e); }
}

async function createTag() {
  if (!newTagName.value.trim()) { alert('Enter a tag name'); return; }
  try { await invoke('git_create_tag', { name: newTagName.value, message: newTagMessage.value }); newTagName.value = ''; newTagMessage.value = ''; await refreshAll(); }
  catch (e) { alert('Create tag failed: ' + e); }
}
async function deleteTag(name) {
  if (!confirm(`Delete tag "${name}"?`)) return;
  try { await invoke('git_delete_tag', { name }); await refreshAll(); }
  catch (e) { alert('Delete tag failed: ' + e); }
}

async function addRemote() {
  if (!newRemoteName.value.trim() || !newRemoteUrl.value.trim()) { alert('Enter name and URL'); return; }
  try { await invoke('git_add_remote', { name: newRemoteName.value, url: newRemoteUrl.value }); newRemoteName.value = ''; newRemoteUrl.value = ''; await refreshAll(); }
  catch (e) { alert('Add remote failed: ' + e); }
}
async function fetchRemote(remote) {
  try { await invoke('git_fetch_remote', { remote }); await refreshAll(); }
  catch (e) { alert('Fetch failed: ' + e); }
}
async function pullRemote(remote) {
  const branch = status.value?.branch || 'main';
  try { await invoke('git_pull_from_remote', { remote, branch }); await refreshAll(); }
  catch (e) { alert('Pull failed: ' + e); }
}
async function pushRemote(remote) {
  const branch = status.value?.branch || 'main';
  try { await invoke('git_push_to_remote', { remote, branch, force: false }); await refreshAll(); }
  catch (e) { alert('Push failed: ' + e); }
}

watch(() => [props.visible, props.activeFile], refreshAll, { immediate: true });

onMounted(refreshAll);
</script>
