<template>
  <div id="settings-modal" class="modal-overlay" :class="{ hidden: !open }" @click.self="close">
    <div class="modal-dialog">
      <div class="modal-header">
        <span class="modal-title">Settings</span>
        <button id="settings-close" class="modal-close" @click="close">×</button>
      </div>
      <div class="modal-body">
        <div class="settings-tabs">
          <button class="settings-tab" :class="{ active: activeTab === 'themes' }" @click="activeTab = 'themes'">Themes</button>
          <button class="settings-tab" :class="{ active: activeTab === 'language' }" @click="activeTab = 'language'">Language</button>
          <button class="settings-tab" :class="{ active: activeTab === 'github' }" @click="activeTab = 'github'">GitHub</button>
        </div>

        <!-- Themes -->
        <div id="settings-themes" class="settings-panel" :class="{ hidden: activeTab !== 'themes' }">
          <div class="settings-section-title">Interface Theme</div>
          <div class="theme-grid">
            <div
              v-for="t in interfaceThemes"
              :key="t.id"
              class="theme-card"
              :class="{ selected: settings.theme === t.id }"
              @click="setTheme(t.id)"
            >
              <div class="theme-card-name">{{ t.name }}</div>
              <div class="theme-card-swatches">
                <span v-for="(c, i) in t.swatches" :key="i" class="theme-swatch" :style="{ background: c }"></span>
              </div>
            </div>
          </div>
          <div class="settings-section-title" style="margin-top: 16px;">Code Syntax Palette</div>
          <div class="theme-grid">
            <div
              v-for="t in syntaxThemes"
              :key="t.id"
              class="theme-card"
              :class="{ selected: settings.syntaxTheme === t.id }"
              @click="settings.syntaxTheme = t.id; save()"
            >
              <div class="theme-card-name">{{ t.name }}</div>
              <div class="theme-card-swatches">
                <span v-for="(c, i) in t.swatches" :key="i" class="theme-swatch" :style="{ background: c }"></span>
              </div>
            </div>
          </div>
          <div class="settings-section-title" style="margin-top: 16px;">Preview</div>
          <div id="theme-preview" class="theme-preview">
            <div class="pv-heading">Heading</div>
            <div>Text with <span class="pv-emphasis">emphasis</span>, <span class="pv-code">code</span>, and a <a class="pv-link" href="#">link</a>.</div>
            <div class="pv-quote">A block quote example.</div>
            <div class="pv-syntax">
              <span class="syn-kw">fn</span> <span class="syn-fn">main</span>() &#123;<br>
              &nbsp;&nbsp;<span class="syn-cmt">// Hello</span><br>
              &nbsp;&nbsp;<span class="syn-var">name</span> = <span class="syn-str">"world"</span>;<br>
              &#125;
            </div>
          </div>
        </div>

        <!-- Language -->
        <div id="settings-language" class="settings-panel" :class="{ hidden: activeTab !== 'language' }">
          <div class="settings-section-title">Interface Language</div>
          <div class="language-grid">
            <div
              v-for="l in languages"
              :key="l.id"
              class="lang-card"
              :class="{ selected: settings.language === l.id }"
              @click="setLanguage(l.id)"
            >
              <span class="lang-card-name">{{ l.name }}</span>
            </div>
          </div>
        </div>

        <!-- GitHub -->
        <div id="settings-github" class="settings-panel" :class="{ hidden: activeTab !== 'github' }">
          <div class="settings-section-title">GitHub Authentication</div>
          <div id="github-auth-status" class="github-auth-status">
            <div v-if="ghAuth.authenticated" class="gh-status-line">
              <span class="gh-status-icon">v</span>
              <span>Logged in as <span class="gh-user">{{ ghAuth.user }}</span></span>
            </div>
            <div v-else class="gh-status-line">
              <span class="gh-status-icon">x</span>
              <span>Not authenticated</span>
            </div>
          </div>
          <div class="github-actions">
            <button id="btn-gh-login" class="git-action-btn" @click="ghLogin">Login with gh CLI</button>
            <button id="btn-gh-refresh" class="git-action-btn" @click="refreshGh">Refresh status</button>
            <button id="btn-gh-logout" class="git-action-btn danger" @click="ghLogout">Logout</button>
          </div>

          <div class="settings-section-title" style="margin-top: 16px;">Repository</div>
          <div id="github-repo-info" class="github-repo-info">
            <div v-if="ghRepo.full_name" class="gh-repo-name">{{ ghRepo.full_name }}</div>
            <div v-if="ghRepo.html_url" class="gh-repo-url">{{ ghRepo.html_url }}</div>
            <div v-if="ghRepo.default_branch" class="gh-repo-branch">Default: {{ ghRepo.default_branch }}</div>
            <div v-else class="gh-status-line">No repository info</div>
          </div>
          <div class="github-actions">
            <button id="btn-gh-prs" class="git-action-btn" @click="loadPrs">Load Pull Requests</button>
            <button id="btn-gh-branches" class="git-action-btn" @click="loadRemoteBranches">Load Remote Branches</button>
          </div>

          <div id="github-prs" class="github-prs">
            <div v-for="pr in ghPrs" :key="pr.number" class="github-pr-entry">
              <span class="pr-number">#{{ pr.number }}</span>
              <span class="pr-title">{{ pr.title }}</span>
              <span class="pr-state" :class="pr.state">{{ pr.state }}</span>
            </div>
            <div v-if="!ghPrs.length" class="file-tree-empty">No PRs loaded</div>
          </div>
          <div id="github-branches" class="github-branches">
            <div v-for="b in ghBranches" :key="b.name" class="github-branch-entry">
              <span class="br-name">{{ b.name }}</span>
            </div>
            <div v-if="!ghBranches.length" class="file-tree-empty">No branches loaded</div>
          </div>
        </div>
      </div>
    </div>
  </div>
</template>

<script setup>
import { ref, watch, onMounted } from 'vue';
import { invoke } from '@tauri-apps/api/core';
import { fetchGitHubStatus } from '../lib/githubStatus.js';

const props = defineProps({
  open: Boolean,
});
const emit = defineEmits(['update:open', 'theme']);

const SETTINGS_KEY = 'womd_settings';

const interfaceThemes = [
  { id: 'mocha', name: 'Catppuccin Mocha', swatches: ['#1e1e2e', '#cdd6f4', '#89b4fa', '#f9e2af'] },
  { id: 'latte', name: 'Catppuccin Latte', swatches: ['#eff1f5', '#4c4f69', '#1e66f5', '#df8e1d'] },
  { id: 'monokai', name: 'Monokai', swatches: ['#272822', '#f8f8f2', '#66d9ef', '#a6e22e'] },
  { id: 'solarized-dark', name: 'Solarized Dark', swatches: ['#002b36', '#93a1a1', '#268bd2', '#b58900'] },
  { id: 'github-dark', name: 'GitHub Dark', swatches: ['#0d1117', '#c9d1d9', '#58a6ff', '#d29922'] },
  { id: 'dracula', name: 'Dracula', swatches: ['#282a36', '#f8f8f2', '#bd93f9', '#50fa7b'] },
  { id: 'one-dark', name: 'One Dark', swatches: ['#282c34', '#abb2bf', '#61afef', '#e5c07b'] },
];

const syntaxThemes = interfaceThemes;

const languages = [
  { id: 'en', name: 'English' },
  { id: 'ar', name: 'العربية' },
];

const activeTab = ref('themes');
const settings = ref({
  theme: 'mocha',
  syntaxTheme: 'mocha',
  language: 'en',
});

const ghAuth = ref({ authenticated: false, user: '' });
const ghRepo = ref({ full_name: '', default_branch: '', html_url: '' });
const ghPrs = ref([]);
const ghBranches = ref([]);

function loadSettings() {
  try {
    const raw = window.localStorage.getItem(SETTINGS_KEY);
    if (raw) {
      settings.value = { ...settings.value, ...JSON.parse(raw) };
    }
  } catch (e) { console.error('loadSettings:', e); }
}

function save() {
  try {
    window.localStorage.setItem(SETTINGS_KEY, JSON.stringify(settings.value));
  } catch (e) { console.error('saveSettings:', e); }
  applyTheme();
}

function applyTheme() {
  const app = document.getElementById('app');
  if (app) {
    app.setAttribute('data-theme', settings.value.theme);
  }
  document.documentElement.setAttribute('data-theme', settings.value.theme);
  document.documentElement.lang = settings.value.language;
  document.documentElement.dir = settings.value.language === 'ar' ? 'rtl' : 'ltr';
  emit('theme', settings.value.theme);
}

function setTheme(id) {
  settings.value.theme = id;
  save();
}

function setLanguage(id) {
  settings.value.language = id;
  save();
}

function close() {
  emit('update:open', false);
}

async function refreshGh() {
  // Auth and repo metadata are independent calls — `repo view` legitimately
  // fails when the open file is not inside a GitHub repo, and that failure
  // must not report the user as logged out (see lib/githubStatus.js).
  const { auth, repo } = await fetchGitHubStatus(invoke);
  ghAuth.value = auth;
  ghRepo.value = repo;
}

async function ghLogin() {
  try {
    await invoke('github_login');
    await refreshGh();
  } catch (e) {
    alert('GitHub login: ' + e);
  }
}

async function ghLogout() {
  try {
    await invoke('github_logout');
    await refreshGh();
  } catch (e) {
    console.error('ghLogout:', e);
  }
}

async function loadPrs() {
  try {
    ghPrs.value = await invoke('github_pull_requests');
  } catch (e) {
    console.error('loadPrs:', e);
    ghPrs.value = [];
  }
}

async function loadRemoteBranches() {
  try {
    ghBranches.value = await invoke('github_remote_branches');
  } catch (e) {
    console.error('loadRemoteBranches:', e);
    ghBranches.value = [];
  }
}

watch(() => props.open, (v) => { if (v) { refreshGh(); activeTab.value = 'themes'; } });

onMounted(() => {
  loadSettings();
  applyTheme();
  if (props.open) refreshGh();
});
</script>
