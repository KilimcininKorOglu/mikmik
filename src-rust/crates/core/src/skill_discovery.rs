//! Skill discovery: load custom prompt-template skills from markdown files
//! on disk and (optionally) from git URLs.
//!
//! Every source is scanned and every skill is kept, so the user can reach all
//! of them from the picker. Two skills may share a name; the source (its
//! [`SkillOrigin`]) both tags the description in the picker and, on a name
//! clash, decides which skill keeps the bare command name.
//!
//! Sources, in priority order (the rank that wins the bare name on a clash):
//!   1. Project `.mikmik/skills/` — walk up from `cwd`  → `mikmik-project`
//!   2. Project `.agents/skills/`  — walk up from `cwd` → `agents-project`
//!   3. Global `~/.config/mikmik/skills/`               → `mikmik-global`
//!   4. Global `~/.agents/skills/`                      → `agents-global`
//!   5. Configured extra paths from `SkillsConfig.paths` → `configured`
//!   6. Git-URL repos from `SkillsConfig.urls` (cloned once, cached) → `url`

use std::path::{Path, PathBuf};

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// A discovered skill loaded from a markdown file.
#[derive(Debug, Clone)]
pub struct DiscoveredSkill {
    /// Skill name (from `name:` frontmatter or file stem).
    pub name: String,
    /// One-line description (from `description:` frontmatter or default).
    pub description: String,
    /// The prompt body after stripping frontmatter.
    pub template: String,
    /// Absolute path to the source `.md` file.
    pub source_path: PathBuf,
}

/// Where a discovered skill came from.
///
/// The origin drives the `(origin)` label shown in the picker and the
/// `@origin` suffix a skill takes when it must yield the bare command name to a
/// higher-priority skill of the same name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillOrigin {
    /// Project `.mikmik/skills/`.
    MikmikProject,
    /// Global `~/.config/mikmik/skills/`.
    MikmikGlobal,
    /// Project `.agents/skills/`.
    AgentsProject,
    /// Global `~/.agents/skills/`.
    AgentsGlobal,
    /// A path listed in `SkillsConfig.paths`.
    Configured,
    /// A skill cloned from a git URL in `SkillsConfig.urls`.
    Url,
}

impl SkillOrigin {
    /// The source label, e.g. `mikmik-project`. Used both as the `(origin)`
    /// description prefix and as the `@origin` command-name suffix.
    pub fn label(self) -> &'static str {
        match self {
            SkillOrigin::MikmikProject => "mikmik-project",
            SkillOrigin::MikmikGlobal => "mikmik-global",
            SkillOrigin::AgentsProject => "agents-project",
            SkillOrigin::AgentsGlobal => "agents-global",
            SkillOrigin::Configured => "configured",
            SkillOrigin::Url => "url",
        }
    }

    /// Priority rank; the lowest rank in a name-clash group keeps the bare
    /// command name. mikmik always outranks agents, and project always
    /// outranks global, so the default source is always mikmik.
    fn rank(self) -> u8 {
        match self {
            SkillOrigin::MikmikProject => 0,
            SkillOrigin::MikmikGlobal => 1,
            SkillOrigin::AgentsProject => 2,
            SkillOrigin::AgentsGlobal => 3,
            SkillOrigin::Configured => 4,
            SkillOrigin::Url => 5,
        }
    }
}

/// A discovered skill together with its origin and the exact command name that
/// reaches it.
///
/// `command_name` is unique across the whole discovery result: on a name clash
/// the highest-priority skill keeps the bare name and the rest take
/// `name@origin`, so every discovered skill stays typeable and pickable.
#[derive(Debug, Clone)]
pub struct ResolvedSkill {
    /// The parsed skill.
    pub skill: DiscoveredSkill,
    /// The source this skill came from.
    pub origin: SkillOrigin,
    /// What a user types after `/` to run this skill.
    pub command_name: String,
}

impl ResolvedSkill {
    /// The description shown in the picker, prefixed with the `(origin)` tag.
    pub fn tagged_description(&self) -> String {
        format!("({}) {}", self.origin.label(), self.skill.description)
    }
}

// ---------------------------------------------------------------------------
// Frontmatter parsing
// ---------------------------------------------------------------------------

/// Parse a skill markdown file.
///
/// Expects optional YAML frontmatter delimited by `---`.
/// Returns `None` when the file is empty after trimming.
pub fn parse_skill_file(content: &str, path: &Path) -> Option<DiscoveredSkill> {
    let content = content.trim();
    if content.is_empty() {
        return None;
    }

    let (name, description, template) = if let Some(after_open) = content.strip_prefix("---") {
        // Accept both `\n---` and `\r\n---` as closing delimiter.
        if let Some(close_pos) = after_open.find("\n---") {
            let frontmatter = &after_open[..close_pos];
            let rest = after_open[close_pos + 4..].trim_start_matches(['\r', '\n']);

            let mut name: Option<String> = None;
            let mut description: Option<String> = None;

            for line in frontmatter.lines() {
                let line = line.trim();
                if let Some(v) = line.strip_prefix("name:") {
                    name = Some(v.trim().trim_matches('"').trim_matches('\'').to_string());
                } else if let Some(v) = line.strip_prefix("description:") {
                    description = Some(v.trim().trim_matches('"').trim_matches('\'').to_string());
                }
            }

            (name, description, rest.to_string())
        } else {
            // Malformed frontmatter — treat entire content as template.
            (None, None, content.to_string())
        }
    } else {
        (None, None, content.to_string())
    };

    let name = name.unwrap_or_else(|| {
        path.file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("unnamed")
            .to_string()
    });
    let description = description.unwrap_or_else(|| "Custom skill".to_string());

    if template.is_empty() && name.is_empty() {
        return None;
    }

    Some(DiscoveredSkill {
        name,
        description,
        template,
        source_path: path.to_path_buf(),
    })
}

/// Drop a leading `---` YAML frontmatter block from a prompt body.
///
/// Every caller that expands a markdown prompt needs this, because the
/// frontmatter describes the file to the loader and means nothing to a model.
pub fn strip_frontmatter(content: &str) -> String {
    if let Some(after_open) = content.strip_prefix("---") {
        if let Some(close_pos) = after_open.find("\n---") {
            let rest = &after_open[close_pos + 4..];
            return rest.trim_start_matches(['\r', '\n']).to_string();
        }
    }
    content.to_string()
}

// ---------------------------------------------------------------------------
// Directory scanning
// ---------------------------------------------------------------------------

/// Scan a single directory for skills.
///
/// Two layouts are accepted: a flat `<dir>/<name>.md` file, and a
/// `<dir>/<name>/SKILL.md` package, which is what a plugin's `skills/`
/// directory holds.
fn scan_dir(dir: &Path) -> Vec<DiscoveredSkill> {
    let mut skills = Vec::new();
    if !dir.is_dir() {
        return skills;
    }

    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(err) => {
            tracing::debug!(dir = %dir.display(), error = %err, "skill_discovery: read_dir failed");
            return skills;
        }
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if let Some(skill) = scan_skill_package(&path) {
                skills.push(skill);
            }
        } else if path.extension().and_then(|e| e.to_str()) == Some("md") {
            match std::fs::read_to_string(&path) {
                Ok(content) => {
                    if let Some(skill) = parse_skill_file(&content, &path) {
                        skills.push(skill);
                    }
                }
                Err(err) => {
                    tracing::debug!(path = %path.display(), error = %err, "skill_discovery: read failed");
                }
            }
        }
    }

    skills
}

/// Read `<dir>/SKILL.md` (or `skill.md`) as one skill named after `dir`.
///
/// The file stem is the same for every package, so the directory name is the
/// only usable fallback when the frontmatter carries no `name:`.
fn scan_skill_package(dir: &Path) -> Option<DiscoveredSkill> {
    let manifest = ["SKILL.md", "skill.md"]
        .iter()
        .map(|f| dir.join(f))
        .find(|p| p.is_file())?;

    let content = match std::fs::read_to_string(&manifest) {
        Ok(c) => c,
        Err(err) => {
            tracing::debug!(path = %manifest.display(), error = %err, "skill_discovery: read failed");
            return None;
        }
    };

    let mut skill = parse_skill_file(&content, &manifest)?;
    let stem = manifest.file_stem().and_then(|s| s.to_str()).unwrap_or("");
    if skill.name == stem {
        if let Some(dir_name) = dir.file_name().and_then(|n| n.to_str()) {
            skill.name = dir_name.to_string();
        }
    }
    Some(skill)
}

// ---------------------------------------------------------------------------
// Top-level discovery
// ---------------------------------------------------------------------------

/// Resolve one configured skill path.
///
/// A leading `~/` expands to the home directory, so a path like
/// `~/.agents/skills` reaches the user's home instead of being joined onto
/// `cwd`. An absolute path is kept as it is; anything else is taken relative to
/// `cwd`. `home` is `None` only when the home directory cannot be found, and
/// then a `~/` path is left untouched rather than resolved wrongly.
fn resolve_skill_path(path_str: &str, cwd: &Path, home: Option<&Path>) -> PathBuf {
    if let Some(rest) = path_str.strip_prefix("~/") {
        return match home {
            Some(home) => home.join(rest),
            None => PathBuf::from(path_str),
        };
    }
    let path = Path::new(path_str);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    }
}

/// Assign a unique, typeable `command_name` to every discovered skill.
///
/// Skills that share a name are ordered by origin priority (then source path
/// for a stable result). The first keeps the bare name; each other takes
/// `name@origin`, and a `-N` suffix is added only if that string still repeats
/// (two skills of the same name from the same origin). The bare name therefore
/// always resolves to the highest-priority (mikmik-first) skill, and every
/// other skill stays reachable under a name the user can see and type.
fn assign_command_names(tagged: Vec<(SkillOrigin, DiscoveredSkill)>) -> Vec<ResolvedSkill> {
    use std::collections::{BTreeMap, HashSet};

    // Group input positions by skill name, in a deterministic order.
    let mut groups: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    for (i, (_, skill)) in tagged.iter().enumerate() {
        groups.entry(skill.name.clone()).or_default().push(i);
    }

    let mut command_names: Vec<Option<String>> = vec![None; tagged.len()];
    let mut used: HashSet<String> = HashSet::new();

    for (_, mut idxs) in groups {
        idxs.sort_by(|&a, &b| {
            tagged[a]
                .0
                .rank()
                .cmp(&tagged[b].0.rank())
                .then_with(|| tagged[a].1.source_path.cmp(&tagged[b].1.source_path))
        });
        for (pos, &i) in idxs.iter().enumerate() {
            let (origin, skill) = &tagged[i];
            let base = if pos == 0 {
                skill.name.clone()
            } else {
                format!("{}@{}", skill.name, origin.label())
            };
            let mut candidate = base.clone();
            let mut n = 2;
            while used.contains(&candidate) {
                candidate = format!("{}-{}", base, n);
                n += 1;
            }
            used.insert(candidate.clone());
            command_names[i] = Some(candidate);
        }
    }

    tagged
        .into_iter()
        .zip(command_names)
        .map(|((origin, skill), command_name)| ResolvedSkill {
            skill,
            origin,
            command_name: command_name.unwrap_or_default(),
        })
        .collect()
}

/// Discover all skills from every configured source.
///
/// Returns every discovered skill, each tagged with its [`SkillOrigin`] and a
/// unique `command_name`. Nothing is deduplicated: two skills of the same name
/// are both returned and told apart by `command_name` (see
/// [`assign_command_names`]).
pub fn discover_skills(
    cwd: &Path,
    config_skills: &crate::config::SkillsConfig,
) -> Vec<ResolvedSkill> {
    let mut tagged: Vec<(SkillOrigin, DiscoveredSkill)> = Vec::new();
    let mut push = |origin: SkillOrigin, skills: Vec<DiscoveredSkill>| {
        for skill in skills {
            tagged.push((origin, skill));
        }
    };

    // ---- 1. Project skills: walk up from cwd --------------------------------
    {
        let mut dir: &Path = cwd;
        loop {
            push(
                SkillOrigin::MikmikProject,
                scan_dir(&dir.join(".mikmik").join("skills")),
            );
            push(
                SkillOrigin::AgentsProject,
                scan_dir(&dir.join(".agents").join("skills")),
            );
            match dir.parent() {
                Some(parent) if parent != dir => dir = parent,
                _ => break,
            }
        }
    }

    // ---- 2. Global mikmik skills: <mikmik home>/skills/ --------------------
    push(
        SkillOrigin::MikmikGlobal,
        scan_dir(&crate::config::Settings::config_dir().join("skills")),
    );

    // ---- 3. Global agents skills: ~/.agents/skills/ ------------------------
    if let Some(home) = dirs::home_dir() {
        push(
            SkillOrigin::AgentsGlobal,
            scan_dir(&home.join(".agents").join("skills")),
        );
    }

    // ---- 4. Configured extra paths ------------------------------------------
    for path_str in &config_skills.paths {
        let path = resolve_skill_path(path_str, cwd, dirs::home_dir().as_deref());
        push(SkillOrigin::Configured, scan_dir(&path));
    }

    // ---- 5. Git URL skills (cached) -----------------------------------------
    for url in &config_skills.urls {
        if let Some(git_skills) = fetch_git_skills(url) {
            push(SkillOrigin::Url, git_skills);
        }
    }

    assign_command_names(tagged)
}

// ---------------------------------------------------------------------------
// Git URL support
// ---------------------------------------------------------------------------

/// Clone or reuse a cached git repo and return skills found in it.
///
/// Cache location: `<system-cache>/mikmik/skills/<repo-name>/`
/// On first access the repo is cloned with `--depth=1`.
/// Subsequent calls use the already-cloned cache directory as-is.
fn fetch_git_skills(url: &str) -> Option<Vec<DiscoveredSkill>> {
    let cache_dir = dirs::cache_dir()?.join("mikmik").join("skills");

    // Use the last path segment of the URL as the local directory name.
    let repo_name = url.split('/').next_back()?.trim_end_matches(".git");

    if repo_name.is_empty() {
        tracing::warn!(url, "skill_discovery: cannot derive repo name from git URL");
        return None;
    }

    let repo_dir = cache_dir.join(repo_name);

    if !repo_dir.exists() {
        tracing::info!(url, dest = %repo_dir.display(), "skill_discovery: cloning skills repo");

        // Ensure the parent cache directory exists.
        if let Err(err) = std::fs::create_dir_all(&cache_dir) {
            tracing::warn!(
                dir = %cache_dir.display(),
                error = %err,
                "skill_discovery: could not create cache dir"
            );
            return None;
        }

        let repo_dir_str = repo_dir.to_str()?;
        let status = std::process::Command::new("git")
            .args(["clone", "--depth=1", url, repo_dir_str])
            .status();

        match status {
            Ok(s) if s.success() => {
                tracing::info!(url, "skill_discovery: clone succeeded");
            }
            Ok(s) => {
                tracing::warn!(url, exit_code = ?s.code(), "skill_discovery: git clone failed");
                return None;
            }
            Err(err) => {
                tracing::warn!(url, error = %err, "skill_discovery: could not spawn git");
                return None;
            }
        }
    }

    // Scan repo root and optional `skills/` subdirectory.
    let mut skills = scan_dir(&repo_dir);
    skills.extend(scan_dir(&repo_dir.join("skills")));
    Some(skills)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as IoWrite;

    fn make_temp_dir() -> tempfile::TempDir {
        tempfile::tempdir().expect("tempdir")
    }

    fn write_file(dir: &Path, name: &str, content: &str) {
        let path = dir.join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(content.as_bytes()).unwrap();
    }

    // ---- parse_skill_file ---------------------------------------------------

    #[test]
    fn test_parse_with_frontmatter() {
        let content =
            "---\nname: review\ndescription: Review code changes\n---\n\nPlease review $ARGUMENTS";
        let path = PathBuf::from("review.md");
        let skill = parse_skill_file(content, &path).unwrap();
        assert_eq!(skill.name, "review");
        assert_eq!(skill.description, "Review code changes");
        assert!(skill.template.contains("$ARGUMENTS"));
    }

    #[test]
    fn test_parse_no_frontmatter_uses_stem() {
        let content = "Do something useful.";
        let path = PathBuf::from("my-skill.md");
        let skill = parse_skill_file(content, &path).unwrap();
        assert_eq!(skill.name, "my-skill");
        assert_eq!(skill.description, "Custom skill");
        assert_eq!(skill.template, "Do something useful.");
    }

    #[test]
    fn test_parse_missing_name_uses_stem() {
        let content = "---\ndescription: No name field\n---\n\nBody text.";
        let path = PathBuf::from("fallback.md");
        let skill = parse_skill_file(content, &path).unwrap();
        assert_eq!(skill.name, "fallback");
        assert_eq!(skill.description, "No name field");
    }

    #[test]
    fn test_parse_empty_returns_none() {
        let skill = parse_skill_file("   ", &PathBuf::from("empty.md"));
        assert!(skill.is_none());
    }

    #[test]
    fn test_parse_quoted_frontmatter_values() {
        let content = "---\nname: \"quoted name\"\ndescription: 'single quoted'\n---\nBody.";
        let skill = parse_skill_file(content, &PathBuf::from("x.md")).unwrap();
        assert_eq!(skill.name, "quoted name");
        assert_eq!(skill.description, "single quoted");
    }

    // ---- scan_dir -----------------------------------------------------------

    #[test]
    fn test_scan_dir_finds_skills() {
        let tmp = make_temp_dir();
        write_file(
            tmp.path(),
            "review.md",
            "---\nname: review\n---\nReview $ARGUMENTS",
        );
        write_file(tmp.path(), "debug.md", "Debug help.");
        write_file(tmp.path(), "not-md.txt", "ignored");

        let skills = scan_dir(tmp.path());
        assert_eq!(skills.len(), 2);
        let names: Vec<&str> = skills.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"review"));
        assert!(names.contains(&"debug"));
    }

    #[test]
    fn a_skill_package_is_named_after_its_directory() {
        let tmp = make_temp_dir();
        let package = tmp.path().join("release-notes");
        std::fs::create_dir_all(&package).unwrap();
        write_file(&package, "SKILL.md", "Write the notes.");

        let skills = scan_dir(tmp.path());
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "release-notes");
    }

    #[test]
    fn a_skill_package_keeps_the_name_its_frontmatter_gives() {
        let tmp = make_temp_dir();
        let package = tmp.path().join("release-notes");
        std::fs::create_dir_all(&package).unwrap();
        write_file(
            &package,
            "SKILL.md",
            "---\nname: notes\ndescription: Ship it\n---\nWrite the notes.",
        );

        let skills = scan_dir(tmp.path());
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "notes");
        assert_eq!(skills[0].description, "Ship it");
    }

    #[test]
    fn a_directory_without_a_skill_file_is_skipped() {
        let tmp = make_temp_dir();
        let package = tmp.path().join("empty-dir");
        std::fs::create_dir_all(&package).unwrap();
        write_file(&package, "README.md", "Not a skill entry point.");

        assert!(scan_dir(tmp.path()).is_empty());
    }

    #[test]
    fn frontmatter_goes_and_the_body_stays() {
        let stripped = strip_frontmatter("---\nname: x\n---\nBody line.");
        assert_eq!(stripped, "Body line.");
    }

    #[test]
    fn a_body_without_frontmatter_is_untouched() {
        assert_eq!(strip_frontmatter("Just a body."), "Just a body.");
    }

    #[test]
    fn an_unclosed_frontmatter_block_is_left_alone() {
        let content = "---\nname: x\nBody with no close.";
        assert_eq!(strip_frontmatter(content), content);
    }

    #[test]
    fn test_scan_dir_nonexistent_returns_empty() {
        let skills = scan_dir(Path::new("/nonexistent/path/xyz"));
        assert!(skills.is_empty());
    }

    // ---- discover_skills ----------------------------------------------------

    /// Locate a discovered skill by the command name it resolves to.
    fn by_command<'a>(skills: &'a [ResolvedSkill], command: &str) -> Option<&'a ResolvedSkill> {
        skills.iter().find(|r| r.command_name == command)
    }

    #[test]
    fn test_discover_from_project_dir() {
        let tmp = make_temp_dir();
        let skills_dir = tmp.path().join(".mikmik").join("skills");
        std::fs::create_dir_all(&skills_dir).unwrap();
        write_file(
            &skills_dir,
            "myskill.md",
            "---\nname: myskill\ndescription: Test\n---\nDo it.",
        );

        let config = crate::config::SkillsConfig::default();
        let discovered = discover_skills(tmp.path(), &config);
        let skill = by_command(&discovered, "myskill").expect("myskill discovered");
        assert_eq!(skill.origin, SkillOrigin::MikmikProject);
        assert_eq!(skill.skill.description, "Test");
    }

    #[test]
    fn test_discover_extra_paths() {
        let tmp = make_temp_dir();
        let extra = make_temp_dir();
        write_file(
            extra.path(),
            "extra.md",
            "---\nname: extra\n---\nExtra skill.",
        );

        let config = crate::config::SkillsConfig {
            paths: vec![extra.path().to_str().unwrap().to_string()],
            urls: vec![],
        };
        let discovered = discover_skills(tmp.path(), &config);
        // No clash, so a configured skill keeps the bare command name.
        let skill = by_command(&discovered, "extra").expect("extra discovered");
        assert_eq!(skill.origin, SkillOrigin::Configured);
    }

    #[test]
    fn clashing_skills_are_all_kept_and_disambiguated() {
        let tmp = make_temp_dir();
        let proj_skills = tmp.path().join(".mikmik").join("skills");
        std::fs::create_dir_all(&proj_skills).unwrap();
        write_file(
            &proj_skills,
            "dup.md",
            "---\nname: dup\ndescription: project\n---\nProject.",
        );

        let extra = make_temp_dir();
        write_file(
            extra.path(),
            "dup.md",
            "---\nname: dup\ndescription: extra\n---\nExtra.",
        );

        let config = crate::config::SkillsConfig {
            paths: vec![extra.path().to_str().unwrap().to_string()],
            urls: vec![],
        };
        let discovered = discover_skills(tmp.path(), &config);
        // Both are kept: nothing is deduplicated.
        let dups: Vec<_> = discovered
            .iter()
            .filter(|r| r.skill.name == "dup")
            .collect();
        assert_eq!(dups.len(), 2);
        // The mikmik-project skill keeps the bare command name.
        let bare = by_command(&discovered, "dup").expect("bare dup");
        assert_eq!(bare.origin, SkillOrigin::MikmikProject);
        assert_eq!(bare.skill.description, "project");
        // The configured skill stays reachable under a qualified name.
        let qualified = by_command(&discovered, "dup@configured").expect("qualified dup");
        assert_eq!(qualified.skill.description, "extra");
    }

    #[test]
    fn mikmik_project_outranks_agents_project_for_the_bare_name() {
        let tmp = make_temp_dir();
        let mikmik_dir = tmp.path().join(".mikmik").join("skills");
        std::fs::create_dir_all(&mikmik_dir).unwrap();
        write_file(
            &mikmik_dir,
            "dup.md",
            "---\nname: dup\ndescription: from mikmik\n---\nBody.",
        );
        let agents_dir = tmp.path().join(".agents").join("skills");
        std::fs::create_dir_all(&agents_dir).unwrap();
        write_file(
            &agents_dir,
            "dup.md",
            "---\nname: dup\ndescription: from agents\n---\nBody.",
        );

        let discovered = discover_skills(tmp.path(), &crate::config::SkillsConfig::default());
        let bare = by_command(&discovered, "dup").expect("bare dup");
        assert_eq!(bare.origin, SkillOrigin::MikmikProject);
        // The project `.agents/skills/` source is scanned and stays reachable.
        let agents = by_command(&discovered, "dup@agents-project").expect("agents dup");
        assert_eq!(agents.skill.description, "from agents");
    }

    #[test]
    fn origin_label_and_tagged_description() {
        assert_eq!(SkillOrigin::MikmikProject.label(), "mikmik-project");
        assert_eq!(SkillOrigin::AgentsGlobal.label(), "agents-global");
        let resolved = ResolvedSkill {
            skill: DiscoveredSkill {
                name: "x".to_string(),
                description: "does x".to_string(),
                template: String::new(),
                source_path: PathBuf::from("x.md"),
            },
            origin: SkillOrigin::Configured,
            command_name: "x".to_string(),
        };
        assert_eq!(resolved.tagged_description(), "(configured) does x");
    }

    #[test]
    fn a_same_origin_name_clash_gets_a_numeric_suffix() {
        // Three skills, same name, same origin: the qualified `@origin` name
        // alone cannot separate them, so the extras take a `-N` suffix.
        let mk = |path: &str| DiscoveredSkill {
            name: "foo".to_string(),
            description: String::new(),
            template: String::new(),
            source_path: PathBuf::from(path),
        };
        let tagged = vec![
            (SkillOrigin::AgentsGlobal, mk("/a/foo.md")),
            (SkillOrigin::AgentsGlobal, mk("/b/foo.md")),
            (SkillOrigin::AgentsGlobal, mk("/c/foo.md")),
        ];
        let resolved = assign_command_names(tagged);
        let names: std::collections::HashSet<&str> =
            resolved.iter().map(|r| r.command_name.as_str()).collect();
        // All three are distinct and reachable.
        assert_eq!(names.len(), 3);
        assert!(names.contains("foo"));
        assert!(names.contains("foo@agents-global"));
        assert!(names.contains("foo@agents-global-2"));
    }

    #[test]
    fn a_configured_skill_path_expands_a_leading_tilde() {
        // `~/.agents/skills` must reach home, not `cwd/~/.agents/skills`, or a
        // configured global skills directory would never be found.
        let home = PathBuf::from("/home/user");
        let cwd = PathBuf::from("/work");

        assert_eq!(
            resolve_skill_path("~/.agents/skills", &cwd, Some(&home)),
            PathBuf::from("/home/user/.agents/skills")
        );
        // An absolute path is left as it is.
        assert_eq!(
            resolve_skill_path("/abs/skills", &cwd, Some(&home)),
            PathBuf::from("/abs/skills")
        );
        // Anything else is taken relative to cwd.
        assert_eq!(
            resolve_skill_path("rel/skills", &cwd, Some(&home)),
            PathBuf::from("/work/rel/skills")
        );
    }

    #[test]
    fn a_tilde_path_is_left_untouched_when_home_is_unknown() {
        // Resolving `~/x` against `cwd` would be wrong, so with no home the path
        // stays literal rather than pointing somewhere it does not belong.
        assert_eq!(
            resolve_skill_path("~/x", &PathBuf::from("/work"), None),
            PathBuf::from("~/x")
        );
    }
}
