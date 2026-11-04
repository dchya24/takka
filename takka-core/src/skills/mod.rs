//! Skill system — domain-specific instructions loaded on demand.
//!
//! Follows the [Agent Skills standard](https://agentskills.io/specification)
//! for cross-agent compatibility with pi, opencode, codex, and similar tools.
//!
//! ## Structure
//!
//! - [`SkillMetadata`]: Parsed frontmatter from a `SKILL.md` file.
//! - [`Skill`]: A discovered skill with its directory, metadata, and content.
//! - [`SkillIndex`]: Collection of all discovered skills with lookup.
//! - [`SkillLoader`]: Trait for resolving/activating skills at runtime.
//! - [`SkillTool`]: The `skill` tool registered in the tool registry.

pub mod discovery;
pub mod tool;

pub use discovery::{discover_skills, DiscoveryConfig};
pub use tool::SkillTool;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// Frontmatter metadata parsed from a `SKILL.md` file.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct SkillMetadata {
    /// Machine name: lowercase a-z, 0-9, hyphens only, 1-64 chars.
    /// Must match the parent directory name.
    pub name: String,
    /// Human-readable description (max 1024 chars).
    pub description: String,
    /// Free-form tags for query-aware candidate scoring (P2-2).
    /// Parsed from a comma-separated `tags:` frontmatter line.
    pub tags: Vec<String>,
    /// Optional semver-ish version string from the frontmatter.
    pub version: Option<String>,
}

/// A single discovered skill.
#[derive(Debug, Clone)]
pub struct Skill {
    /// Parsed frontmatter.
    pub metadata: SkillMetadata,
    /// Absolute path to the skill directory.
    pub dir: PathBuf,
    /// Full content of the SKILL.md file (frontmatter + body).
    pub content: String,
    /// Raw frontmatter string (the `---\n...\n---` block).
    pub frontmatter: String,
    /// Body after the frontmatter block.
    pub body: String,
}

impl Skill {
    pub fn name(&self) -> &str {
        &self.metadata.name
    }

    pub fn description(&self) -> &str {
        &self.metadata.description
    }

    /// Path to the SKILL.md file.
    pub fn skill_md_path(&self) -> PathBuf {
        self.dir.join("SKILL.md")
    }
}

/// Index of all discovered skills.
#[derive(Debug, Clone, Default)]
pub struct SkillIndex {
    /// name → Skill (first-found wins on collision).
    skills: HashMap<String, Skill>,
    /// Names that were skipped due to the blocklist.
    blocked: Vec<String>,
}

impl SkillIndex {
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert a skill. Returns `true` if this is the first skill with that name.
    pub fn insert(&mut self, skill: Skill) -> bool {
        let name = skill.name().to_string();
        if let std::collections::hash_map::Entry::Vacant(e) = self.skills.entry(name) {
            e.insert(skill);
            true
        } else {
            false
        }
    }

    /// Look up a skill by name (exact match).
    pub fn get(&self, name: &str) -> Option<&Skill> {
        self.skills.get(name)
    }

    /// All skills (unordered).
    pub fn all(&self) -> Vec<&Skill> {
        self.skills.values().collect()
    }

    /// Number of skills in the index.
    pub fn len(&self) -> usize {
        self.skills.len()
    }

    pub fn is_empty(&self) -> bool {
        self.skills.is_empty()
    }

    /// Names that were blocked from the index.
    pub fn blocked(&self) -> &[String] {
        &self.blocked
    }

    pub fn add_blocked(&mut self, name: String) {
        self.blocked.push(name);
    }

    /// Remove a skill by name (returns the skill if it existed).
    pub fn remove(&mut self, name: &str) -> Option<Skill> {
        self.skills.remove(name)
    }
}

impl FromIterator<Skill> for SkillIndex {
    fn from_iter<I: IntoIterator<Item = Skill>>(iter: I) -> Self {
        let mut index = SkillIndex::new();
        for skill in iter {
            index.insert(skill);
        }
        index
    }
}

// ---------------------------------------------------------------------------
// Skill Loader trait (callback pattern, following QuestionHandler precedent)
// ---------------------------------------------------------------------------

/// Trait for resolving and activating skills at runtime.
///
/// Follows the same callback pattern as [`QuestionHandler`] and
/// [`TodoChangeHandler`] — the CLI registers its own implementation and
/// the core library delegates through it.
pub trait SkillLoader: Send + Sync {
    /// Resolve a skill by name and return its full content (SKILL.md +
    /// referenced files joined into one blob), or `None` if unknown.
    fn resolve(&self, name: &str) -> Option<String>;

    /// List all available skills as `(name, description)` pairs.
    fn list(&self) -> Vec<(String, String)>;

    /// Activate a skill for the session duration. May append instructions
    /// to the system prompt / active context.
    fn activate(&self, name: &str) -> Result<String, String>;

    /// Deactivate the currently active skill.
    fn deactivate(&self);

    /// Name of the currently active skill, if any.
    fn active_skill(&self) -> Option<String>;
}

// ---------------------------------------------------------------------------
// SkillRegistry — query-aware candidate scoring (P2-2)
// ---------------------------------------------------------------------------

/// One scored candidate from [`SkillRegistry::candidates`].
#[derive(Debug, Clone, PartialEq)]
pub struct SkillCandidate {
    pub name: String,
    pub description: String,
    pub version: Option<String>,
    /// Relevance score against the query (0 = not matched).
    pub score: usize,
}

/// Query-aware ranking over a [`SkillIndex`] — the discovery half of
/// progressive loading (P2-2): metadata is always in memory, full
/// content is only loaded on activation (via the `skill` tool).
///
/// Scoring (higher = more relevant):
/// - exact name match                → +100
/// - name contains the query         → +50
/// - query matches a tag             → +30
/// - name/description word hit       → +10 each (description capped)
pub struct SkillRegistry {
    index: Arc<std::sync::RwLock<SkillIndex>>,
}

impl SkillRegistry {
    pub fn new(index: Arc<std::sync::RwLock<SkillIndex>>) -> Self {
        Self { index }
    }

    /// Build from a one-shot discovery.
    pub fn from_discovery(config: &DiscoveryConfig) -> Self {
        Self::new(Arc::new(std::sync::RwLock::new(discover_skills(config))))
    }

    /// Rank available skills against a free-text query. Empty queries
    /// return everything with score 0 (description-ordered).
    pub fn candidates(&self, query: &str) -> Vec<SkillCandidate> {
        let query = query.trim().to_lowercase();
        let index = self.index.read().unwrap();
        let mut out: Vec<SkillCandidate> = index
            .all()
            .into_iter()
            .map(|skill| {
                let name = skill.name().to_lowercase();
                let desc = skill.description().to_lowercase();
                let mut score = 0usize;

                if !query.is_empty() {
                    if name == query {
                        score += 100;
                    } else if name.contains(&query) {
                        score += 50;
                    }
                    if skill.metadata.tags.iter().any(|t| t == &query) {
                        score += 30;
                    }
                    // Word hits: every query word found in name or
                    // description contributes.
                    for word in query.split_whitespace() {
                        if name.contains(word) {
                            score += 10;
                        }
                        if desc.contains(word) {
                            score += 10;
                        }
                    }
                }

                SkillCandidate {
                    name: skill.name().to_string(),
                    description: skill.description().to_string(),
                    version: skill.metadata.version.clone(),
                    score,
                }
            })
            .collect();

        // Relevance first, then alphabetical for determinism.
        out.sort_by(|a, b| b.score.cmp(&a.score).then_with(|| a.name.cmp(&b.name)));
        out
    }
}

#[cfg(test)]
mod registry_tests {
    use super::*;

    fn make_skill(name: &str, description: &str, tags: &[&str], version: Option<&str>) -> Skill {
        Skill {
            metadata: SkillMetadata {
                name: name.to_string(),
                description: description.to_string(),
                tags: tags.iter().map(|t| t.to_string()).collect(),
                version: version.map(|v| v.to_string()),
            },
            dir: std::path::PathBuf::from("/tmp/skills").join(name),
            content: String::new(),
            frontmatter: String::new(),
            body: String::new(),
        }
    }

    fn registry(skills: Vec<Skill>) -> SkillRegistry {
        let mut index = SkillIndex::new();
        for s in skills {
            index.insert(s);
        }
        SkillRegistry::new(Arc::new(std::sync::RwLock::new(index)))
    }

    #[test]
    fn candidates_rank_exact_name_first() {
        let reg = registry(vec![
            make_skill(
                "postgres",
                "Database tuning",
                &["sql", "database"],
                Some("1.2.0"),
            ),
            make_skill("rust", "Rust development", &["rust"], None),
        ]);

        let cands = reg.candidates("postgres");
        assert_eq!(cands[0].name, "postgres");
        assert!(cands[0].score >= 100);
        assert_eq!(cands[0].version.as_deref(), Some("1.2.0"));
        assert!(cands[1].score == 0, "non-matching skill scores 0");
    }

    #[test]
    fn candidates_match_tags_and_partial_names() {
        let reg = registry(vec![
            make_skill("db-tuning", "Postgres performance", &["postgres"], None),
            make_skill("web-dev", "Frontend work", &["react"], None),
        ]);

        // Tag hit ranks db-tuning above web-dev.
        let cands = reg.candidates("postgres");
        assert_eq!(cands[0].name, "db-tuning");
        assert!(cands[0].score >= 30);

        // Partial name hit.
        let cands = reg.candidates("db");
        assert_eq!(cands[0].name, "db-tuning");
        assert!(cands[0].score >= 50);
    }

    #[test]
    fn candidates_deterministic_and_empty_query_lists_all() {
        let reg = registry(vec![
            make_skill("b", "second", &[], None),
            make_skill("a", "first", &[], None),
        ]);

        let cands = reg.candidates("");
        assert_eq!(cands.len(), 2);
        assert_eq!(cands[0].name, "a", "alphabetical tiebreak");
        assert_eq!(cands[1].name, "b");
    }

    #[test]
    fn frontmatter_parses_tags_and_version() {
        use crate::skills::discovery::__test_parse_frontmatter;
        let meta = __test_parse_frontmatter(
            "name: my-skill\ndescription: Does things\ntags: SQL, Postgres , sql\nversion: 2.1.0",
        )
        .unwrap();
        assert_eq!(meta.tags, vec!["sql", "postgres"], "deduped + lowercased");
        assert_eq!(meta.version.as_deref(), Some("2.1.0"));

        let plain = __test_parse_frontmatter("name: other\ndescription: No extras").unwrap();
        assert!(plain.tags.is_empty());
        assert!(plain.version.is_none());
    }
}
