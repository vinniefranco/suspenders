//! The skill subsystem (P2c, ADR-0058) - disk-discovered SKILL.md skills.
//!
//! A skill is a directory under `<root>/.suspenders/skills/<name>/` (project) or
//! `~/.suspenders/skills/<name>/` (user) containing a `SKILL.md` manifest: YAML
//! frontmatter between `---` fences (a required `name` + `description`, an
//! optional `when_to_use`, plus the honored fields `priority`, `argument-hint`,
//! `disable-model-invocation`, `paths`, and a nested `hooks:` block) followed by
//! a markdown body. The body is the instruction text the model reads when it
//! invokes the skill. Beneath project and user sits a third source, bundled
//! skills embedded at compile time (Phase 4a, ADR-0058), so precedence is
//! project > user > bundled.
//!
//! This is a LEAF module (ADR-0058): it imports only `std` + `serde_json` (+ the
//! shared `crate::glob_match` and `crate::hooks::config` leaves), never the
//! agent/run/ui/session layers. [`SkillManager::discover`] walks the two disk
//! skill roots and the embedded bundled set fail-open - a malformed or invalid
//! manifest is recorded on `failures` and skipped, mirroring how
//! [`crate::mcp::manager`] records a broken server. The one `skill` tool
//! (`crate::tools::skill`) holds an `Arc` of the manager and embeds
//! [`SkillManager::catalog`] into its dynamically-built description as an
//! `<available_skills>` catalog; that catalog IS the surfacing mechanism, so the
//! tool is always visible (not deferred).
//!
//! ## Conditional (`paths:`) activation
//!
//! A skill whose frontmatter carries `paths:` globs is a *conditional skill*: it
//! is hidden from the catalog until a Tool Call touches a matching file path,
//! then activated for the rest of the Session (ADR-0058, qwen's
//! `skill-activation.ts`). The manager holds a shared, interior-mutable
//! activation registry ([`ActivationRegistry`]); `crate::run::batch` calls
//! [`SkillManager::activate_by_path`] at the tool-success seam, and the `skill`
//! tool reads the same registry through the shared `Arc<SkillManager>` when it
//! builds the catalog, so a newly-activated skill appears next turn with no
//! change-listener (unlike qwen).
//!
//! ## What is honored and what is out
//!
//! Ported from qwen v0.16.0 (`skills/skill-load.ts`, `skills/skill-manager.ts`,
//! `skills/types.ts`, `tools/skill-utils.ts`): the frontmatter fence split, the
//! required-field rejection, the `SKILL_NAME_PATTERN` charset, the XML escape,
//! the LLM content wrapper ([`build_skill_llm_content`]), the honored fields
//! above, alphabetical (name) ordering with `priority:` parsed-but-not-a-sort-key,
//! project-over-user-over-bundled precedence, and conditional activation. `disable-model-invocation` and the
//! `hooks` block are PARSED into [`Skill`] here for Phases 4b/4c to consume but
//! carry no behavior yet. Deferred as OUT (ADR-0058): `model` override,
//! `allowedTools` gating, extension skills, and the change-listener refresh.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde_json::Value;

/// The manifest filename inside each skill directory (qwen's
/// `SKILL_MANIFEST_FILE`).
const SKILL_MANIFEST_FILE: &str = "SKILL.md";

/// Which source a skill was discovered from (ADR-0058). Precedence resolves in
/// this order, project first: a project skill shadows a same-named user skill,
/// which shadows a same-named bundled skill. Recorded on each [`Skill`] so a
/// consumer (and a test) can tell where a skill came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillLevel {
    /// `<project-root>/.suspenders/skills/`.
    Project,
    /// The user's `~/.suspenders/skills/`.
    User,
    /// The set embedded in the binary at compile time.
    Bundled,
}

impl SkillLevel {
    /// The lowercase label qwen appends to each catalog entry's description as a
    /// `(${skill.level})` suffix (tools/skill.ts:174): `project`/`user`/`bundled`.
    pub fn label(self) -> &'static str {
        match self {
            SkillLevel::Project => "project",
            SkillLevel::User => "user",
            SkillLevel::Bundled => "bundled",
        }
    }
}

/// A loaded skill: the required `name`/`description`, the optional `when_to_use`
/// hint, the skill's base directory, the markdown body (trimmed), and the honored
/// frontmatter (ADR-0058). `disable_model_invocation` and `hooks` are parsed for
/// Phases 4b/4c but unused here. Mirrors the fields qwen's `SkillConfig` carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub when_to_use: Option<String>,
    /// The directory the skill's `SKILL.md` lives in. For a disk skill this is
    /// the real path; for a bundled skill it is the synthetic
    /// `<bundled>/<name>` marker (there is no on-disk directory).
    pub base_dir: PathBuf,
    pub body: String,
    /// Which source this skill was discovered from.
    pub level: SkillLevel,
    /// The `priority:` hint, PARSED AND STORED but with no current display
    /// surface: qwen reserves it for its `/skills` listing (which Suspenders does
    /// not have) and keeps the model-facing catalog name-sorted, so priority does
    /// NOT reorder the catalog here (skill-manager.ts:238-243). A missing/invalid
    /// `priority` normalizes to `0`.
    pub priority: i64,
    /// The `argument-hint` string shown after the slash command name in the
    /// `/`-menu completion (Phase 4b consumer). Display-only.
    pub argument_hint: Option<String>,
    /// The `disable-model-invocation` flag: `true` drops the skill from the
    /// model-facing catalog, leaving it `/<name>`-only (Phase 4b consumer).
    /// Parsed here, not wired.
    pub disable_model_invocation: bool,
    /// The `paths:` globs that gate a conditional skill. A non-empty list makes
    /// the skill hidden until a matching file is touched (see
    /// [`ActivationRegistry`]).
    pub paths: Vec<String>,
    /// The nested `hooks:` block parsed to the same `serde_json::Value` shape the
    /// [`crate::hooks::HookManager`] consumes (Phase 4c consumer). Parsed here,
    /// not wired. `None` when absent or malformed (fail-open, ADR-0058).
    pub hooks: Option<Value>,
}

/// The frontmatter fields a manifest parse yields (ADR-0058): the required
/// `name` + `description`, the optional `when_to_use`, and the honored
/// `priority`/`argument-hint`/`disable-model-invocation`/`paths`/`hooks`. Public
/// because it is the return shape of the public [`parse_skill_content`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frontmatter {
    pub name: String,
    pub description: String,
    pub when_to_use: Option<String>,
    pub priority: i64,
    pub argument_hint: Option<String>,
    pub disable_model_invocation: bool,
    pub paths: Vec<String>,
    pub hooks: Option<Value>,
}

/// The shared, interior-mutable set of activated conditional-skill names
/// (ADR-0058, qwen's `SkillActivationRegistry`). A conditional skill (non-empty
/// `paths:`) starts inactive and flips to active - and stays active for the
/// registry's lifetime - the first time a touched file path matches one of its
/// globs. Wrapped in `Arc<Mutex<..>>` so the `skill` tool (reading, when it
/// builds the catalog) and `crate::run::batch` (writing, at the tool-success
/// seam) observe the same set through the shared `Arc<SkillManager>`.
#[derive(Debug, Clone, Default)]
struct ActivationRegistry {
    activated: Arc<Mutex<HashSet<String>>>,
}

impl ActivationRegistry {
    /// Whether `name` has been activated (a plain, non-conditional skill never
    /// enters here - the manager treats it as always active).
    fn is_activated(&self, name: &str) -> bool {
        self.activated
            .lock()
            .map(|set| set.contains(name))
            .unwrap_or(false)
    }

    /// Marks `name` activated.
    fn activate(&self, name: &str) {
        if let Ok(mut set) = self.activated.lock() {
            set.insert(name.to_string());
        }
    }
}

/// The fail-open discovery front for the skill subsystem (ADR-0058), mirroring
/// [`crate::mcp::manager::McpManager`]: [`discover`](SkillManager::discover)
/// walks the project + user skill roots and the embedded bundled set, and a
/// malformed or invalid manifest is recorded on `failures` and skipped rather
/// than crashing startup. The one `skill` tool holds an `Arc` of this manager
/// and reads [`catalog`](SkillManager::catalog) into its `<available_skills>`
/// block; the same `Arc` is threaded to each Run so `crate::run::batch` can
/// [`activate_by_path`](SkillManager::activate_by_path) a conditional skill.
#[derive(Debug, Clone, Default)]
pub struct SkillManager {
    skills: Vec<Skill>,
    failures: Vec<(String, String)>,
    activation: ActivationRegistry,
}

impl SkillManager {
    /// Walks the project skill root, the optional user skill root, and the
    /// embedded bundled set for `<root>/<name>/SKILL.md` manifests, fail-open.
    /// Each disk root is `<...>/.suspenders/skills`; the caller passes those
    /// directories directly. A directory without a `SKILL.md`, a plain file, and
    /// an unreadable root are all silently skipped (qwen's `loadSkillsFromDir`);
    /// a `SKILL.md` that fails to parse or validate is recorded as a
    /// `(skill <name>, reason)` failure and skipped. Sources are walked in
    /// precedence order project > user > bundled, so on a name collision the
    /// higher-precedence skill wins and the shadowed one is dropped silently.
    /// Discovered skills are name-sorted (`priority:` is parsed and stored but
    /// is not a sort key).
    pub fn discover(project_root: &Path, user_root: Option<&Path>) -> SkillManager {
        // Precedence project > user > bundled: the first-loaded skill wins a name
        // collision, so a project skill shadows a same-named user skill, and a
        // disk skill shadows a same-named bundled skill (qwen precedence). The
        // walk order IS the precedence; the Discovery's dedup set enforces it.
        let mut discovery = Discovery::default();
        load_root(project_root, SkillLevel::Project, &mut discovery);
        if let Some(user_root) = user_root {
            load_root(user_root, SkillLevel::User, &mut discovery);
        }
        load_bundled(&mut discovery);
        let Discovery {
            mut skills,
            failures,
            ..
        } = discovery;

        // Stable ALPHABETICAL by name (qwen's `listSkills`, skill-manager.ts:238):
        // `priority:` is parsed and stored but does NOT reorder the model-facing
        // catalog. qwen applies its priority sort only at the `/skills` display
        // layer, which Suspenders does not have; the programmatic consumer here
        // (the `skill` tool's `<available_skills>` block) stays name-sorted.
        sort_skills(&mut skills);

        SkillManager {
            skills,
            failures,
            activation: ActivationRegistry::default(),
        }
    }

    /// Every discovered skill, name-sorted. Includes conditional and
    /// `disable-model-invocation` skills; [`catalog`](SkillManager::catalog) is
    /// the filtered model-facing view.
    pub fn available(&self) -> &[Skill] {
        &self.skills
    }

    /// The model-facing catalog: every non-`disable-model-invocation` skill that
    /// is currently active. A plain skill is always active; a conditional skill
    /// (non-empty `paths:`) is included only once a touched file has activated
    /// it. Name-sorted like [`available`](SkillManager::available). Read by
    /// the `skill` tool to build its `<available_skills>` block and to resolve an
    /// invocation (a skill NOT in this set is not model-invocable).
    pub fn catalog(&self) -> Vec<&Skill> {
        self.skills
            .iter()
            .filter(|s| !s.disable_model_invocation)
            .filter(|s| self.is_active(s))
            .collect()
    }

    /// Whether a skill is currently eligible for the catalog: a plain skill (no
    /// `paths:`) is always active; a conditional skill is active only after a
    /// matching file path activated it (qwen's `isSkillActive`).
    fn is_active(&self, skill: &Skill) -> bool {
        skill.paths.is_empty() || self.activation.is_activated(&skill.name)
    }

    /// Activates any conditional skill whose `paths:` globs match `file_path`,
    /// resolved relative to `project_root` (ADR-0058, qwen's `matchAndConsume`).
    /// Called at the tool-success seam in `crate::run::batch` with the path a
    /// just-run Tool Call touched. A path outside the project root, or one that
    /// matches no inactive conditional skill, is a no-op. Activate-once-sticky:
    /// a skill already active is left alone. Returns the names newly activated by
    /// this call (empty when nothing flipped), so the caller can surface them.
    pub fn activate_by_path(&self, file_path: &Path, project_root: &Path) -> Vec<String> {
        let Some(relative) = project_relative(file_path, project_root) else {
            return Vec::new();
        };

        let mut newly: Vec<String> = Vec::new();
        for skill in &self.skills {
            if skill.paths.is_empty() || self.activation.is_activated(&skill.name) {
                continue;
            }
            if skill.paths.iter().any(|glob| path_matches(glob, &relative)) {
                self.activation.activate(&skill.name);
                newly.push(skill.name.clone());
            }
        }
        newly
    }

    /// The skill matching `name` exactly, or `None`. Matches ANY discovered
    /// skill (including hidden/inactive ones), so it is the discovery-level
    /// lookup; the model-invocable resolution goes through
    /// [`find_invocable`](SkillManager::find_invocable) instead.
    pub fn find(&self, name: &str) -> Option<&Skill> {
        self.skills.iter().find(|s| s.name == name)
    }

    /// The MODEL-INVOCABLE skill matching `name`, or `None`: the same filtered set
    /// [`catalog`](SkillManager::catalog) surfaces (not `disable-model-invocation`
    /// AND currently active). qwen's `validateToolParams` (tools/skill.ts:262-265)
    /// only permits invoking a skill in this set, so the model can never invoke a
    /// hidden or not-yet-activated skill through the `skill` tool.
    pub fn find_invocable(&self, name: &str) -> Option<&Skill> {
        self.catalog().into_iter().find(|s| s.name == name)
    }

    /// Whether `name` is a PENDING conditional skill: a model-invocable
    /// (not `disable-model-invocation`) skill with `paths:` globs that has not yet
    /// been activated by a matching file touch (qwen's
    /// `pendingConditionalSkillNames`, tools/skill.ts:119-129). The `skill` tool
    /// uses this to give qwen's distinct path-gated message instead of the generic
    /// not-found one.
    pub fn is_pending_conditional(&self, name: &str) -> bool {
        self.skills.iter().any(|s| {
            s.name == name
                && !s.disable_model_invocation
                && !s.paths.is_empty()
                && !self.activation.is_activated(&s.name)
        })
    }

    /// The per-manifest discovery failures (`(skill <name>, reason)`). The Agent
    /// surfaces one launch notice per entry after discovery (see `init_agent`),
    /// the same fail-open report line an MCP connect failure takes.
    pub fn failures(&self) -> &[(String, String)] {
        &self.failures
    }
}

/// Sorts skills ALPHABETICALLY by name (qwen's `listSkills`,
/// skill-manager.ts:243: `skills.sort((a, b) => a.name.localeCompare(b.name))`).
/// `priority:` is deliberately NOT a sort key: a qwen manifest with `priority:`
/// still loads and the field is parsed-and-stored, but it does not reorder the
/// model-facing catalog (qwen reserves priority for its `/skills` display layer,
/// which Suspenders has no equivalent of). Deterministic: names are unique after
/// dedup, so the name comparison fully orders the set.
fn sort_skills(skills: &mut [Skill]) {
    skills.sort_by(|a, b| a.name.cmp(&b.name));
}

/// Resolves `file_path` to a project-root-relative form (`/`-separated), or
/// `None` when it escapes the project root (qwen's `resolveProjectRelativePath`
/// scoping: conditional skills are project-scoped). An already-relative path is
/// taken as-is against the root; an absolute path is stripped of the root prefix.
fn project_relative(file_path: &Path, project_root: &Path) -> Option<String> {
    let joined = if file_path.is_absolute() {
        file_path.to_path_buf()
    } else {
        project_root.join(file_path)
    };
    // Lexical: `strip_prefix` against the (possibly relative) root. A path
    // outside the root yields None (conditional skills are project-scoped).
    let relative = joined.strip_prefix(project_root).ok()?;
    Some(relative.to_string_lossy().replace('\\', "/"))
}

/// Whether a project-root-relative `path` matches a skill's `glob` via the shared
/// [`crate::glob_match`] engine (the same matcher behind the ignore-crate walk
/// and `@path` completion, so one glob semantics across the codebase). A glob
/// that will not compile matches nothing (fail-open, no panic).
///
/// Uses the CASE-SENSITIVE compile path: qwen's skill activation builds its
/// picomatch matchers with the default `nocase: false` (skill-activation.ts:100),
/// so `paths: ["**/*.RS"]` activates on `x.RS` but NOT on `x.rs`. This is the one
/// glob caller that wants case-sensitivity; the search tools (glob/grep) keep the
/// case-insensitive [`crate::glob_match::compile`] default.
fn path_matches(glob: &str, path: &str) -> bool {
    crate::glob_match::compile_case_sensitive(glob)
        .map(|re| re.is_match(path))
        .unwrap_or(false)
}

/// The accumulating result of the skill walks (project, then user, then
/// bundled): the loaded skills, the per-directory failures, and the name-dedup
/// set that enforces precedence. ONE value threads through the walks, so the
/// shadow rule (first loader wins a name, silently) lives in
/// [`Discovery::push_parsed`] - not as a three-`&mut`-parameter convention every
/// walk re-states.
#[derive(Default)]
struct Discovery {
    /// Names already loaded from a higher-precedence source (the dedup set).
    seen: HashSet<String>,
    /// The skills loaded so far, in walk order (sorted by the caller).
    skills: Vec<Skill>,
    /// `(dir_name, reason)` for each manifest that failed to parse/validate.
    failures: Vec<(String, String)>,
}

impl Discovery {
    /// Parses one manifest `content` and, on success, pushes the resulting
    /// [`Skill`] unless its name was already loaded from a higher-precedence
    /// source (a silent shadow, not a failure). A parse/validate error is
    /// recorded under `dir_name` (the source directory, since the manifest may
    /// fail before yielding a name). Shared by the disk-root and bundled walks
    /// so precedence + failure handling stay identical.
    fn push_parsed(&mut self, content: &str, base_dir: PathBuf, level: SkillLevel, dir_name: &str) {
        match parse_skill_content(content) {
            Ok((fm, body)) => {
                // A lower-precedence same-named skill loses to the earlier one;
                // record nothing - it is a silent shadow, not a failure.
                if self.seen.contains(&fm.name) {
                    return;
                }
                self.seen.insert(fm.name.clone());
                self.skills.push(Skill {
                    name: fm.name,
                    description: fm.description,
                    when_to_use: fm.when_to_use,
                    base_dir,
                    body,
                    level,
                    priority: fm.priority,
                    argument_hint: fm.argument_hint,
                    disable_model_invocation: fm.disable_model_invocation,
                    paths: fm.paths,
                    hooks: fm.hooks,
                });
            }
            Err(reason) => self.fail(dir_name, reason),
        }
    }

    /// Record a failure for the skill directory `dir_name`.
    fn fail(&mut self, dir_name: &str, reason: String) {
        self.failures.push((dir_name.to_string(), reason));
    }
}

/// Walks one disk skill root's immediate subdirectories for `SKILL.md`
/// manifests. A non-directory entry, a directory without a manifest, or a name
/// already loaded from a higher-precedence source is skipped; a manifest that
/// fails to parse/validate is recorded on the [`Discovery`]. An unreadable root
/// (it does not exist yet) is a silent no-op - the common case where a project
/// has no `.suspenders/skills/` dir.
fn load_root(root: &Path, level: SkillLevel, discovery: &mut Discovery) {
    let entries = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        // The root does not exist / cannot be read: no skills here, not an error.
        Err(_) => return,
    };

    // Collect + sort by directory name so discovery order is stable across runs
    // (read_dir order is filesystem-dependent).
    let mut dirs: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    dirs.sort();

    for dir in dirs {
        let manifest = dir.join(SKILL_MANIFEST_FILE);
        // A directory without a SKILL.md is silently skipped (qwen).
        if !manifest.is_file() {
            continue;
        }

        let dir_name = dir
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();

        let content = match std::fs::read_to_string(&manifest) {
            Ok(content) => content,
            Err(e) => {
                discovery.fail(&dir_name, format!("could not read SKILL.md - {e}"));
                continue;
            }
        };

        discovery.push_parsed(&content, dir.clone(), level, &dir_name);
    }
}

/// Walks the compile-time-embedded bundled set (ADR-0058): each bundled skill's
/// `SKILL.md` is baked into the binary, so there is no on-disk root to read.
/// Same precedence rule as a disk root, one level deeper: a name already loaded
/// from project/user shadows the bundled one silently. The `base_dir` is a
/// synthetic `<bundled>/<name>` marker (there is no real directory), which the
/// LLM content wrapper still renders faithfully.
fn load_bundled(discovery: &mut Discovery) {
    for (dir_name, content) in BUNDLED_SKILLS {
        let base_dir = Path::new("<bundled>").join(dir_name);
        discovery.push_parsed(content, base_dir, SkillLevel::Bundled, dir_name);
    }
}

/// The bundled skills embedded at compile time (ADR-0058). Each entry is
/// `(<name>, <SKILL.md contents>)`, sourced with `include_str!` so no on-disk
/// install step and no extra dependency is needed (Suspenders is a single binary,
/// ADR-0019). qwen's shipped `loop`/`review`/`qc-helper` are deferred (they rely
/// on subsystems Suspenders does not have); `batch` is a faithful port (the one
/// `task` -> `agent` tool rename) and `stuck` is a Suspenders-native rewrite.
const BUNDLED_SKILLS: &[(&str, &str)] = &[
    ("batch", include_str!("bundled/batch/SKILL.md")),
    ("stuck", include_str!("bundled/stuck/SKILL.md")),
];

/// Splits a `SKILL.md` into its frontmatter fields and trimmed body (pure).
/// Ports qwen's `parseSkillContent` regex
/// `^---\n([\s\S]*?)\n---(?:\n|$)([\s\S]*)$` as a manual line scan (no regex):
/// the frontmatter is the block between the leading `---` fence and the next
/// `---` line; the body is everything after, trimmed. Flat `key: value` scalars
/// (`name`, `description`, `when_to_use`, `priority`, `argument-hint`,
/// `disable-model-invocation`) are hand-parsed; the nested `paths:` list and
/// `hooks:` map are carved out and handed to `serde_yaml_ng` (the same YAML path
/// [`crate::hooks::config::hooks_value_from_yaml`] uses), so the hand parser
/// never has to understand nesting. A malformed nested value fails open (the
/// skill still loads off name+description). Errs (skipping the skill) when the
/// fences are missing or the required `name`/`description` are absent or empty.
pub fn parse_skill_content(text: &str) -> Result<(Frontmatter, String), String> {
    // Normalize BOM + CRLF the way qwen's `normalizeContent` does, so a Windows-
    // authored manifest still splits on the `---` fences.
    let normalized = text
        .strip_prefix('\u{feff}')
        .unwrap_or(text)
        .replace("\r\n", "\n")
        .replace('\r', "\n");

    let (frontmatter, body) = split_frontmatter(&normalized)
        .ok_or_else(|| "Invalid format: missing YAML frontmatter".to_string())?;

    let fields = parse_frontmatter_fields(frontmatter);

    let name = required_field(&fields, "name")?;
    validate_skill_name(&name)?;
    let description = required_field(&fields, "description")?;
    let when_to_use = scalar_field(&fields, "when_to_use");
    let priority = parse_priority(&fields);
    let argument_hint = scalar_field(&fields, "argument-hint");
    let disable_model_invocation = parse_bool_field(&fields, "disable-model-invocation");
    let paths = parse_paths_block(frontmatter);
    let hooks = parse_hooks_block(frontmatter);

    Ok((
        Frontmatter {
            name,
            description,
            when_to_use,
            priority,
            argument_hint,
            disable_model_invocation,
            paths,
            hooks,
        },
        body.trim().to_string(),
    ))
}

/// Splits the leading `---`-fenced frontmatter from the body, returning
/// `(frontmatter, body)` or `None` when the opening fence is missing or has no
/// closing `---` line. The `content` MUST already be BOM/CRLF-normalized. Mirrors
/// qwen's regex: the opening fence is the very first line (`^---\n`), the closing
/// fence is a line that is exactly `---`, and the body is everything after that
/// line (allowing the frontmatter to end at EOF via `(?:\n|$)`).
fn split_frontmatter(content: &str) -> Option<(&str, &str)> {
    /// The YAML frontmatter fence marker (qwen's `---`).
    const FENCE: &str = "---";

    // Opening fence must be the first line: `---` then a newline.
    let rest = content.strip_prefix("---\n")?;

    // The closing fence is a line equal to `---`. Scan line boundaries so a `---`
    // inside a value (mid-line) does not close the block.
    let mut search_from = 0;
    loop {
        let idx = rest[search_from..].find(FENCE)?;
        let abs = search_from + idx;
        // The fence must start a line (at the block start or right after a \n).
        let at_line_start = abs == 0 || rest.as_bytes()[abs - 1] == b'\n';
        // ...and end a line: the `---` is followed by a newline or EOF.
        let after = abs + FENCE.len();
        let ends_line = after == rest.len() || rest.as_bytes()[after] == b'\n';
        if at_line_start && ends_line {
            let frontmatter = &rest[..abs];
            // Body is everything after the fence line's trailing newline (or "" at
            // EOF).
            let body = if after < rest.len() {
                &rest[after + 1..]
            } else {
                ""
            };
            return Some((frontmatter, body));
        }
        search_from = abs + FENCE.len();
    }
}

/// Parses the frontmatter block into flat `key: value` scalar pairs, in the
/// order they appear. List/nested YAML fields (a `key:` with no inline value, or
/// a `- ` continuation line) are parsed-and-ignored HERE: the key's value is left
/// empty and the continuation lines are skipped, so the flat scan never crashes
/// on `paths`/`hooks` (those blocks are carved out separately by
/// [`parse_paths_block`]/[`parse_hooks_block`]). A `# comment` line and a blank
/// line are skipped. Quotes around a scalar value are stripped.
fn parse_frontmatter_fields(frontmatter: &str) -> Vec<(String, String)> {
    let mut fields: Vec<(String, String)> = Vec::new();
    for line in frontmatter.lines() {
        let trimmed = line.trim();
        // Skip blanks, comments, and YAML list continuation lines (`- item`).
        if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with('-') {
            continue;
        }
        // A flat `key: value` line. Only split on the FIRST colon so a value
        // containing `:` survives.
        let Some((key, value)) = trimmed.split_once(':') else {
            continue;
        };
        let key = key.trim().to_string();
        let value = strip_quotes(value.trim());
        fields.push((key, value));
    }
    fields
}

/// Carves the nested YAML block for `key` out of the raw frontmatter and returns
/// it as its own YAML document string (or `None` when `key` is absent or has an
/// inline scalar value rather than a nested block). A block is the `key:` line
/// plus every following line that is indented under it, ending at the next line
/// at the frontmatter's top level. The `key:` line is re-emitted verbatim, so
/// `serde_yaml_ng` sees a top-level `key:` mapping - the shape
/// [`crate::hooks::config::hooks_value_from_yaml`] expects.
fn extract_nested_block(frontmatter: &str, key: &str) -> Option<String> {
    let lines: Vec<&str> = frontmatter.lines().collect();
    let key_prefix = format!("{key}:");

    // Find the `key:` line at the frontmatter's top level (indent 0). A nested
    // key inside another block is not a top-level field.
    let start = lines.iter().position(|l| {
        let indent = l.len() - l.trim_start().len();
        indent == 0 && l.trim_start().starts_with(&key_prefix)
    })?;

    // An inline value (`key: something`) is a scalar, not a nested block.
    let inline = lines[start].trim_start()[key_prefix.len()..].trim();
    if !inline.is_empty() {
        return None;
    }

    // Collect the continuation lines: everything after `key:` that is indented
    // (a blank line inside the block is kept; the block ends at the next
    // top-level line).
    let mut block: Vec<String> = vec![format!("{key}:")];
    for line in &lines[start + 1..] {
        if line.trim().is_empty() {
            block.push(String::new());
            continue;
        }
        let indent = line.len() - line.trim_start().len();
        if indent == 0 {
            break;
        }
        block.push((*line).to_string());
    }

    // Nothing followed `key:` - an empty block (qwen's `paths:` with no value).
    if block.len() == 1 {
        return None;
    }
    Some(block.join("\n"))
}

/// Parses the nested `paths:` block into a normalized glob list (ADR-0058, qwen's
/// `parsePathsField`). Carves the block via [`extract_nested_block`] and parses
/// it with `serde_yaml_ng`; a non-list value or an unparseable block yields an
/// empty list (fail-open: the skill loads as unconditional rather than dropping).
/// Blank entries are trimmed out. Absolute or root-escaping (`..`) globs are
/// dropped, since a conditional skill is project-scoped and such a glob could
/// never match a project-relative path anyway.
fn parse_paths_block(frontmatter: &str) -> Vec<String> {
    let Some(block) = extract_nested_block(frontmatter, "paths") else {
        return Vec::new();
    };
    let Ok(value) = serde_yaml_ng::from_str::<Value>(&block) else {
        return Vec::new();
    };
    let Some(list) = value.get("paths").and_then(Value::as_array) else {
        return Vec::new();
    };
    list.iter()
        .filter_map(|v| v.as_str())
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .filter(|p| project_scoped_glob(p))
        .map(str::to_string)
        .collect()
}

/// Whether a `paths:` glob is project-scoped: not absolute and with no `..`
/// segment that would escape the project root (qwen's `parsePathsField` guard).
/// A backslash is normalized to `/` first so a Windows-shaped pattern is judged
/// the same way.
fn project_scoped_glob(glob: &str) -> bool {
    let normalized = glob.replace('\\', "/");
    if normalized.starts_with('/') {
        return false;
    }
    // A drive-letter prefix (`C:...`) is absolute on Windows.
    let mut chars = normalized.chars();
    if let (Some(c), Some(':')) = (chars.next(), chars.next())
        && c.is_ascii_alphabetic()
    {
        return false;
    }
    !normalized.split('/').any(|seg| seg == "..")
}

/// Parses the nested `hooks:` block to the `serde_json::Value` shape the
/// [`crate::hooks::HookManager`] consumes (ADR-0058/0066). Carves the block via
/// [`extract_nested_block`] and reuses
/// [`crate::hooks::config::hooks_value_from_yaml`] (the ONE YAML path for skill
/// hooks) so the skill loader never adds a second YAML approach. A malformed or
/// absent block yields `None` (fail-open: the skill still loads off
/// name+description; the hooks are simply dropped, ADR-0058).
fn parse_hooks_block(frontmatter: &str) -> Option<Value> {
    let block = extract_nested_block(frontmatter, "hooks")?;
    let value = crate::hooks::config::hooks_value_from_yaml(&block).ok()?;
    // The block re-includes the `hooks:` key, so pull the mapping back out.
    value.get("hooks").cloned()
}

/// Parses the flat `priority:` scalar to an `i64`, normalizing a missing, empty,
/// or non-integer value to `0` (ADR-0058, qwen's `parsePriorityField`: a
/// cosmetic ordering hint must never make a valid skill disappear).
fn parse_priority(fields: &[(String, String)]) -> i64 {
    scalar_field(fields, "priority")
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or(0)
}

/// Parses a flat boolean scalar (`disable-model-invocation`): `true` only for the
/// literal `true` (qwen accepts the boolean `true` and the string `"true"`; the
/// flat parser sees a string, so this is the string check). Anything else,
/// including absence, is `false`.
fn parse_bool_field(fields: &[(String, String)], key: &str) -> bool {
    scalar_field(fields, key)
        .map(|v| v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

/// Strips a single matching pair of surrounding single or double quotes from a
/// scalar value (YAML's most common scalar forms). An unquoted value is returned
/// as-is.
fn strip_quotes(value: &str) -> String {
    let bytes = value.as_bytes();
    if bytes.len() >= 2 {
        let first = bytes[0];
        let last = bytes[bytes.len() - 1];
        if (first == b'"' && last == b'"') || (first == b'\'' && last == b'\'') {
            return value[1..value.len() - 1].to_string();
        }
    }
    value.to_string()
}

/// The value of a required frontmatter scalar, or an `Err` naming the missing
/// field (qwen's `Missing "name"` / `Missing "description"` throws, reworded as
/// the parse-error reason that skips the skill). A present-but-empty value is
/// treated as missing, matching qwen's `nameRaw === ''` guard.
fn required_field(fields: &[(String, String)], key: &str) -> Result<String, String> {
    match scalar_field(fields, key) {
        Some(v) => Ok(v),
        None => Err(format!("Missing \"{key}\" in frontmatter")),
    }
}

/// The value of an optional frontmatter scalar, or `None` when the key is absent
/// or its value is empty. The last occurrence wins (a duplicate key overrides).
fn scalar_field(fields: &[(String, String)], key: &str) -> Option<String> {
    fields
        .iter()
        .rev()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.clone())
        .filter(|v| !v.is_empty())
}

/// Rejects a skill `name` whose characters fall outside qwen's
/// `SKILL_NAME_PATTERN` (letters, digits, `_`, `:`, `.`, `-`). The name flows
/// into the model-facing `<available_skills>` catalog verbatim, so a name with a
/// structurally-unsafe character (`<`, `>`, `/`, whitespace) is rejected at parse
/// time - the skill is skipped, matching qwen's `validateSkillName` throw. Uses
/// Unicode letter/digit classes so a non-ASCII (CJK, Cyrillic, accented) name
/// keeps loading, as qwen's `\p{L}\p{N}` charset does.
fn validate_skill_name(name: &str) -> Result<(), String> {
    let ok = name
        .chars()
        .all(|c| c.is_alphabetic() || c.is_numeric() || matches!(c, '_' | ':' | '.' | '-'));
    if ok && !name.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "\"name\" must match /^[\\p{{L}}\\p{{N}}_:.-]+$/u (letters, digits, _, :, ., -); got \"{name}\""
        ))
    }
}

/// Builds the LLM-facing content string a skill invocation returns (VERBATIM from
/// qwen's `buildSkillLlmContent`): the skill's base directory, the absolute-path
/// resolution reminder, then the body. Shared so the `skill` tool's `run` and any
/// future estimate stay in sync.
pub fn build_skill_llm_content(base_dir: &Path, body: &str) -> String {
    format!(
        "Base directory for this skill: {}\nImportant: ALWAYS resolve absolute paths from this base directory when working with skills.\n\n{body}\n",
        base_dir.display()
    )
}

/// Escapes the five XML metacharacters so a skill name/description is safe to
/// embed in the `<available_skills>` catalog verbatim (VERBATIM from qwen's
/// `escapeXml`): a value containing `<`/`>`/`&`/`"`/`'` cannot close the envelope
/// early and forge sibling tags the model would treat as trusted metadata.
pub fn escape_xml(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

#[cfg(test)]
#[path = "../../tests/skills.rs"]
mod tests;
