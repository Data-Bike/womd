//! Path sandbox: confines every filesystem operation to a configured root
//! directory and refuses well-known sensitive locations (ADR-007).
//!
//! Two layers:
//!
//! * **Root validation** (`Workspace::new`) — the configured root must be an
//!   existing directory and must NOT be (or contain) a protected location:
//!   filesystem/drive roots, the user's home directory itself, Desktop and
//!   other user-profile top folders, OS system directories, credential/config
//!   dot-directories. Nested folders of an approved root remain fully
//!   accessible — the denial applies to *choosing* a protected directory as the
//!   workspace root or to being *inside* a system subtree.
//! * **Per-request resolution** (`Workspace::resolve`) — relative paths only;
//!   `..` can never climb above the root; existing paths are canonicalized so
//!   symlinks cannot escape; for new files the nearest existing ancestor is
//!   canonicalized instead. Repository internals (`.git`) and WoMD atomic-save
//!   temp files are never exposed.

use std::env;
use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};

/// Errors produced by sandbox checks. `Display` text is safe to return to the
/// model — it never contains credentials or unexplored host paths beyond the
/// offending input.
#[derive(Debug)]
pub enum SandboxError {
    /// Path escapes the workspace root (`..` above root, absolute path, or a
    /// symlink whose target is outside).
    OutsideRoot,
    /// Path is a denied location (system directory, credential dir, `.git`, …).
    Denied(String),
    /// The workspace root itself failed validation.
    InvalidRoot(String),
    /// Path does not exist where existence was required.
    NotFound(String),
    /// Generic I/O failure.
    Io(String),
}

impl std::fmt::Display for SandboxError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::OutsideRoot => write!(f, "path escapes the workspace root"),
            Self::Denied(s) => write!(f, "access denied: {s}"),
            Self::InvalidRoot(s) => write!(f, "invalid workspace root: {s}"),
            Self::NotFound(s) => write!(f, "not found: {s}"),
            Self::Io(s) => write!(f, "io error: {s}"),
        }
    }
}

impl std::error::Error for SandboxError {}

pub type SandboxResult<T> = Result<T, SandboxError>;

/// Canonicalize a path and strip the Windows `\\?\` verbatim prefix so results
/// compare cleanly with non-verbatim paths from environment variables.
fn canon(path: &Path) -> SandboxResult<PathBuf> {
    let c = path
        .canonicalize()
        .map_err(|e| SandboxError::Io(format!("{}: {e}", path.display())))?;
    Ok(strip_verbatim(&c))
}

pub(crate) fn strip_verbatim(p: &Path) -> PathBuf {
    let s = p.to_string_lossy();
    if let Some(rest) = s.strip_prefix(r"\\?\") {
        PathBuf::from(rest)
    } else {
        p.to_path_buf()
    }
}

fn home_dir() -> Option<PathBuf> {
    env::home_dir().map(|h| strip_verbatim(&h))
}

/// A directory the root may not *be* exactly (its subdirectories stay usable).
fn denied_exact() -> Vec<PathBuf> {
    let mut v = Vec::new();
    if let Some(h) = home_dir() {
        v.push(h.clone());
        for name in [
            "Desktop",
            "Documents",
            "Downloads",
            "Pictures",
            "Music",
            "Videos",
        ] {
            v.push(h.join(name));
        }
    }
    #[cfg(target_os = "macos")]
    v.push(PathBuf::from("/Volumes"));
    v
}

/// A directory the root may not be inside of or equal to. Anything mounted in
/// `allowed_exceptions` wins over this list (e.g. temp dirs under `/var`).
fn denied_subtree() -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = Vec::new();
    let unix = [
        "/etc", "/usr", "/bin", "/sbin", "/lib", "/lib64", "/lib32", "/boot", "/proc", "/sys",
        "/dev", "/run", "/var", "/opt", "/snap", "/root",
    ];
    for d in unix {
        v.push(PathBuf::from(d));
    }
    #[cfg(target_os = "macos")]
    {
        for d in [
            "/System",
            "/Library",
            "/private",
            "/Applications",
            "/cores",
            "/Network",
        ] {
            v.push(PathBuf::from(d));
        }
    }
    #[cfg(target_os = "windows")]
    {
        let sys_root = env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into());
        v.push(PathBuf::from(sys_root));
        for d in [
            r"C:\Program Files",
            r"C:\Program Files (x86)",
            r"C:\ProgramData",
            r"C:\$Recycle.Bin",
            r"C:\Users\Public",
        ] {
            v.push(PathBuf::from(d));
        }
    }
    if let Some(h) = home_dir() {
        // Credential / agent-config locations: an AI must never operate in these.
        for d in [
            ".ssh",
            ".aws",
            ".azure",
            ".gnupg",
            ".kube",
            ".docker",
            ".config",
            ".local",
            ".cache",
            ".cargo",
            ".npm",
            ".gitconfig.d",
        ] {
            v.push(h.join(d));
        }
        #[cfg(target_os = "windows")]
        v.push(h.join("AppData"));
        #[cfg(target_os = "macos")]
        v.push(h.join("Library"));
    }
    v
}

/// Paths that are allowed even though they sit under a denied subtree
/// (ephemeral temp space — used by tests and by headless MCP invocations).
fn allowed_exceptions() -> Vec<PathBuf> {
    let mut v = vec![
        PathBuf::from("/tmp"),
        PathBuf::from("/var/tmp"),
        PathBuf::from("/private/tmp"),
        PathBuf::from("/private/var/tmp"),
        PathBuf::from("/var/folders"),
        PathBuf::from("/private/var/folders"),
        PathBuf::from("/dev/shm"),
    ];
    for key in ["TMPDIR", "TEMP", "TMP"] {
        if let Ok(d) = env::var(key) {
            v.push(PathBuf::from(d));
        }
    }
    if let Ok(d) = env::var("CARGO_TARGET_TMPDIR") {
        v.push(PathBuf::from(d));
    }
    v
}

fn canon_or_raw(p: &Path) -> PathBuf {
    canon(p).unwrap_or_else(|_| strip_verbatim(p))
}

/// A sandboxed workspace rooted at a user-chosen directory.
#[derive(Debug, Clone)]
pub struct Workspace {
    root: PathBuf,
}

impl Workspace {
    /// Validate `root` against the security policy and return the sandbox.
    pub fn new(root: impl AsRef<Path>) -> SandboxResult<Self> {
        let raw = root.as_ref();
        if !raw.exists() {
            return Err(SandboxError::InvalidRoot(format!(
                "{} does not exist",
                raw.display()
            )));
        }
        if !raw.is_dir() {
            return Err(SandboxError::InvalidRoot(format!(
                "{} is not a directory",
                raw.display()
            )));
        }
        let root = canon(raw).map_err(|e| match e {
            SandboxError::Io(s) => SandboxError::InvalidRoot(s),
            other => other,
        })?;
        Self::check_root(&root)?;
        Ok(Self { root })
    }

    /// The policy check, separated so tests can exercise it directly.
    fn check_root(root: &Path) -> SandboxResult<()> {
        // Filesystem / drive roots are never acceptable.
        if root.parent().is_none() {
            return Err(SandboxError::InvalidRoot(
                "filesystem root is not an allowed workspace".into(),
            ));
        }
        // The root must not be an ancestor of the home directory (that would
        // expose every user folder, e.g. C:\Users or /home).
        if let Some(h) = home_dir() {
            let h = canon_or_raw(&h);
            if h.starts_with(root) {
                return Err(SandboxError::InvalidRoot(
                    "workspace may not contain the user's home directory".into(),
                ));
            }
        }
        let exceptions: Vec<PathBuf> = allowed_exceptions()
            .iter()
            .map(|p| canon_or_raw(p))
            .collect();
        let excepted = exceptions.iter().any(|e| root.starts_with(e));
        if !excepted {
            for d in denied_subtree() {
                let d = canon_or_raw(&d);
                if root == d || root.starts_with(&d) {
                    return Err(SandboxError::InvalidRoot(format!(
                        "{} is a protected system location",
                        d.display()
                    )));
                }
            }
        }
        for d in denied_exact() {
            let d = canon_or_raw(&d);
            if root == d {
                return Err(SandboxError::InvalidRoot(format!(
                    "{} must not be used as a workspace root; pick a subfolder",
                    d.display()
                )));
            }
        }
        // Per-component read policy: a root inside .git, ~/.ssh-style
        // credential dirs (anywhere, not only under home), a device name or
        // an otherwise denied name must be rejected — the component checks
        // in resolve only apply to paths *relative* to the root.
        for c in root.components() {
            let Component::Normal(name) = c else { continue };
            let s = name.to_string_lossy();
            if let Some(r) = Self::denied_component_reason(&s, false) {
                return Err(SandboxError::InvalidRoot(format!(
                    "{} contains a protected component '{s}': {r}",
                    root.display()
                )));
            }
        }
        // macOS: a mounted volume root (/Volumes/X) is equivalent to a drive
        // root — expose only subfolders of it.
        #[cfg(target_os = "macos")]
        if root.parent() == Some(Path::new("/Volumes")) {
            return Err(SandboxError::InvalidRoot(
                "a mounted volume root is not an allowed workspace; pick a subfolder".into(),
            ));
        }
        Ok(())
    }

    /// The canonical workspace root.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Normalize a relative path into lexical components; `..` may not pop
    /// above the root.
    fn normalize_rel(rel: &Path) -> SandboxResult<Vec<OsString>> {
        let mut out: Vec<OsString> = Vec::new();
        for c in rel.components() {
            match c {
                Component::Normal(s) => out.push(s.to_os_string()),
                Component::CurDir => {}
                Component::ParentDir => {
                    if out.pop().is_none() {
                        return Err(SandboxError::OutsideRoot);
                    }
                }
                // Prefix (Windows drive/UNC) or RootDir — absolute paths are
                // not accepted; everything is expressed relative to the root.
                Component::Prefix(_) | Component::RootDir => return Err(SandboxError::OutsideRoot),
            }
        }
        Ok(out)
    }

    /// File/directory names the AI may neither read nor write (secrets,
    /// OS integration, repository internals). Checked case-insensitively.
    fn is_denied_read_name(name: &str) -> Option<&'static str> {
        let lower = name.to_lowercase();
        if lower == ".git" {
            return Some("repository internals (.git) are not accessible");
        }
        // Other VCS internals: a nested .svn/.hg/.bzr checkout (or a vendored
        // copy) carries the same working-copy metadata and hook machinery.
        // `.terraform` caches downloaded provider *binaries* and can hold
        // sensitive plan/state artifacts.
        if matches!(lower.as_str(), ".svn" | ".hg" | ".bzr" | ".terraform") {
            return Some("version-control/tooling internals are not accessible");
        }
        // `.cargo` can hold credentials.toml (registry auth tokens) — same
        // class as `.docker`/`.config` below.
        if lower == ".cargo" {
            return Some("credential/configuration directory");
        }
        if lower.starts_with(".womd-tmp-") {
            return Some("editor temp files are not accessible");
        }
        // Windows device names apply to the stem even with an extension
        // ("NUL.txt" still hits the device).
        let stem = lower.split('.').next().unwrap_or("");
        if matches!(stem, "con" | "prn" | "aux" | "nul")
            || ((stem.starts_with("com") || stem.starts_with("lpt"))
                && (stem[3..].chars().all(|c| c.is_ascii_digit()) && stem.len() == 4
                    // NTFS quirk: superscript digits map to COM/LPT devices too
                    // ("COM¹" opens COM1).
                    || (!stem[3..].is_empty()
                        && stem[3..].chars().all(|c| matches!(c, '¹' | '²' | '³')))))
        {
            return Some("reserved device name");
        }
        // Credential / agent-runtime directories anywhere in the workspace.
        if matches!(
            lower.as_str(),
            ".ssh"
                | ".aws"
                | ".azure"
                | ".gnupg"
                | ".kube"
                | ".docker"
                | ".config"
                | ".gcloud"
                | ".secrets"
                | ".credentials"
        ) {
            return Some("credential/configuration directory");
        }
        if Self::is_credential_file(&lower) {
            return Some("credential or secret file");
        }
        None
    }

    /// Secret-bearing file names: private keys, token stores, `.env`.
    fn is_credential_file(lower: &str) -> bool {
        // `.env` and `.env.*` except the documentation-style examples.
        if lower == ".env"
            || (lower.starts_with(".env.")
                && !matches!(
                    lower,
                    ".env.example" | ".env.sample" | ".env.template" | ".env.dist"
                ))
        {
            return true;
        }
        if matches!(
            lower,
            ".netrc"
                | ".npmrc"
                | ".pypirc"
                | ".git-credentials"
                | ".gitconfig"
                | ".pgpass"
                | ".my.cnf"
                | ".kubeconfig"
                | "kubeconfig"
                // direnv: a shell script executed on `cd` that almost always
                // carries exported secrets — read AND write are denied.
                | ".envrc"
                // Web-server credential stores and package-manager configs
                // that can carry auth tokens (npm/yarn scopes, registries).
                | ".htpasswd" | ".htdigest" | ".yarnrc" | ".yarnrc.yml"
        ) {
            return true;
        }
        for key in ["id_rsa", "id_dsa", "id_ecdsa", "id_ed25519"] {
            if lower.starts_with(key) {
                return true;
            }
        }
        // `*.tfvars.json` — the rsplit('.') check below only sees `json`.
        if lower.ends_with(".tfvars.json") || lower.ends_with(".tfplan") {
            return true;
        }
        matches!(
            lower.rsplit('.').next().unwrap_or(""),
            // Key/cert stores + PuTTY/OpenVPN formats (embedded private keys)
            // + Terraform state/vars (plaintext secrets live there verbatim).
            "pem"
                | "key"
                | "pfx"
                | "p12"
                | "keystore"
                | "jks"
                | "kdbx"
                | "kdb"
                | "ppk"
                | "ovpn"
                | "tfstate"
                | "tfvars"
        )
    }

    /// Extensions that make a file directly executable/installable on a
    /// common desktop OS (double-click or shell-run). Unix scripts without
    /// the +x bit are inert, but files the OS treats as programs are denied
    /// outright: MCP must never be able to plant executable code.
    fn is_executable_name(lower: &str) -> bool {
        matches!(
            lower.rsplit('.').next().unwrap_or(""),
            "exe"
                | "dll"
                | "com"
                | "scr"
                | "pif"
                | "msi"
                | "msp"
                | "mst"
                | "bat"
                | "cmd"
                | "ps1"
                | "psd1"
                | "psm1"
                | "vbs"
                | "vbe"
                | "js"
                | "jse"
                | "mjs"
                | "cjs"
                | "wsf"
                | "wsh"
                | "hta"
                | "jar"
                | "gadget"
                | "msc"
                | "cpl"
                | "ocx"
                | "sys"
                | "drv"
                | "reg"
                | "lnk"
                | "url"
                | "inf"
                | "ins"
                | "isp"
                | "sct"
                | "shb"
                | "app"
                | "action"
                | "workflow"
                | "command"
                | "dmg"
                | "pkg"
                | "deb"
                | "rpm"
                | "apk"
                | "ipa"
                | "run"
                // Interpreter scripts — every extension above is launched by
                // double-click or a shell directly; these run through an
                // installed interpreter (`sh x.sh`, `python x.py`). Same
                // policy: MCP must not plant runnable code, and `.js` was
                // already denied on exactly this basis.
                | "sh" | "bash" | "zsh" | "fish" | "ksh"
                | "py" | "pyw" | "pyc" | "pyo"
                | "rb" | "pl" | "pm" | "php" | "lua" | "tcl"
                | "scpt" | "applescript" | "osascript"
        )
    }

    /// Names whose content is interpreted by tools the user runs — build
    /// scripts, package manifests with install hooks, CI definitions, OS
    /// folder metadata. Writing these would let MCP change how *other*
    /// programs behave (run a postinstall script, a CI job, a build.rs).
    fn is_config_affecting_name(lower: &str) -> bool {
        matches!(
            lower,
            "build.rs" | "makefile" | "gnumakefile" | "cmakelists.txt"
                | "package.json" | "cargo.toml" | "setup.py" | "pyproject.toml"
                | "pom.xml" | "build.gradle" | "build.gradle.kts" | "settings.gradle"
                | "gemfile" | "rakefile" | "pipfile" | "vagrantfile"
                | "dockerfile" | "docker-compose.yml" | "docker-compose.yaml"
                | "compose.yml" | "compose.yaml"
                | ".gitlab-ci.yml" | ".travis.yml" | "appveyor.yml"
                | "azure-pipelines.yml" | "jenkinsfile" | "bitrise.yml"
                | "circle.yml" | ".drone.yml" | "codefresh.yml"
                | "desktop.ini" | "thumbs.db" | ".ds_store" | "autorun.inf"
                // Git-side configuration: `.gitattributes` can wire files to
                // external filter programs (clean/smudge run during
                // add/checkout); `.gitmodules` steers submodule fetches.
                | ".gitattributes" | ".gitmodules"
                // `.gitignore` doesn't run code, but it steers what `git`
                // sees — writing it could hide planted files from status and
                // commits (and from the per-mutation commit the API promises).
                | ".gitignore"
                // Git-hook managers: their config *is* executed by git on
                // commit/push. husky stores scripts under `.husky/` (dir is
                // write-denied below); lefthook/pre-commit read these files.
                | "lefthook.yml" | "lefthook.yaml" | ".lefthook.yml"
                | ".pre-commit-config.yaml" | ".huskyrc" | ".huskyrc.json"
        )
    }

    /// Directory names whose contents configure other apps / agents —
    /// writing into them changes how editors and AI tools behave
    /// (`.claude` hooks can execute commands, `.vscode` tasks run on demand).
    fn is_write_denied_dir(lower: &str) -> bool {
        matches!(
            lower,
            ".vscode"
                | ".idea"
                | ".vs"
                | ".fleet"
                | ".zed"
                | ".cursor"
                | ".claude"
                | ".devin"
                | ".agents"
                | ".windsurf"
                | ".codeium"
                | ".aider"
                | ".continue"
                // `.husky` — git hook scripts executed on commit/push.
                // `.devcontainer` — devcontainer.json postCreateCommand runs
                // arbitrary commands when the folder opens in an IDE.
                // (`.cargo` is read-denied entirely — it can hold tokens.)
                | ".husky" | ".devcontainer"
        )
    }

    /// One component fails hygiene or policy. Returns the denial reason.
    fn denied_component_reason(name: &str, for_write: bool) -> Option<String> {
        // Name-level hygiene first: characters Windows/forbidden in portable
        // names, and trailing '.' / ' ' which NTFS silently strips — a
        // component ".git " would otherwise bypass the ".git" check and still
        // resolve into ".git".
        if name.is_empty()
            || name.ends_with('.')
            || name.ends_with(' ')
            || name
                .chars()
                .any(|c| c.is_control() || matches!(c, '<' | '>' | ':' | '"' | '|' | '?' | '*'))
        {
            return Some("illegal file name".into());
        }
        let lower = name.to_lowercase();
        if let Some(r) = Self::is_denied_read_name(&lower) {
            return Some(r.into());
        }
        if for_write {
            if Self::is_executable_name(&lower) {
                return Some("executable files cannot be created or modified through MCP".into());
            }
            if Self::is_config_affecting_name(&lower) {
                return Some(
                    "files that configure builds/apps/CI cannot be modified through MCP".into(),
                );
            }
            if Self::is_write_denied_dir(&lower) {
                return Some(
                    "editor/agent configuration directories are read-only through MCP".into(),
                );
            }
        }
        None
    }

    fn check_components(comps: &[OsString], for_write: bool) -> SandboxResult<()> {
        for (i, c) in comps.iter().enumerate() {
            let s = c.to_string_lossy();
            if let Some(r) = Self::denied_component_reason(&s, for_write) {
                return Err(SandboxError::Denied(format!("'{s}': {r}")));
            }
            // `.github/workflows/*` — CI definitions; `.github/CONTRIBUTING.md`
            // stays writable, only the workflows subtree is off-limits.
            if for_write
                && s.eq_ignore_ascii_case(".github")
                && comps
                    .get(i + 1)
                    .map(|n| n.to_string_lossy().eq_ignore_ascii_case("workflows"))
                    .unwrap_or(false)
            {
                return Err(SandboxError::Denied(
                    "CI workflow definitions are read-only through MCP".into(),
                ));
            }
        }
        Ok(())
    }

    /// Resolve a client-supplied relative path to an absolute path guaranteed
    /// to be inside the workspace (or, for not-yet-existing paths, under a
    /// canonicalized ancestor inside the workspace). `for_write` additionally
    /// applies the write policy (no executables, no app/build/CI config).
    fn resolve_inner(&self, rel: &Path, for_write: bool) -> SandboxResult<PathBuf> {
        let comps = Self::normalize_rel(rel)?;
        Self::check_components(&comps, for_write)?;
        let mut candidate = self.root.clone();
        for c in &comps {
            candidate.push(c);
        }
        match candidate.symlink_metadata() {
            Ok(md) => {
                if md.file_type().is_symlink() && candidate.canonicalize().is_err() {
                    // A dangling symlink as a write target would materialize
                    // its target — possibly outside the root.
                    return Err(SandboxError::Denied("dangling symlink".into()));
                }
                let c = canon(&candidate)?;
                if c != self.root && !c.starts_with(&self.root) {
                    return Err(SandboxError::OutsideRoot);
                }
                // Re-check the canonical tail: NTFS 8.3 aliases ("GIT~1"),
                // odd-cased names and symlinked components can all resolve to
                // a denied real name that the raw-component check never saw.
                let canon_comps: Vec<OsString> = c
                    .strip_prefix(&self.root)
                    .map(|p| {
                        p.components()
                            .map(|x| x.as_os_str().to_os_string())
                            .collect()
                    })
                    .unwrap_or_default();
                Self::check_components(&canon_comps, for_write)?;
                Ok(c)
            }
            Err(_) => {
                // Walk up to the nearest existing ancestor; canonicalize it so
                // a symlinked directory cannot smuggle writes outside.
                let mut ancestors: Vec<&Path> = candidate.ancestors().collect();
                ancestors.reverse(); // root ..= candidate
                let mut canon_ancestor = None;
                for (idx, p) in ancestors.iter().enumerate().rev() {
                    match p.canonicalize() {
                        Ok(c) => {
                            canon_ancestor = Some((strip_verbatim(&c), idx));
                            break;
                        }
                        Err(_) => continue,
                    }
                }
                let (canon_ancestor, idx) = canon_ancestor
                    .ok_or_else(|| SandboxError::Io("no existing ancestor".into()))?;
                if canon_ancestor != self.root && !canon_ancestor.starts_with(&self.root) {
                    return Err(SandboxError::OutsideRoot);
                }
                // Same canonical-tail re-check for the ancestor prefix.
                let canon_comps: Vec<OsString> = canon_ancestor
                    .strip_prefix(&self.root)
                    .map(|p| {
                        p.components()
                            .map(|x| x.as_os_str().to_os_string())
                            .collect()
                    })
                    .unwrap_or_default();
                Self::check_components(&canon_comps, for_write)?;
                let mut out = canon_ancestor;
                for p in &ancestors[idx + 1..] {
                    if let Some(name) = p.file_name() {
                        out.push(name);
                    }
                }
                Ok(out)
            }
        }
    }

    /// Resolve a relative path for reading.
    pub fn resolve(&self, rel: impl AsRef<Path>) -> SandboxResult<PathBuf> {
        self.resolve_inner(rel.as_ref(), false)
    }

    /// Resolve a relative path for writing/creating/deleting — the full
    /// write policy applies on top of the read policy.
    pub fn resolve_write(&self, rel: impl AsRef<Path>) -> SandboxResult<PathBuf> {
        self.resolve_inner(rel.as_ref(), true)
    }

    /// Resolve and require the path to exist and be a file.
    pub fn resolve_file(&self, rel: impl AsRef<Path>) -> SandboxResult<PathBuf> {
        let p = self.resolve(rel.as_ref())?;
        if !p.is_file() {
            return Err(SandboxError::NotFound(rel.as_ref().display().to_string()));
        }
        Ok(p)
    }

    /// `resolve_file` under the write policy (read-modify-write tools).
    pub fn resolve_file_write(&self, rel: impl AsRef<Path>) -> SandboxResult<PathBuf> {
        let p = self.resolve_write(rel.as_ref())?;
        if !p.is_file() {
            return Err(SandboxError::NotFound(rel.as_ref().display().to_string()));
        }
        Ok(p)
    }

    /// Resolve and require the path to exist and be a directory.
    pub fn resolve_dir(&self, rel: impl AsRef<Path>) -> SandboxResult<PathBuf> {
        let p = self.resolve(rel.as_ref())?;
        if !p.is_dir() {
            return Err(SandboxError::NotFound(rel.as_ref().display().to_string()));
        }
        Ok(p)
    }

    /// True when `name` (one path component) is denied even for reading —
    /// used by directory traversal so denied entries never surface in
    /// listings or search results.
    pub fn name_denied_for_read(name: &str) -> bool {
        Self::is_denied_read_name(&name.to_lowercase()).is_some()
    }

    /// True when `name` (one path component) is denied for writes —
    /// executable types, config-affecting names, denied dirs.
    pub fn name_denied_for_write(name: &str) -> bool {
        Self::denied_component_reason(&name.to_lowercase(), true).is_some()
    }

    /// Path relative to the root, for display back to the model.
    pub fn display_rel(&self, abs: &Path) -> String {
        abs.strip_prefix(&self.root)
            .map(|p| {
                if p.as_os_str().is_empty() {
                    ".".into()
                } else {
                    p.display().to_string()
                }
            })
            .unwrap_or_else(|_| abs.display().to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_workspace() -> (tempfile::TempDir, Workspace) {
        let dir = tempfile::tempdir().unwrap();
        let ws = Workspace::new(dir.path()).expect("tempdir must be a valid root");
        (dir, ws)
    }

    #[test]
    fn tempdir_is_valid_root() {
        let (_d, ws) = tmp_workspace();
        assert!(ws.root().is_absolute());
    }

    #[test]
    fn filesystem_root_denied() {
        let fs_root = if cfg!(windows) { r"C:\" } else { "/" };
        assert!(matches!(
            Workspace::new(fs_root),
            Err(SandboxError::InvalidRoot(_))
        ));
    }

    #[test]
    fn home_dir_denied_as_root() {
        if let Some(h) = home_dir() {
            assert!(matches!(
                Workspace::new(&h),
                Err(SandboxError::InvalidRoot(_))
            ));
        }
    }

    #[test]
    fn desktop_denied_as_root() {
        if let Some(h) = home_dir() {
            let d = h.join("Desktop");
            if d.is_dir() {
                assert!(matches!(
                    Workspace::new(&d),
                    Err(SandboxError::InvalidRoot(_))
                ));
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn system_dirs_denied() {
        for d in ["/etc", "/usr", "/var", "/bin"] {
            if Path::new(d).is_dir() {
                assert!(
                    matches!(Workspace::new(d), Err(SandboxError::InvalidRoot(_))),
                    "{d} should be denied"
                );
            }
        }
        // A nested folder inside a denied subtree is denied too…
        assert!(matches!(
            Workspace::new("/etc/ssh"),
            Err(SandboxError::InvalidRoot(_))
        ));
        // …but temp exceptions remain usable.
        assert!(Workspace::new("/tmp").is_ok());
    }

    #[test]
    fn ancestor_of_home_denied() {
        // e.g. C:\Users, /home, /Users — must not be selectable since they
        // would expose the whole home directory.
        if let Some(h) = home_dir() {
            if let Some(parent) = h.parent() {
                if parent.is_dir() {
                    assert!(matches!(
                        Workspace::new(parent),
                        Err(SandboxError::InvalidRoot(_))
                    ));
                }
            }
        }
    }

    #[test]
    fn nested_folders_are_accessible() {
        let (dir, ws) = tmp_workspace();
        let nested = dir.path().join("a/b/c");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(nested.join("doc.md"), b"# hi").unwrap();
        let p = ws.resolve_file("a/b/c/doc.md").unwrap();
        assert!(p.ends_with("doc.md"));
    }

    #[test]
    fn parent_traversal_rejected() {
        let (_d, ws) = tmp_workspace();
        assert!(matches!(
            ws.resolve("../outside.txt"),
            Err(SandboxError::OutsideRoot)
        ));
        assert!(matches!(
            ws.resolve("a/../../outside.txt"),
            Err(SandboxError::OutsideRoot)
        ));
    }

    #[test]
    fn absolute_paths_rejected() {
        let (_d, ws) = tmp_workspace();
        let abs = if cfg!(windows) {
            r"C:\x\y"
        } else {
            "/etc/passwd"
        };
        assert!(matches!(ws.resolve(abs), Err(SandboxError::OutsideRoot)));
    }

    #[cfg(unix)]
    #[test]
    fn symlink_escape_rejected() {
        let (dir, ws) = tmp_workspace();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret.txt"), b"x").unwrap();
        std::os::unix::fs::symlink(outside.path(), dir.path().join("link")).unwrap();
        assert!(matches!(
            ws.resolve("link/secret.txt"),
            Err(SandboxError::OutsideRoot)
        ));
    }

    #[test]
    fn git_internals_denied() {
        let (dir, ws) = tmp_workspace();
        std::fs::create_dir_all(dir.path().join(".git")).unwrap();
        assert!(matches!(
            ws.resolve(".git/config"),
            Err(SandboxError::Denied(_))
        ));
    }

    #[test]
    fn nonexistent_nested_write_resolves() {
        let (_d, ws) = tmp_workspace();
        let p = ws.resolve("new/dir/file.md").unwrap();
        assert!(p.starts_with(ws.root()));
        assert!(p.ends_with("file.md"));
    }

    // ------------------------------------------------------------------
    // Name policy (API 3.0.0)
    // ------------------------------------------------------------------

    #[test]
    fn executables_are_write_denied_but_readable() {
        let (dir, ws) = tmp_workspace();
        std::fs::write(dir.path().join("tool.exe"), b"MZ").unwrap();
        std::fs::write(dir.path().join("run.ps1"), b"x").unwrap();
        // Reads of existing files stay possible…
        assert!(ws.resolve("tool.exe").is_ok());
        // …but no executable can be created or modified through MCP.
        for name in [
            "new.exe",
            "new.dll",
            "new.bat",
            "new.cmd",
            "new.ps1",
            "new.js",
            "run.ps1",
            "tool.exe",
            "new.msi",
            "new.jar",
            "new.app",
            "new.sh.lnk",
            "new.reg",
            "new.vbs",
            "new.msi",
        ] {
            assert!(
                matches!(ws.resolve_write(name), Err(SandboxError::Denied(_))),
                "{name} should be write-denied"
            );
        }
        // Plain documents stay writable; interpreter scripts do NOT — `.js`
        // was already denied on this basis, and `sh x.sh`/`python x.py` are
        // the same class of "MCP must not plant runnable code".
        assert!(ws.resolve_write("notes.md").is_ok());
        for name in ["script.py", "example.sh", "tool.rb", "run.php"] {
            assert!(
                matches!(ws.resolve_write(name), Err(SandboxError::Denied(_))),
                "{name} should be write-denied (interpreter script)"
            );
            // …but still readable for context.
            assert!(ws.resolve(name).is_ok(), "{name} should stay readable");
        }
    }

    #[test]
    fn credentials_are_neither_readable_nor_writable() {
        let (dir, ws) = tmp_workspace();
        for name in [
            ".env",
            ".env.local",
            ".env.production",
            "id_rsa",
            "id_ed25519",
            "key.pem",
            "cert.p12",
            "wallet.kdbx",
            ".netrc",
            ".npmrc",
            ".pgpass",
            "kubeconfig",
            // direnv script — executes on `cd` and usually carries secrets.
            ".envrc",
            // PuTTY key, OpenVPN profile (embedded keys), KeePass1,
            // Terraform state/vars (plaintext secrets), web auth stores,
            // package-manager token configs.
            "deploy.ppk",
            "client.ovpn",
            "old.kdb",
            "terraform.tfstate",
            "prod.tfvars",
            "auto.tfvars.json",
            ".htpasswd",
            ".htdigest",
            ".yarnrc",
            ".yarnrc.yml",
        ] {
            assert!(
                matches!(ws.resolve(name), Err(SandboxError::Denied(_))),
                "{name} should be read-denied"
            );
            assert!(
                matches!(ws.resolve_write(name), Err(SandboxError::Denied(_))),
                "{name} should be write-denied"
            );
        }
        // Documentation-style env examples remain usable.
        assert!(ws.resolve(".env.example").is_ok());
        assert!(ws.resolve_write(".env.sample").is_ok());
        // Credential dirs are denied anywhere in the tree — `.cargo` can hold
        // registry tokens in credentials.toml, `.svn`/`.hg`/`.bzr`/`.terraform`
        // are VCS/tooling internals like `.git`.
        for d in [
            ".ssh/config",
            ".aws/credentials",
            "proj/.ssh/id_rsa",
            ".config/gh/hosts.yml",
            ".cargo/credentials.toml",
            ".svn/entries",
            "vendor/x/.hg/store",
            ".bzr/branch/last-revision",
            ".terraform/providers/hard",
        ] {
            assert!(matches!(ws.resolve(d), Err(SandboxError::Denied(_))), "{d}");
        }
        let _ = dir;
    }

    #[test]
    fn app_and_ci_config_is_write_denied() {
        let (_d, ws) = tmp_workspace();
        for name in [
            "build.rs",
            "Makefile",
            "CMakeLists.txt",
            "package.json",
            "Cargo.toml",
            "setup.py",
            "Dockerfile",
            "docker-compose.yml",
            ".gitlab-ci.yml",
            "Jenkinsfile",
            ".travis.yml",
            ".vscode/settings.json",
            ".idea/workspace.xml",
            ".claude/hooks/x.sh",
            ".cursor/rules.md",
            ".github/workflows/ci.yml",
            "proj/.github/workflows/deploy.yml",
            // Git-hook runners, devcontainers (postCreateCommand), cargo
            // runner config, and .gitignore (steers commit visibility).
            ".gitignore",
            ".husky/pre-commit",
            ".devcontainer/devcontainer.json",
            "lefthook.yml",
            ".pre-commit-config.yaml",
            // Interpreter scripts — runnable code like .bat/.ps1/.js.
            "deploy.sh",
            "tool.py",
            "script.rb",
            "index.php",
            "init.lua",
        ] {
            assert!(
                matches!(ws.resolve_write(name), Err(SandboxError::Denied(_))),
                "{name} should be write-denied"
            );
        }
        // Reading them for context is allowed.
        assert!(ws.resolve("Cargo.toml").is_ok());
        assert!(ws.resolve(".vscode/settings.json").is_ok());
        assert!(ws.resolve(".github/workflows/ci.yml").is_ok());
        assert!(ws.resolve(".gitignore").is_ok());
        assert!(ws.resolve("deploy.sh").is_ok());
        assert!(ws.resolve(".husky/pre-commit").is_ok());
        // …and non-workflow .github files stay writable.
        assert!(ws.resolve_write(".github/CONTRIBUTING.md").is_ok());
    }

    #[test]
    fn device_and_illegal_names_denied() {
        let (_d, ws) = tmp_workspace();
        for name in [
            "NUL",
            "nul.txt",
            "CON",
            "com1.md",
            "LPT3",
            "aux",
            "file:name",
            "trailing ",
            "trailing.",
            "a<b",
            "x|y",
            "star*",
            // NTFS maps superscript digits to devices as well.
            "com¹",
            "LPT²",
            "com³.txt",
        ] {
            assert!(
                matches!(ws.resolve(name), Err(SandboxError::Denied(_))),
                "{name}"
            );
        }
        // Legitimate lookalikes must stay usable.
        for ok in ["com10.md", "lpt12.txt", "comma.md", "console.md", "null.md"] {
            assert!(ws.resolve(ok).is_ok(), "{ok}");
        }
    }

    #[test]
    fn git_is_denied_case_insensitively() {
        let (dir, ws) = tmp_workspace();
        std::fs::create_dir_all(dir.path().join(".git")).unwrap();
        for p in [".git/config", ".GIT/config", ".Git/HEAD"] {
            assert!(matches!(ws.resolve(p), Err(SandboxError::Denied(_))), "{p}");
        }
    }

    /// `--root` must refuse a directory that itself sits inside protected
    /// components — the relative-path checks can't catch the root itself.
    /// A `.git` root would hand the agent repo internals (config, hooks),
    /// a `.ssh` root its credentials.
    #[test]
    fn root_inside_protected_components_is_denied() {
        let dir = tempfile::tempdir().unwrap();
        for protected in [".git", ".ssh", ".config", ".aws"] {
            let p = dir.path().join("proj").join(protected);
            std::fs::create_dir_all(&p).unwrap();
            assert!(
                Workspace::new(&p).is_err(),
                "root inside '{protected}' must be denied"
            );
            // Nested deeper — same.
            let deep = p.join("sub");
            std::fs::create_dir_all(&deep).unwrap();
            assert!(
                Workspace::new(&deep).is_err(),
                "root inside '{protected}/sub' must be denied"
            );
        }
        // An ordinary nested folder remains usable.
        let ok = dir.path().join("proj").join("docs");
        std::fs::create_dir_all(&ok).unwrap();
        assert!(Workspace::new(&ok).is_ok());
    }
}
