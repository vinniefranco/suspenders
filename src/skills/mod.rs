//! The skill subsystem (P2c, ADR-0058) - disk-discovered SKILL.md skills.
//!
//! A skill is a directory under `<root>/.suspenders/skills/<name>/` (project) or
//! `~/.suspenders/skills/<name>/` (user) containing a `SKILL.md` manifest: YAML
//! frontmatter between `---` fences (a required `name` + `description`, an
//! optional `when_to_use`, plus qwen's other flat fields parsed-and-ignored so a
//! real qwen manifest still loads) followed by a markdown body. The body is the
//! instruction text the model reads when it invokes the skill.
//!
//! This is a LEAF module (ADR-0058): it imports only `std` + `serde_json`, never
//! the agent/run/ui/session layers. [`SkillManager::discover`] walks the two
//! skill roots fail-open - a malformed or invalid manifest is recorded on
//! `failures` and skipped, mirroring how [`crate::mcp::manager`] records a broken
//! server. The one `skill` tool (`crate::tools::skill`) holds an `Arc` of the
//! manager and embeds [`SkillManager::available`] into its dynamically-built
//! description as an `<available_skills>` catalog; that catalog IS the surfacing
//! mechanism, so the tool is always visible (not deferred).
//!
//! ## What is ported and what is out
//!
//! Ported verbatim from qwen v0.16.0 (`skills/skill-load.ts`, `skills/types.ts`,
//! `tools/skill-utils.ts`): the frontmatter fence split, the required-field
//! rejection, the `SKILL_NAME_PATTERN` charset, the XML escape, and the LLM
//! content wrapper ([`build_skill_llm_content`]). Deferred as OUT (ADR-0058):
//! `paths:` conditional activation, `hooks:`, `model` override, `priority`,
//! `disable-model-invocation`, extension/bundled skill levels, and the
//! change-listener refresh. Those fields are parsed-and-ignored so their presence
//! never blocks a load.

use std::path::{Path, PathBuf};

/// The manifest filename inside each skill directory (qwen's
/// `SKILL_MANIFEST_FILE`).
const SKILL_MANIFEST_FILE: &str = "SKILL.md";

/// A loaded skill: the required `name`/`description`, the optional `when_to_use`
/// hint, the skill's base directory (the directory containing its `SKILL.md`,
/// used for absolute-path resolution), and the markdown body (trimmed). Mirrors
/// the fields qwen's `SkillConfig` carries that Suspenders actually uses; the
/// rest of qwen's fields are parsed-and-ignored (ADR-0058).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub when_to_use: Option<String>,
    pub base_dir: PathBuf,
    pub body: String,
}

/// The frontmatter fields a manifest parse yields: the required `name` +
/// `description` and the optional `when_to_use`. The other qwen frontmatter keys
/// are parsed-and-ignored, so they never reach this struct. Public because it is
/// the return shape of the public [`parse_skill_content`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frontmatter {
    pub name: String,
    pub description: String,
    pub when_to_use: Option<String>,
}

/// The fail-open discovery front for the skill subsystem (ADR-0058), mirroring
/// [`crate::mcp::manager::McpManager`]: [`discover`](SkillManager::discover)
/// walks the project + user skill roots, and a malformed or invalid manifest is
/// recorded on `failures` and skipped rather than crashing startup. The one
/// `skill` tool holds an `Arc` of this manager and reads
/// [`available`](SkillManager::available) into its `<available_skills>` catalog.
#[derive(Debug, Clone, Default)]
pub struct SkillManager {
    skills: Vec<Skill>,
    failures: Vec<(String, String)>,
}

impl SkillManager {
    /// Walks the project skill root (and the optional user skill root) for
    /// `<root>/<name>/SKILL.md` manifests, fail-open. Each root is
    /// `<...>/.suspenders/skills`; the caller passes those directories directly.
    /// A directory without a `SKILL.md`, a plain file, and an unreadable root are
    /// all silently skipped (qwen's `loadSkillsFromDir`); a `SKILL.md` that fails
    /// to parse or validate is recorded as a `(skill <name>, reason)` failure and
    /// skipped. Project skills are walked first, so on a name collision the
    /// project skill wins (`find`/`available` return the first match) - qwen's
    /// project-over-user precedence.
    pub fn discover(project_root: &Path, user_root: Option<&Path>) -> SkillManager {
        let mut skills: Vec<Skill> = Vec::new();
        let mut failures: Vec<(String, String)> = Vec::new();

        // Project first, then user: the first-loaded skill wins a name collision,
        // so a project skill shadows a same-named user skill (qwen precedence).
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        for root in [Some(project_root), user_root].into_iter().flatten() {
            load_root(root, &mut seen, &mut skills, &mut failures);
        }

        SkillManager { skills, failures }
    }

    /// The loaded skills, in discovery order (project before user). Read by the
    /// `skill` tool to build its `<available_skills>` catalog.
    pub fn available(&self) -> &[Skill] {
        &self.skills
    }

    /// The skill matching `name` exactly, or `None`. Used by the `skill` tool's
    /// `run` to resolve an invocation to its body.
    pub fn find(&self, name: &str) -> Option<&Skill> {
        self.skills.iter().find(|s| s.name == name)
    }

    /// The per-manifest discovery failures (`(skill <name>, reason)`). The Agent
    /// surfaces one launch notice per entry after discovery (see `init_agent`),
    /// the same fail-open report line an MCP connect failure takes.
    pub fn failures(&self) -> &[(String, String)] {
        &self.failures
    }
}

/// Walks one skill root's immediate subdirectories for `SKILL.md` manifests. A
/// non-directory entry, a directory without a manifest, or a name already loaded
/// from an earlier root is skipped; a manifest that fails to parse/validate is
/// recorded on `failures`. An unreadable root (it does not exist yet) is a silent
/// no-op - the common case where a project has no `.suspenders/skills/` dir.
fn load_root(
    root: &Path,
    seen: &mut std::collections::HashSet<String>,
    skills: &mut Vec<Skill>,
    failures: &mut Vec<(String, String)>,
) {
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

        let content = match std::fs::read_to_string(&manifest) {
            Ok(content) => content,
            Err(e) => {
                let name = dir
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                failures.push((name, format!("could not read SKILL.md - {e}")));
                continue;
            }
        };

        match parse_skill_content(&content) {
            Ok((fm, body)) => {
                // A later root's same-named skill loses to the earlier one
                // (project-over-user); record nothing - it is not a failure.
                if seen.contains(&fm.name) {
                    continue;
                }
                seen.insert(fm.name.clone());
                skills.push(Skill {
                    name: fm.name,
                    description: fm.description,
                    when_to_use: fm.when_to_use,
                    base_dir: dir.clone(),
                    body,
                });
            }
            Err(reason) => {
                // Name the failure by the directory (the manifest may have failed
                // before yielding a name).
                let name = dir
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                failures.push((name, reason));
            }
        }
    }
}

/// Splits a `SKILL.md` into its frontmatter fields and trimmed body (pure).
/// Ports qwen's `parseSkillContent` regex
/// `^---\n([\s\S]*?)\n---(?:\n|$)([\s\S]*)$` as a manual line scan (no YAML
/// crate, no regex): the frontmatter is the block between the leading `---`
/// fence and the next `---` line; the body is everything after, trimmed. The
/// frontmatter is parsed as flat `key: value` scalars - list/nested qwen fields
/// (`allowedTools`, `hooks`, `paths`) are parsed-and-ignored so a real qwen
/// manifest still loads. Errs (skipping the skill) when the fences are missing or
/// the required `name`/`description` are absent or empty (qwen's throw sites).
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

    Ok((
        Frontmatter {
            name,
            description,
            when_to_use,
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
    // Opening fence must be the first line: `---` then a newline.
    let rest = content.strip_prefix("---\n")?;

    // The closing fence is a line equal to `---`. Scan line boundaries so a `---`
    // inside a value (mid-line) does not close the block.
    let mut search_from = 0;
    loop {
        let idx = rest[search_from..].find("---")?;
        let abs = search_from + idx;
        // The fence must start a line (at the block start or right after a \n).
        let at_line_start = abs == 0 || rest.as_bytes()[abs - 1] == b'\n';
        // ...and end a line: the `---` is followed by a newline or EOF.
        let after = abs + 3;
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
        search_from = abs + 3;
    }
}

/// Parses the frontmatter block into flat `key: value` scalar pairs, in the
/// order they appear, and drops the trailing frontmatter newline qwen's regex
/// leaves out. List/nested YAML fields (a `key:` with no inline value, or a `- `
/// continuation line) are parsed-and-ignored: the key's value is left empty and
/// the continuation lines are skipped, so `allowedTools`/`hooks`/`paths` never
/// crash the parse. A `# comment` line and a blank line are skipped. Quotes
/// around a scalar value are stripped.
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
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn write_skill(root: &Path, name: &str, content: &str) {
        let dir = root.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("SKILL.md"), content).unwrap();
    }

    // ---- parse_skill_content ----

    #[test]
    fn parses_valid_frontmatter_and_trims_the_body() {
        let text = "---\nname: pdf\ndescription: Work with PDF files\n---\n\n  Body text here.  \n";
        let (fm, body) = parse_skill_content(text).unwrap();
        assert_eq!(fm.name, "pdf");
        assert_eq!(fm.description, "Work with PDF files");
        assert_eq!(fm.when_to_use, None);
        assert_eq!(body, "Body text here.");
    }

    #[test]
    fn when_to_use_is_optional_and_parsed_when_present() {
        let text =
            "---\nname: pdf\ndescription: Work with PDFs\nwhen_to_use: When asked about a PDF\n---\nbody";
        let (fm, _) = parse_skill_content(text).unwrap();
        assert_eq!(fm.when_to_use.as_deref(), Some("When asked about a PDF"));
    }

    #[test]
    fn missing_name_is_an_error() {
        let text = "---\ndescription: no name here\n---\nbody";
        assert_eq!(
            parse_skill_content(text),
            Err("Missing \"name\" in frontmatter".to_string())
        );
    }

    #[test]
    fn missing_description_is_an_error() {
        let text = "---\nname: pdf\n---\nbody";
        assert_eq!(
            parse_skill_content(text),
            Err("Missing \"description\" in frontmatter".to_string())
        );
    }

    #[test]
    fn empty_name_value_counts_as_missing() {
        let text = "---\nname:\ndescription: has desc\n---\nbody";
        assert!(parse_skill_content(text).is_err());
    }

    #[test]
    fn missing_frontmatter_fences_is_an_error() {
        let text = "no frontmatter at all\njust a body";
        assert_eq!(
            parse_skill_content(text),
            Err("Invalid format: missing YAML frontmatter".to_string())
        );
    }

    #[test]
    fn unknown_and_list_fields_are_parsed_and_ignored() {
        // A real qwen SKILL.md with allowedTools (a list), model, priority, and
        // paths must still load off name + description.
        let text = "---\nname: office\ndescription: Office suite\nmodel: fast\npriority: 5\nallowedTools:\n  - read_file\n  - grep\npaths:\n  - src/**\n---\nbody";
        let (fm, body) = parse_skill_content(text).unwrap();
        assert_eq!(fm.name, "office");
        assert_eq!(fm.description, "Office suite");
        assert_eq!(body, "body");
    }

    #[test]
    fn frontmatter_may_end_at_eof_without_a_body() {
        let text = "---\nname: pdf\ndescription: desc\n---";
        let (fm, body) = parse_skill_content(text).unwrap();
        assert_eq!(fm.name, "pdf");
        assert_eq!(body, "");
    }

    #[test]
    fn quoted_scalar_values_are_unquoted() {
        let text = "---\nname: \"pdf\"\ndescription: 'Work with PDFs'\n---\nbody";
        let (fm, _) = parse_skill_content(text).unwrap();
        assert_eq!(fm.name, "pdf");
        assert_eq!(fm.description, "Work with PDFs");
    }

    #[test]
    fn a_colon_in_the_description_value_survives() {
        let text = "---\nname: pdf\ndescription: Ratio 16:9 handling\n---\nbody";
        let (fm, _) = parse_skill_content(text).unwrap();
        assert_eq!(fm.description, "Ratio 16:9 handling");
    }

    // ---- validate_skill_name ----

    #[test]
    fn name_charset_rejects_structurally_unsafe_characters() {
        assert!(validate_skill_name("pdf").is_ok());
        assert!(validate_skill_name("ms-office:pdf").is_ok());
        assert!(validate_skill_name("skill_v1.2").is_ok());
        // Non-ASCII letters keep working (qwen's \p{L}\p{N} charset).
        assert!(validate_skill_name("日本語").is_ok());
        // Structurally unsafe: injection vectors and whitespace.
        assert!(validate_skill_name("bad name").is_err());
        assert!(validate_skill_name("bad/name").is_err());
        assert!(validate_skill_name("<script>").is_err());
        assert!(validate_skill_name("a&b").is_err());
    }

    #[test]
    fn a_rejected_name_carries_the_verbatim_qwen_reason() {
        assert_eq!(
            validate_skill_name("bad name"),
            Err(
                "\"name\" must match /^[\\p{L}\\p{N}_:.-]+$/u (letters, digits, _, :, ., -); got \"bad name\""
                    .to_string()
            )
        );
    }

    #[test]
    fn a_bad_name_makes_the_whole_skill_a_parse_error() {
        let text = "---\nname: bad name\ndescription: desc\n---\nbody";
        assert!(parse_skill_content(text).is_err());
    }

    // ---- SkillManager::discover ----

    #[test]
    fn discover_finds_a_well_formed_skill() {
        let tmp = TempDir::new().unwrap();
        write_skill(
            tmp.path(),
            "pdf",
            "---\nname: pdf\ndescription: Work with PDFs\n---\nbody",
        );
        let mgr = SkillManager::discover(tmp.path(), None);
        assert_eq!(mgr.available().len(), 1);
        assert_eq!(mgr.available()[0].name, "pdf");
        assert_eq!(mgr.available()[0].base_dir, tmp.path().join("pdf"));
        assert!(mgr.failures().is_empty());
    }

    #[test]
    fn discover_skips_a_directory_without_a_manifest() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join("empty")).unwrap();
        write_skill(
            tmp.path(),
            "pdf",
            "---\nname: pdf\ndescription: desc\n---\nbody",
        );
        let mgr = SkillManager::discover(tmp.path(), None);
        assert_eq!(mgr.available().len(), 1);
        assert_eq!(mgr.available()[0].name, "pdf");
        // The manifest-less dir is a silent skip, not a failure.
        assert!(mgr.failures().is_empty());
    }

    #[test]
    fn discover_records_a_malformed_manifest_as_a_failure() {
        let tmp = TempDir::new().unwrap();
        write_skill(tmp.path(), "broken", "no frontmatter here");
        let mgr = SkillManager::discover(tmp.path(), None);
        assert!(mgr.available().is_empty());
        assert_eq!(mgr.failures().len(), 1);
        assert_eq!(mgr.failures()[0].0, "broken");
        assert!(mgr.failures()[0].1.contains("missing YAML frontmatter"));
    }

    #[test]
    fn discover_records_a_missing_required_field_as_a_failure() {
        let tmp = TempDir::new().unwrap();
        write_skill(tmp.path(), "noname", "---\ndescription: desc\n---\nbody");
        let mgr = SkillManager::discover(tmp.path(), None);
        assert!(mgr.available().is_empty());
        assert_eq!(mgr.failures().len(), 1);
        assert_eq!(mgr.failures()[0].0, "noname");
    }

    #[test]
    fn a_missing_root_is_a_silent_no_op() {
        let tmp = TempDir::new().unwrap();
        let mgr = SkillManager::discover(&tmp.path().join("does-not-exist"), None);
        assert!(mgr.available().is_empty());
        assert!(mgr.failures().is_empty());
    }

    #[test]
    fn project_shadows_user_on_a_name_collision() {
        let project = TempDir::new().unwrap();
        let user = TempDir::new().unwrap();
        write_skill(
            project.path(),
            "pdf",
            "---\nname: pdf\ndescription: PROJECT pdf\n---\nproject body",
        );
        write_skill(
            user.path(),
            "pdf",
            "---\nname: pdf\ndescription: USER pdf\n---\nuser body",
        );
        let mgr = SkillManager::discover(project.path(), Some(user.path()));
        // Only the project skill survives; the user one is shadowed, not failed.
        assert_eq!(mgr.available().len(), 1);
        assert_eq!(mgr.available()[0].description, "PROJECT pdf");
        assert!(mgr.failures().is_empty());
    }

    #[test]
    fn user_skills_load_when_not_shadowed() {
        let project = TempDir::new().unwrap();
        let user = TempDir::new().unwrap();
        write_skill(
            project.path(),
            "pdf",
            "---\nname: pdf\ndescription: PROJECT\n---\nb",
        );
        write_skill(
            user.path(),
            "xlsx",
            "---\nname: xlsx\ndescription: USER\n---\nb",
        );
        let mgr = SkillManager::discover(project.path(), Some(user.path()));
        let names: Vec<&str> = mgr.available().iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["pdf", "xlsx"]);
    }

    // ---- build_skill_llm_content ----

    #[test]
    fn build_skill_llm_content_is_verbatim() {
        let out = build_skill_llm_content(Path::new("/tmp/skills/pdf"), "Do the thing.");
        assert_eq!(
            out,
            "Base directory for this skill: /tmp/skills/pdf\nImportant: ALWAYS resolve absolute paths from this base directory when working with skills.\n\nDo the thing.\n"
        );
    }

    // ---- escape_xml ----

    #[test]
    fn escape_xml_covers_all_five_metacharacters() {
        assert_eq!(
            escape_xml("<a & b> \"c\" 'd'"),
            "&lt;a &amp; b&gt; &quot;c&quot; &apos;d&apos;"
        );
    }
}
