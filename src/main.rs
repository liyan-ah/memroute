use std::{
    collections::{BTreeMap, BTreeSet, HashSet},
    fs,
    io::Write,
    path::{Component, Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand, ValueEnum};
use fs2::FileExt;
use regex::Regex;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tantivy::{
    Index, IndexReader, ReloadPolicy, TantivyDocument, Term,
    collector::TopDocs,
    query::{AllQuery, BooleanQuery, Occur, Query, QueryParser, TermQuery},
    schema::{
        FAST, Field, INDEXED, IndexRecordOption, STORED, STRING, Schema, TextFieldIndexing,
        TextOptions, Value,
    },
    tokenizer::{
        LowerCaser, NgramTokenizer, RawTokenizer, RemoveLongFilter, SimpleTokenizer, TextAnalyzer,
    },
};
use walkdir::WalkDir;

const CONFIG_FILE: &str = "memroute.toml";
const INDEX_DIR: &str = ".memroute/index";
const MANIFEST_PATH: &str = ".memroute/manifest.json";
const EVENT_LOG_PATH: &str = ".memroute/events.jsonl";
const INDEX_VERSION: u32 = 5;
const MAX_TOPICS: usize = 16;
static REQUEST_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Parser, Debug)]
#[command(
    name = "memroute",
    version,
    about = "A portable local Tantivy knowledge router for agent workspaces"
)]
struct Cli {
    /// Workspace collection root containing memroute.toml and named workspaces.
    #[arg(long, global = true)]
    root: Option<PathBuf>,

    /// Local JSONL event log. Relative paths are resolved from the collection root.
    #[arg(long, global = true, conflicts_with = "no_event_log")]
    event_log: Option<PathBuf>,

    /// Do not write local event logs. Feedback is unavailable when disabled.
    #[arg(long, global = true)]
    no_event_log: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Build a fresh local index from AGENTS.md, memory/, and task records.
    Index(IndexArgs),
    /// Search chunks and return paths, headings, and exact line ranges.
    Search(SearchArgs),
    /// Produce an L2 context plan for a task; never returns full source documents.
    Route(RouteArgs),
    /// Audit navigation references, un-routed memory files, and source freshness metadata.
    Audit(AuditArgs),
    /// Review a task record for candidate knowledge promotion; never changes source files.
    Review(ReviewArgs),
    /// Show index documents affected by a source path.
    Impact(ImpactArgs),
    /// Record how an agent or caller used the candidates from a prior route request.
    Feedback(FeedbackArgs),
    /// Analyze local execution records and report retrieval effectiveness metrics.
    Analyze(AnalyzeArgs),
}

#[derive(Args, Clone, Copy, Debug)]
struct IndexArgs {
    /// Rebuild even when source hashes match the last manifest.
    #[arg(long)]
    force: bool,
}

#[derive(Args, Debug)]
struct SearchArgs {
    /// Natural-language text, identifiers, error codes, or resource names.
    query: String,

    /// Limit results to a workspace name declared in memroute.toml.
    #[arg(long)]
    workspace: Option<String>,

    /// Limit results to selected content kinds. May be supplied more than once.
    #[arg(long = "kind", value_enum)]
    kinds: Vec<KindArg>,

    /// Match a known entity, such as namespace, topic, service, error code, or path segment.
    #[arg(long = "entity")]
    entities: Vec<String>,

    /// Match an environment, cluster, or namespace term.
    #[arg(long)]
    environment: Option<String>,

    /// Maximum number of chunks to return.
    #[arg(long, default_value_t = 8)]
    limit: usize,

    /// Produce JSON for agents and other callers.
    #[arg(long)]
    json: bool,
}

#[derive(Args, Debug)]
struct RouteArgs {
    /// Natural-language task statement or compact task title.
    query: String,

    /// Workspace that owns this task.
    #[arg(long)]
    workspace: String,

    /// Task action, such as create, investigate, migrate, validate, or retire.
    #[arg(long)]
    intent: Option<String>,

    /// Exact entities. May be supplied more than once.
    #[arg(long = "entity")]
    entities: Vec<String>,

    /// Environment, cluster, namespace, or region.
    #[arg(long)]
    environment: Option<String>,

    /// Risk level. High risk causes L3 authority entries to be returned.
    #[arg(long, value_enum, default_value_t = Risk::Normal)]
    risk: Risk,

    /// Explicitly include other named workspaces. Default scope is the owning workspace.
    #[arg(long = "include-workspace")]
    include_workspaces: Vec<String>,

    /// Maximum L2 chunks; constrained to the workspace policy range 3..=8.
    #[arg(long, default_value_t = 6)]
    limit: usize,

    /// Opaque, non-sensitive identifier used only to group local route metrics.
    #[arg(long)]
    task_id: Option<String>,

    /// Produce JSON for agents and other callers.
    #[arg(long)]
    json: bool,
}

#[derive(Args, Clone, Copy, Debug)]
struct AuditArgs {
    /// Produce JSON for agents and other callers.
    #[arg(long)]
    json: bool,
}

#[derive(Args, Debug)]
struct ReviewArgs {
    /// Workspace containing the task.
    #[arg(long)]
    workspace: String,

    /// Task name, relative to tasks/.
    #[arg(long)]
    task: String,

    /// Produce JSON for agents and other callers.
    #[arg(long)]
    json: bool,
}

#[derive(Args, Debug)]
struct ImpactArgs {
    /// Relative source path, optionally prefixed with `<workspace>/`.
    path: String,

    /// Produce JSON for agents and other callers.
    #[arg(long)]
    json: bool,
}

#[derive(Args, Debug)]
struct FeedbackArgs {
    /// Identifier returned by a prior `route` invocation.
    #[arg(long)]
    request_id: String,

    /// Opaque chunk identifier returned in `l2_context` or `l3_authorities`. Repeat as needed.
    #[arg(long = "chunk-id")]
    chunk_ids: Vec<String>,

    /// How the caller used the routed candidates.
    #[arg(long, value_enum)]
    outcome: FeedbackOutcome,

    /// Produce JSON for agents and other callers.
    #[arg(long)]
    json: bool,
}

#[derive(Args, Clone, Debug)]
struct AnalyzeArgs {
    /// Limit metrics to one workspace declared in memroute.toml.
    #[arg(long)]
    workspace: Option<String>,

    /// Produce JSON for agents and other callers.
    #[arg(long)]
    json: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "snake_case")]
enum FeedbackOutcome {
    Used,
    Background,
    Rejected,
    NeedsL3,
    Blocked,
}

impl FeedbackOutcome {
    const fn requires_chunks(self) -> bool {
        !matches!(self, Self::Blocked)
    }

    const fn as_str(self) -> &'static str {
        match self {
            Self::Used => "used",
            Self::Background => "background",
            Self::Rejected => "rejected",
            Self::NeedsL3 => "needs_l3",
            Self::Blocked => "blocked",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
enum KindArg {
    Route,
    StableFact,
    Runbook,
    TaskExperience,
    ExternalAuthority,
}

impl KindArg {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Route => "route",
            Self::StableFact => "stable_fact",
            Self::Runbook => "runbook",
            Self::TaskExperience => "task_experience",
            Self::ExternalAuthority => "external_authority",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
enum Risk {
    Low,
    Normal,
    High,
}

impl Risk {
    const fn is_high(self) -> bool {
        matches!(self, Self::High)
    }
}

#[derive(Debug, Deserialize)]
struct Config {
    #[serde(default = "default_index_dir")]
    index_dir: String,
    #[serde(default = "default_manifest_path")]
    manifest_path: String,
    #[serde(default)]
    ignore_paths: Vec<String>,
    #[serde(default)]
    workspace: Vec<WorkspaceConfig>,
}

fn default_index_dir() -> String {
    INDEX_DIR.to_owned()
}

fn default_manifest_path() -> String {
    MANIFEST_PATH.to_owned()
}

#[derive(Debug, Deserialize, Clone)]
struct WorkspaceConfig {
    name: String,
    path: String,
    #[serde(default)]
    enabled: bool,
}

#[derive(Debug, Serialize, Deserialize, Default)]
struct Manifest {
    index_version: u32,
    indexed_at_epoch_secs: u64,
    sources: BTreeMap<String, String>,
}

#[derive(Clone, Debug)]
struct EventLog {
    path: PathBuf,
}

#[derive(Debug, Serialize, Deserialize)]
struct CommandInvocationEvent {
    schema_version: u8,
    event_type: String,
    timestamp_epoch_millis: u64,
    command: String,
    duration_millis: u64,
    index_fingerprint: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    workspace: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    result_count: Option<usize>,
}

#[derive(Debug, Serialize, Deserialize)]
struct RouteInvocationEvent {
    schema_version: u8,
    event_type: String,
    timestamp_epoch_millis: u64,
    request_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    task_id: Option<String>,
    workspace: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    intent_fingerprint: Option<String>,
    risk: String,
    scopes: Vec<String>,
    duration_millis: u64,
    index_fingerprint: String,
    l2_context: Vec<EventCandidate>,
    l3_authorities: Vec<EventCandidate>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct EventCandidate {
    chunk_id: String,
    rank: usize,
    kind: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct FeedbackEvent {
    schema_version: u8,
    event_type: String,
    timestamp_epoch_millis: u64,
    request_id: String,
    outcome: FeedbackOutcome,
    chunk_ids: Vec<String>,
}

#[derive(Debug)]
enum LoggedEvent {
    CommandInvoked(CommandInvocationEvent),
    RouteInvoked(RouteInvocationEvent),
    RouteFeedback(FeedbackEvent),
}

#[derive(Debug)]
struct ParsedEventLog {
    events: Vec<LoggedEvent>,
    physical_line_count: usize,
    concatenated_event_lines: usize,
    malformed_line_count: usize,
}

#[derive(Clone, Debug)]
struct Chunk {
    workspace: String,
    kind: String,
    path: String,
    heading_path: String,
    line_start: u64,
    line_end: u64,
    title: String,
    body: String,
    entities: Vec<String>,
    environment: Vec<String>,
    lifecycle: Vec<String>,
    source_urls: Vec<String>,
    verified_at: Option<String>,
    canonical_ref: Option<String>,
}

#[derive(Clone, Debug)]
struct NavigationRow {
    topic: String,
    l2_knowledge: String,
    l3_boundary: String,
    line_start: u64,
    line_end: u64,
}

#[derive(Clone)]
struct Fields {
    id: Field,
    workspace: Field,
    kind: Field,
    path: Field,
    heading_path: Field,
    line_start: Field,
    line_end: Field,
    title: Field,
    body: Field,
    title_cjk: Field,
    body_cjk: Field,
    heading_path_cjk: Field,
    entities: Field,
    environment: Field,
    lifecycle: Field,
    source_urls: Field,
    verified_at: Field,
    canonical_ref: Field,
}

impl Fields {
    fn build_schema() -> (Schema, Self) {
        let mut builder = Schema::builder();
        let id = builder.add_text_field("id", STRING | STORED);
        let workspace = builder.add_text_field("workspace", STRING | STORED);
        let kind = builder.add_text_field("kind", STRING | STORED);
        let path = builder.add_text_field("path", STRING | STORED);
        let mixed = |stored: bool| {
            let options = TextOptions::default().set_indexing_options(
                TextFieldIndexing::default()
                    .set_tokenizer("mixed")
                    .set_index_option(IndexRecordOption::WithFreqsAndPositions),
            );
            if stored {
                options.set_stored()
            } else {
                options
            }
        };
        let code = |stored: bool| {
            let options = TextOptions::default().set_indexing_options(
                TextFieldIndexing::default()
                    .set_tokenizer("code")
                    .set_index_option(IndexRecordOption::WithFreqsAndPositions),
            );
            if stored {
                options.set_stored()
            } else {
                options
            }
        };
        let heading_path = builder.add_text_field("heading_path", mixed(true));
        let line_start = builder.add_u64_field("line_start", FAST | STORED | INDEXED);
        let line_end = builder.add_u64_field("line_end", FAST | STORED | INDEXED);
        let title = builder.add_text_field("title", mixed(true));
        let body = builder.add_text_field("body", mixed(true));
        // CJK 检索副本字段：SimpleTokenizer 会把连续中文切成单个整 token，
        // 导致「显卡驱动」无法命中「重启后显卡驱动不正常」。这里用 2-4 字符
        // n-gram 单独索引中文文本，查询时与 above 字段做 OR，英文精度不受影响。
        let cjk = |stored: bool| {
            let options = TextOptions::default().set_indexing_options(
                TextFieldIndexing::default()
                    .set_tokenizer("cjk_ngram")
                    .set_index_option(IndexRecordOption::WithFreqsAndPositions),
            );
            if stored {
                options.set_stored()
            } else {
                options
            }
        };
        let title_cjk = builder.add_text_field("title_cjk", cjk(false));
        let body_cjk = builder.add_text_field("body_cjk", cjk(false));
        let heading_path_cjk = builder.add_text_field("heading_path_cjk", cjk(false));
        let entities = builder.add_text_field("entities", code(true));
        let environment = builder.add_text_field("environment", code(true));
        let lifecycle = builder.add_text_field("lifecycle", mixed(true));
        let source_urls = builder.add_text_field("source_urls", code(true));
        let verified_at = builder.add_text_field("verified_at", STRING | STORED);
        let canonical_ref = builder.add_text_field("canonical_ref", STRING | STORED);
        let schema = builder.build();
        (
            schema,
            Self {
                id,
                workspace,
                kind,
                path,
                heading_path,
                line_start,
                line_end,
                title,
                body,
                title_cjk,
                body_cjk,
                heading_path_cjk,
                entities,
                environment,
                lifecycle,
                source_urls,
                verified_at,
                canonical_ref,
            },
        )
    }

    fn from_schema(schema: &Schema) -> Result<Self> {
        let get = |name| {
            schema
                .get_field(name)
                .map_err(|_| anyhow::anyhow!("index missing field `{name}`"))
        };
        Ok(Self {
            id: get("id")?,
            workspace: get("workspace")?,
            kind: get("kind")?,
            path: get("path")?,
            heading_path: get("heading_path")?,
            line_start: get("line_start")?,
            line_end: get("line_end")?,
            title: get("title")?,
            body: get("body")?,
            title_cjk: get("title_cjk")?,
            body_cjk: get("body_cjk")?,
            heading_path_cjk: get("heading_path_cjk")?,
            entities: get("entities")?,
            environment: get("environment")?,
            lifecycle: get("lifecycle")?,
            source_urls: get("source_urls")?,
            verified_at: get("verified_at")?,
            canonical_ref: get("canonical_ref")?,
        })
    }

    fn searchable(&self) -> Vec<Field> {
        vec![
            self.title,
            self.body,
            self.heading_path,
            self.entities,
            self.environment,
            self.lifecycle,
            self.source_urls,
        ]
    }
}

#[derive(Debug, Clone, Serialize)]
struct SearchResult {
    score: f32,
    chunk_id: String,
    workspace: String,
    kind: String,
    path: String,
    heading_path: String,
    line_start: u64,
    line_end: u64,
    title: String,
    entities: Vec<String>,
    environment: Vec<String>,
    lifecycle: Vec<String>,
    source_urls: Vec<String>,
    verified_at: Option<String>,
    canonical_ref: Option<String>,
    excerpt: String,
}

#[derive(Debug, Serialize)]
struct SearchOutput {
    query: String,
    results: Vec<SearchResult>,
}

#[derive(Debug, Serialize)]
struct RouteOutput {
    #[serde(skip_serializing_if = "Option::is_none")]
    request_id: Option<String>,
    task: TaskProfile,
    route_matches: Vec<SearchResult>,
    l2_context: Vec<SearchResult>,
    l3_authorities: Vec<SearchResult>,
    notes: Vec<String>,
}

#[derive(Debug, Serialize)]
struct TaskProfile {
    workspace: String,
    query: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    task_id: Option<String>,
    intent: Option<String>,
    entities: Vec<String>,
    environment: Option<String>,
    risk: String,
    scopes: Vec<String>,
}

#[derive(Debug, Serialize)]
struct AuditOutput {
    indexed_chunks: usize,
    indexed_sources: usize,
    navigation_references_checked: usize,
    broken_navigation_references: Vec<AuditIssue>,
    un_routed_memory_files: Vec<String>,
    memory_file_count: BTreeMap<String, usize>,
    warnings: Vec<String>,
}

#[derive(Debug, Serialize)]
struct AuditIssue {
    workspace: String,
    source_path: String,
    line: u64,
    reference: String,
    message: String,
}

#[derive(Debug, Serialize)]
struct ReviewOutput {
    workspace: String,
    task: String,
    task_path: String,
    sources: Vec<String>,
    promotion_candidates: Vec<PromotionCandidate>,
    warnings: Vec<String>,
}

#[derive(Debug, Serialize)]
struct PromotionCandidate {
    heading_path: String,
    line_start: u64,
    line_end: u64,
    reason: String,
    suggested_kind: String,
    existing_related_knowledge: Vec<SearchResult>,
}

#[derive(Debug, Serialize)]
struct ImpactOutput {
    requested_path: String,
    affected_chunks: Vec<SearchResult>,
    related_navigation: Vec<SearchResult>,
    notes: Vec<String>,
}

#[derive(Debug, Serialize)]
struct FeedbackOutput {
    request_id: String,
    outcome: FeedbackOutcome,
    accepted_chunk_ids: Vec<String>,
}

#[derive(Debug, Serialize)]
struct AnalysisOutput {
    data_quality: AnalysisDataQuality,
    metrics: AnalysisMetrics,
    conclusion: EffectivenessConclusion,
}

#[derive(Debug, Serialize)]
struct AnalysisDataQuality {
    event_count: usize,
    physical_line_count: usize,
    concatenated_event_lines: usize,
    malformed_line_count: usize,
}

#[derive(Debug, Serialize)]
struct AnalysisMetrics {
    command_invocations: BTreeMap<String, usize>,
    command_duration_millis: BTreeMap<String, DurationSummary>,
    route_count: usize,
    routes_by_workspace: BTreeMap<String, usize>,
    routes_by_risk: BTreeMap<String, usize>,
    routes_with_l2_context: usize,
    l2_route_coverage_percent: Option<f64>,
    route_duration_millis: Option<DurationSummary>,
    high_risk_route_count: usize,
    high_risk_routes_with_l3: usize,
    high_risk_l3_coverage_percent: Option<f64>,
    l2_candidate_count: usize,
    l3_candidate_count: usize,
    routes_with_feedback: usize,
    routes_without_feedback: usize,
    feedback_route_coverage_percent: Option<f64>,
    feedback_event_count: usize,
    feedback_chunk_count: usize,
    judged_candidate_count: usize,
    used_candidate_count: usize,
    used_candidate_rate_among_judged_percent: Option<f64>,
    feedback_outcomes: BTreeMap<String, usize>,
    used_candidates_by_kind: BTreeMap<String, usize>,
    orphan_feedback_count: usize,
}

#[derive(Debug, Serialize)]
struct DurationSummary {
    count: usize,
    min: u64,
    max: u64,
    average: f64,
}

#[derive(Debug, Serialize)]
struct EffectivenessConclusion {
    status: String,
    confidence: String,
    findings: Vec<String>,
    recommendations: Vec<String>,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let root = canonical_or_original(cli.root.clone().unwrap_or(std::env::current_dir()?))?;
    let config = load_config(&root)?;
    let event_log = event_log_for_cli(&root, &cli);

    match cli.command {
        Command::Index(args) => index_command(&root, &config, args, event_log.as_ref()),
        Command::Search(args) => search_command(&root, &config, args, event_log.as_ref()),
        Command::Route(args) => route_command(&root, &config, args, event_log.as_ref()),
        Command::Audit(args) => audit_command(&root, &config, args, event_log.as_ref()),
        Command::Review(args) => review_command(&root, &config, args, event_log.as_ref()),
        Command::Impact(args) => impact_command(&root, &config, args, event_log.as_ref()),
        Command::Feedback(args) => feedback_command(args, event_log.as_ref()),
        Command::Analyze(args) => analyze_command(&config, &args, event_log.as_ref()),
    }
}

fn canonical_or_original(path: PathBuf) -> Result<PathBuf> {
    if !path.exists() {
        bail!("workspace root does not exist: {}", path.display());
    }
    Ok(fs::canonicalize(&path).unwrap_or(path))
}

fn load_config(root: &Path) -> Result<Config> {
    let config_path = root.join(CONFIG_FILE);
    let raw = fs::read_to_string(&config_path)
        .with_context(|| format!("read configuration {}", config_path.display()))?;
    let config: Config = toml::from_str(&raw).context("parse memroute.toml")?;
    if config.workspace.is_empty() {
        bail!("memroute.toml must declare at least one [[workspace]] entry");
    }
    let mut names = HashSet::new();
    for workspace in &config.workspace {
        if workspace.name.trim().is_empty() || workspace.path.trim().is_empty() {
            bail!("workspace name and path must not be empty");
        }
        if !names.insert(workspace.name.clone()) {
            bail!("duplicate workspace name `{}`", workspace.name);
        }
        let workspace_root = root.join(&workspace.path);
        if !workspace_root.is_dir() {
            bail!(
                "workspace `{}` path is not a directory: {}",
                workspace.name,
                workspace_root.display()
            );
        }
    }
    Ok(config)
}

fn enabled_workspaces(config: &Config) -> impl Iterator<Item = &WorkspaceConfig> {
    config
        .workspace
        .iter()
        .filter(|workspace| workspace.enabled)
}

fn index_command(
    root: &Path,
    config: &Config,
    args: IndexArgs,
    event_log: Option<&EventLog>,
) -> Result<()> {
    let started = Instant::now();
    let sources = collect_sources(root, config)?;
    let source_hashes = source_hashes(root, &sources)?;
    let manifest_path = root.join(&config.manifest_path);
    let existing_manifest = read_manifest(&manifest_path)?;
    let index_path = root.join(&config.index_dir);

    if !args.force
        && existing_manifest.index_version == INDEX_VERSION
        && existing_manifest.sources == source_hashes
        && index_path.exists()
    {
        emit_command_invocation(
            event_log,
            "index",
            started,
            &existing_manifest,
            None,
            Some(source_hashes.len()),
        )?;
        println!("index is current: {} sources", source_hashes.len());
        return Ok(());
    }

    let mut chunks = Vec::new();
    for source in &sources {
        let workspace = workspace_for_path(root, config, source)?;
        let relative = relative_path(root.join(&workspace.path), source)?;
        let mut file_chunks = parse_source(&workspace.name, &relative, source)?;
        file_chunks.extend(external_authorities_from_chunks(&file_chunks));
        chunks.extend(file_chunks);
    }

    let temp_index_path = root.join(".memroute/index.building");
    if temp_index_path.exists() {
        fs::remove_dir_all(&temp_index_path).with_context(|| {
            format!("remove stale temporary index {}", temp_index_path.display())
        })?;
    }
    fs::create_dir_all(&temp_index_path)
        .with_context(|| format!("create temporary index {}", temp_index_path.display()))?;

    build_index(&temp_index_path, &chunks)?;
    let parent = index_path
        .parent()
        .context("index directory must have a parent")?;
    fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    if index_path.exists() {
        fs::remove_dir_all(&index_path)
            .with_context(|| format!("remove old index {}", index_path.display()))?;
    }
    fs::rename(&temp_index_path, &index_path).with_context(|| {
        format!(
            "atomically install index {} -> {}",
            temp_index_path.display(),
            index_path.display()
        )
    })?;

    let manifest = Manifest {
        index_version: INDEX_VERSION,
        indexed_at_epoch_secs: epoch_seconds(),
        sources: source_hashes,
    };
    write_manifest(&manifest_path, &manifest)?;

    let by_kind = chunks
        .iter()
        .fold(BTreeMap::<String, usize>::new(), |mut counts, chunk| {
            *counts.entry(chunk.kind.clone()).or_default() += 1;
            counts
        });
    println!(
        "indexed {} chunks from {} sources",
        chunks.len(),
        sources.len()
    );
    for (kind, count) in by_kind {
        println!("  {kind}: {count}");
    }
    emit_command_invocation(
        event_log,
        "index",
        started,
        &manifest,
        None,
        Some(sources.len()),
    )
}

fn search_command(
    root: &Path,
    config: &Config,
    args: SearchArgs,
    event_log: Option<&EventLog>,
) -> Result<()> {
    let started = Instant::now();
    ensure_index(root, config)?;
    let index = open_index(root, config)?;
    let fields = Fields::from_schema(&index.schema())?;
    let results = search_index(
        &index,
        &fields,
        &args.query,
        args.workspace.as_deref(),
        &args
            .kinds
            .iter()
            .map(|kind| kind.as_str().to_owned())
            .collect::<Vec<_>>(),
        &args.entities,
        args.environment.as_deref(),
        args.limit,
    )?;
    let output = SearchOutput {
        query: args.query,
        results,
    };
    let manifest = read_manifest(&root.join(&config.manifest_path))?;
    emit_command_invocation(
        event_log,
        "search",
        started,
        &manifest,
        args.workspace,
        Some(output.results.len()),
    )?;
    print_output(&output, args.json)
}

fn route_command(
    root: &Path,
    config: &Config,
    args: RouteArgs,
    event_log: Option<&EventLog>,
) -> Result<()> {
    let started = Instant::now();
    if !(3..=8).contains(&args.limit) {
        bail!("route --limit must be between 3 and 8");
    }
    ensure_index(root, config)?;
    let scopes = route_scopes(config, &args)?;
    let index = open_index(root, config)?;
    let fields = Fields::from_schema(&index.schema())?;
    let combined_query = build_route_query(&args);
    let route_matches = search_scoped(
        &index,
        &fields,
        &combined_query,
        &scopes,
        &["route".to_owned()],
        &[],
        None,
        MAX_TOPICS,
    )?;
    let preferred_paths = route_matches
        .iter()
        .flat_map(|result| extract_markdown_paths_from_source(root, config, result))
        .collect::<HashSet<_>>();
    let candidates = search_scoped(
        &index,
        &fields,
        &combined_query,
        &scopes,
        &[
            "stable_fact".to_owned(),
            "runbook".to_owned(),
            "task_experience".to_owned(),
        ],
        &args.entities,
        None,
        args.limit.saturating_mul(8),
    )?;
    let l2_context =
        rank_route_candidates(candidates, &preferred_paths, &args.workspace, args.limit);
    let l3_authorities = if args.risk.is_high() {
        search_l3_authorities(
            &index,
            &fields,
            &combined_query,
            &scopes,
            &route_matches,
            MAX_TOPICS,
        )?
    } else {
        Vec::new()
    };
    let notes = route_notes(&args, &scopes, &l3_authorities);

    let request_id = event_log.map(|_| new_request_id());
    let output = RouteOutput {
        request_id: request_id.clone(),
        task: TaskProfile {
            workspace: args.workspace,
            query: args.query,
            task_id: args.task_id,
            intent: args.intent,
            entities: args.entities,
            environment: args.environment,
            risk: format!("{:?}", args.risk).to_lowercase(),
            scopes,
        },
        route_matches,
        l2_context,
        l3_authorities,
        notes,
    };
    if let (Some(event_log), Some(request_id)) = (event_log, request_id) {
        let manifest = read_manifest(&root.join(&config.manifest_path))?;
        event_log.append(&RouteInvocationEvent {
            schema_version: 1,
            event_type: "route_invoked".to_owned(),
            timestamp_epoch_millis: epoch_millis(),
            request_id,
            task_id: output
                .task
                .task_id
                .as_ref()
                .map(|value| value_fingerprint(value)),
            workspace: output.task.workspace.clone(),
            intent_fingerprint: output
                .task
                .intent
                .as_ref()
                .map(|value| value_fingerprint(value)),
            risk: output.task.risk.clone(),
            scopes: output.task.scopes.clone(),
            duration_millis: elapsed_millis(started),
            index_fingerprint: manifest_fingerprint(&manifest),
            l2_context: event_candidates(&output.l2_context),
            l3_authorities: event_candidates(&output.l3_authorities),
        })?;
    }
    print_output(&output, args.json)
}

fn route_scopes(config: &Config, args: &RouteArgs) -> Result<Vec<String>> {
    require_workspace(config, &args.workspace)?;
    let mut scopes = vec![args.workspace.clone()];
    for workspace in &args.include_workspaces {
        require_workspace(config, workspace)?;
        if !scopes.contains(workspace) {
            scopes.push(workspace.clone());
        }
    }
    Ok(scopes)
}

fn route_notes(
    args: &RouteArgs,
    scopes: &[String],
    l3_authorities: &[SearchResult],
) -> Vec<String> {
    let mut notes = vec![
        "MemRoute returns routing evidence only. Read the cited source section before treating a result as fact."
            .to_owned(),
        format!(
            "L2 is limited to {} section(s); stable facts and runbooks rank ahead of task experience.",
            args.limit
        ),
    ];
    if args.risk.is_high() {
        notes.push(
            "High-risk route: verify the returned L3 authority entries against the original system before changing production, permissions, deletion, migration, or data state."
                .to_owned(),
        );
        if l3_authorities.is_empty() {
            notes.push(
                "No explicit external authority was indexed. Use the owning workspace AGENTS.md and source documents to locate an original authority before acting."
                    .to_owned(),
            );
        }
    }
    if scopes.len() > 1 {
        notes.push(format!(
            "Search scope expanded explicitly to: {}.",
            scopes.join(", ")
        ));
    }
    notes
}

fn audit_command(
    root: &Path,
    config: &Config,
    args: AuditArgs,
    event_log: Option<&EventLog>,
) -> Result<()> {
    let started = Instant::now();
    ensure_index(root, config)?;
    let index = open_index(root, config)?;
    let fields = Fields::from_schema(&index.schema())?;
    let chunks = all_documents(&index, &fields)?;
    let sources = collect_sources(root, config)?;

    let mut issues = Vec::new();
    let mut referenced_memory = BTreeSet::new();
    for workspace in enabled_workspaces(config) {
        let workspace_root = root.join(&workspace.path);
        let agents_path = workspace_root.join("AGENTS.md");
        if !agents_path.is_file() {
            issues.push(AuditIssue {
                workspace: workspace.name.clone(),
                source_path: "AGENTS.md".to_owned(),
                line: 0,
                reference: String::new(),
                message: "missing AGENTS.md".to_owned(),
            });
            continue;
        }
        let rows = parse_navigation_rows(&workspace.name, &agents_path)?;
        for row in rows {
            for reference in extract_markdown_paths(&row.l2_knowledge) {
                let normalized = normalize_reference_path(&reference);
                if normalized.starts_with("memory/") {
                    referenced_memory.insert(format!("{}/{}", workspace.name, normalized));
                }
                if normalized.starts_with("tasks/") || normalized.starts_with("memory/") {
                    let target = workspace_root.join(&normalized);
                    if !target.is_file() {
                        issues.push(AuditIssue {
                            workspace: workspace.name.clone(),
                            source_path: "AGENTS.md".to_owned(),
                            line: row.line_start,
                            reference: normalized,
                            message: "knowledge-navigation local reference does not exist"
                                .to_owned(),
                        });
                    }
                }
            }
        }
    }

    let mut memory_file_count = BTreeMap::new();
    let mut un_routed_memory_files = Vec::new();
    for workspace in enabled_workspaces(config) {
        let workspace_root = root.join(&workspace.path);
        let memory_root = workspace_root.join("memory");
        let mut count = 0;
        if memory_root.is_dir() {
            for entry in WalkDir::new(&memory_root)
                .into_iter()
                .filter_map(Result::ok)
            {
                if entry.file_type().is_file() && is_markdown(entry.path()) {
                    count += 1;
                    let relative = relative_path(&workspace_root, entry.path())?;
                    let key = format!("{}/{}", workspace.name, relative);
                    if !referenced_memory.contains(&key) {
                        un_routed_memory_files.push(key);
                    }
                }
            }
        }
        memory_file_count.insert(workspace.name.clone(), count);
    }

    let mut warnings = Vec::new();
    for (workspace, count) in &memory_file_count {
        if *count > 30 {
            warnings.push(format!(
                "workspace `{workspace}` has {count} memory files; reassess whether small-scale retrieval remains sufficient"
            ));
        }
    }
    if !un_routed_memory_files.is_empty() {
        warnings.push(
            "Un-routed memory files are not necessarily invalid; review whether they need an explicit AGENTS.md knowledge-navigation entry."
                .to_owned(),
        );
    }

    let output = AuditOutput {
        indexed_chunks: chunks.len(),
        indexed_sources: sources.len(),
        navigation_references_checked: referenced_memory.len(),
        broken_navigation_references: issues,
        un_routed_memory_files,
        memory_file_count,
        warnings,
    };
    let manifest = read_manifest(&root.join(&config.manifest_path))?;
    emit_command_invocation(
        event_log,
        "audit",
        started,
        &manifest,
        None,
        Some(output.indexed_chunks),
    )?;
    print_output(&output, args.json)
}

fn review_command(
    root: &Path,
    config: &Config,
    args: ReviewArgs,
    event_log: Option<&EventLog>,
) -> Result<()> {
    let started = Instant::now();
    ensure_index(root, config)?;
    let workspace = require_workspace(config, &args.workspace)?;
    let workspace_root = root.join(&workspace.path);
    let task_path = workspace_root
        .join("tasks")
        .join(&args.task)
        .join("task.md");
    if !task_path.is_file() {
        bail!("task record does not exist: {}", task_path.display());
    }
    let relative_task_path = relative_path(&workspace_root, &task_path)?;
    let chunks = parse_markdown_chunks(
        &workspace.name,
        "task_experience",
        &relative_task_path,
        &task_path,
    )?;
    let index = open_index(root, config)?;
    let fields = Fields::from_schema(&index.schema())?;

    let mut candidates = Vec::new();
    for chunk in chunks {
        if !is_promotion_candidate(&chunk) {
            continue;
        }
        let query = format!("{} {}", chunk.title, chunk.entities.join(" "));
        let related = search_index(
            &index,
            &fields,
            &query,
            Some(&workspace.name),
            &["stable_fact".to_owned(), "runbook".to_owned()],
            &chunk.entities,
            chunk.environment.first().map(String::as_str),
            3,
        )?;
        let reason = promotion_reason(&chunk);
        let suggested_kind = suggest_kind(&chunk);
        candidates.push(PromotionCandidate {
            heading_path: chunk.heading_path,
            line_start: chunk.line_start,
            line_end: chunk.line_end,
            reason,
            suggested_kind,
            existing_related_knowledge: related,
        });
    }

    let content = fs::read_to_string(&task_path)?;
    let sources = extract_urls(&content);
    let mut warnings = vec![
        "Review is advisory. Do not promote a task observation to memory until it is reusable, confirmed, and traceable to an authority."
            .to_owned(),
    ];
    if sources.is_empty() {
        warnings.push(
            "No external URLs were found in task.md; if a candidate is promoted, add a traceable source or explicitly mark it pending verification."
                .to_owned(),
        );
    }
    if candidates.is_empty() {
        warnings.push(
            "No section matched the conservative promotion heuristic. This does not imply the task has no reusable conclusions."
                .to_owned(),
        );
    }
    let output = ReviewOutput {
        workspace: workspace.name.clone(),
        task: args.task,
        task_path: relative_task_path,
        sources,
        promotion_candidates: candidates,
        warnings,
    };
    let manifest = read_manifest(&root.join(&config.manifest_path))?;
    emit_command_invocation(
        event_log,
        "review",
        started,
        &manifest,
        Some(output.workspace.clone()),
        Some(output.promotion_candidates.len()),
    )?;
    print_output(&output, args.json)
}

fn impact_command(
    root: &Path,
    config: &Config,
    args: ImpactArgs,
    event_log: Option<&EventLog>,
) -> Result<()> {
    let started = Instant::now();
    ensure_index(root, config)?;
    let index = open_index(root, config)?;
    let fields = Fields::from_schema(&index.schema())?;
    let requested = normalize_reference_path(&args.path);
    let (workspace_filter, path_filter) = split_workspace_path(config, &requested);
    let affected_chunks = search_exact_path(
        &index,
        &fields,
        workspace_filter.as_deref(),
        &path_filter,
        MAX_TOPICS,
    )?;

    let name_terms = Path::new(&path_filter)
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or(&path_filter)
        .replace(['-', '_', '.'], " ");
    let related_navigation = search_scoped(
        &index,
        &fields,
        &name_terms,
        &workspace_filter.clone().into_iter().collect::<Vec<_>>(),
        &["route".to_owned()],
        &[],
        None,
        MAX_TOPICS,
    )?;

    let mut notes = vec![
        "Impact output identifies indexed chunks and likely route entries only; verify source links and semantic relevance before changing navigation."
            .to_owned(),
    ];
    if affected_chunks.is_empty() {
        notes.push(
            "No indexed chunk has this exact path. Run `memroute index` after adding a source, or check the workspace prefix."
                .to_owned(),
        );
    }
    let output = ImpactOutput {
        requested_path: args.path,
        affected_chunks,
        related_navigation,
        notes,
    };
    let manifest = read_manifest(&root.join(&config.manifest_path))?;
    emit_command_invocation(
        event_log,
        "impact",
        started,
        &manifest,
        workspace_filter,
        Some(output.affected_chunks.len()),
    )?;
    print_output(&output, args.json)
}

fn analyze_command(
    config: &Config,
    args: &AnalyzeArgs,
    event_log: Option<&EventLog>,
) -> Result<()> {
    let event_log = event_log.context(
        "analyze requires the local event log; omit `--no-event-log` and use the event log that contains route and feedback records",
    )?;
    if let Some(workspace) = args.workspace.as_deref() {
        require_workspace(config, workspace)?;
    }
    let parsed = parse_event_log(&event_log.path)?;
    let output = analyze_events(parsed, args.workspace.as_deref());
    print_output(&output, args.json)
}

fn parse_event_log(path: &Path) -> Result<ParsedEventLog> {
    if !path.exists() {
        return Ok(ParsedEventLog {
            events: Vec::new(),
            physical_line_count: 0,
            concatenated_event_lines: 0,
            malformed_line_count: 0,
        });
    }
    let raw =
        fs::read_to_string(path).with_context(|| format!("read event log {}", path.display()))?;
    let mut events = Vec::new();
    let mut physical_line_count = 0;
    let mut concatenated_event_lines = 0;
    let mut malformed_line_count = 0;
    for line in raw.lines().filter(|line| !line.trim().is_empty()) {
        physical_line_count += 1;
        let deserializer =
            serde_json::Deserializer::from_str(line).into_iter::<serde_json::Value>();
        let mut parsed_on_line = 0;
        for value in deserializer {
            let Ok(value) = value else {
                malformed_line_count += 1;
                break;
            };
            let event = match parse_logged_event(value) {
                Ok(Some(event)) => event,
                Ok(None) => continue,
                Err(_) => {
                    malformed_line_count += 1;
                    continue;
                }
            };
            parsed_on_line += 1;
            events.push(event);
        }
        if parsed_on_line > 1 {
            concatenated_event_lines += 1;
        }
    }
    Ok(ParsedEventLog {
        events,
        physical_line_count,
        concatenated_event_lines,
        malformed_line_count,
    })
}

fn parse_logged_event(value: serde_json::Value) -> Result<Option<LoggedEvent>> {
    match value.get("event_type").and_then(serde_json::Value::as_str) {
        Some("command_invoked") => serde_json::from_value(value)
            .map(LoggedEvent::CommandInvoked)
            .map(Some)
            .context("parse command invocation event"),
        Some("route_invoked") => serde_json::from_value(value)
            .map(LoggedEvent::RouteInvoked)
            .map(Some)
            .context("parse route invocation event"),
        Some("route_feedback") => serde_json::from_value(value)
            .map(LoggedEvent::RouteFeedback)
            .map(Some)
            .context("parse route feedback event"),
        _ => Ok(None),
    }
}

fn analyze_events(parsed: ParsedEventLog, workspace_filter: Option<&str>) -> AnalysisOutput {
    let ParsedEventLog {
        events,
        physical_line_count,
        concatenated_event_lines,
        malformed_line_count,
    } = parsed;
    let event_count = events.len();
    let all_route_ids = events
        .iter()
        .filter_map(|event| match event {
            LoggedEvent::RouteInvoked(route) => Some(route.request_id.as_str()),
            _ => None,
        })
        .collect::<HashSet<_>>();
    let routes = events
        .iter()
        .filter_map(|event| match event {
            LoggedEvent::RouteInvoked(route) => Some(route),
            _ => None,
        })
        .filter(|route| workspace_filter.is_none_or(|workspace| route.workspace == workspace))
        .collect::<Vec<_>>();
    let route_ids = routes
        .iter()
        .map(|route| route.request_id.as_str())
        .collect::<HashSet<_>>();

    let (command_invocations, command_duration_millis) =
        summarize_command_events(&events, workspace_filter);
    let route_summary = summarize_route_events(&routes);
    let feedback_summary =
        summarize_feedback_events(&events, &all_route_ids, &route_ids, &route_summary);

    let routes_without_feedback = routes
        .len()
        .saturating_sub(feedback_summary.routes_with_feedback.len());
    let route_durations = routes
        .iter()
        .map(|route| route.duration_millis)
        .collect::<Vec<_>>();
    let metrics = AnalysisMetrics {
        command_invocations,
        command_duration_millis,
        route_count: routes.len(),
        routes_by_workspace: route_summary.routes_by_workspace,
        routes_by_risk: route_summary.routes_by_risk,
        routes_with_l2_context: route_summary.routes_with_l2_context,
        l2_route_coverage_percent: percentage(route_summary.routes_with_l2_context, routes.len()),
        route_duration_millis: duration_summary(&route_durations),
        high_risk_route_count: route_summary.high_risk_route_count,
        high_risk_routes_with_l3: route_summary.high_risk_routes_with_l3,
        high_risk_l3_coverage_percent: percentage(
            route_summary.high_risk_routes_with_l3,
            route_summary.high_risk_route_count,
        ),
        l2_candidate_count: route_summary.l2_candidate_count,
        l3_candidate_count: route_summary.l3_candidate_count,
        routes_with_feedback: feedback_summary.routes_with_feedback.len(),
        routes_without_feedback,
        feedback_route_coverage_percent: percentage(
            feedback_summary.routes_with_feedback.len(),
            routes.len(),
        ),
        feedback_event_count: feedback_summary.feedback_event_count,
        feedback_chunk_count: feedback_summary.feedback_chunk_count,
        judged_candidate_count: feedback_summary.judged_candidate_count,
        used_candidate_count: feedback_summary.used_candidate_count,
        used_candidate_rate_among_judged_percent: percentage(
            feedback_summary.used_candidate_count,
            feedback_summary.judged_candidate_count,
        ),
        feedback_outcomes: feedback_summary.feedback_outcomes,
        used_candidates_by_kind: feedback_summary.used_candidates_by_kind,
        orphan_feedback_count: feedback_summary.orphan_feedback_count,
    };
    AnalysisOutput {
        data_quality: AnalysisDataQuality {
            event_count,
            physical_line_count,
            concatenated_event_lines,
            malformed_line_count,
        },
        conclusion: effectiveness_conclusion(&metrics, workspace_filter, malformed_line_count),
        metrics,
    }
}

fn summarize_command_events(
    events: &[LoggedEvent],
    workspace_filter: Option<&str>,
) -> (BTreeMap<String, usize>, BTreeMap<String, DurationSummary>) {
    let mut command_invocations = BTreeMap::new();
    let mut command_durations = BTreeMap::<String, Vec<u64>>::new();
    for command in events.iter().filter_map(|event| match event {
        LoggedEvent::CommandInvoked(command)
            if workspace_filter
                .is_none_or(|workspace| command.workspace.as_deref() == Some(workspace)) =>
        {
            Some(command)
        }
        _ => None,
    }) {
        *command_invocations
            .entry(command.command.clone())
            .or_default() += 1;
        command_durations
            .entry(command.command.clone())
            .or_default()
            .push(command.duration_millis);
    }
    let command_duration_millis = command_durations
        .into_iter()
        .map(|(command, durations)| {
            let sum = durations.iter().sum::<u64>();
            let count = durations.len();
            (
                command,
                DurationSummary {
                    count,
                    min: *durations.iter().min().unwrap_or(&0),
                    max: *durations.iter().max().unwrap_or(&0),
                    average: round_percent(as_f64(sum) / as_f64(count)),
                },
            )
        })
        .collect();
    (command_invocations, command_duration_millis)
}

#[derive(Default)]
struct RouteSummary {
    routes_by_workspace: BTreeMap<String, usize>,
    routes_by_risk: BTreeMap<String, usize>,
    candidate_kinds: BTreeMap<(String, String), String>,
    routes_with_l2_context: usize,
    high_risk_route_count: usize,
    high_risk_routes_with_l3: usize,
    l2_candidate_count: usize,
    l3_candidate_count: usize,
}

fn summarize_route_events(routes: &[&RouteInvocationEvent]) -> RouteSummary {
    let mut summary = RouteSummary::default();
    for route in routes {
        *summary
            .routes_by_workspace
            .entry(route.workspace.clone())
            .or_default() += 1;
        *summary
            .routes_by_risk
            .entry(route.risk.clone())
            .or_default() += 1;
        if !route.l2_context.is_empty() {
            summary.routes_with_l2_context += 1;
        }
        if route.risk == "high" {
            summary.high_risk_route_count += 1;
            if !route.l3_authorities.is_empty() {
                summary.high_risk_routes_with_l3 += 1;
            }
        }
        summary.l2_candidate_count += route.l2_context.len();
        summary.l3_candidate_count += route.l3_authorities.len();
        for candidate in route.l2_context.iter().chain(&route.l3_authorities) {
            summary.candidate_kinds.insert(
                (route.request_id.clone(), candidate.chunk_id.clone()),
                candidate.kind.clone(),
            );
        }
    }
    summary
}

#[derive(Default)]
struct FeedbackSummary {
    routes_with_feedback: HashSet<String>,
    feedback_outcomes: BTreeMap<String, usize>,
    used_candidates_by_kind: BTreeMap<String, usize>,
    feedback_event_count: usize,
    feedback_chunk_count: usize,
    judged_candidate_count: usize,
    used_candidate_count: usize,
    orphan_feedback_count: usize,
}

fn summarize_feedback_events(
    events: &[LoggedEvent],
    all_route_ids: &HashSet<&str>,
    route_ids: &HashSet<&str>,
    route_summary: &RouteSummary,
) -> FeedbackSummary {
    let mut summary = FeedbackSummary::default();
    let mut candidate_outcomes: BTreeMap<(String, String), FeedbackOutcome> = BTreeMap::new();
    for feedback in events.iter().filter_map(|event| match event {
        LoggedEvent::RouteFeedback(feedback) => Some(feedback),
        _ => None,
    }) {
        if !all_route_ids.contains(feedback.request_id.as_str()) {
            summary.orphan_feedback_count += feedback.chunk_ids.len();
            continue;
        }
        if !route_ids.contains(feedback.request_id.as_str()) {
            continue;
        }
        summary
            .routes_with_feedback
            .insert(feedback.request_id.clone());
        summary.feedback_event_count += 1;
        *summary
            .feedback_outcomes
            .entry(feedback.outcome.as_str().to_owned())
            .or_default() += 1;
        summary.feedback_chunk_count += feedback.chunk_ids.len();
        for chunk_id in &feedback.chunk_ids {
            if route_summary
                .candidate_kinds
                .contains_key(&(feedback.request_id.clone(), chunk_id.clone()))
            {
                candidate_outcomes.insert(
                    (feedback.request_id.clone(), chunk_id.clone()),
                    feedback.outcome,
                );
            } else {
                summary.orphan_feedback_count += 1;
            }
        }
    }
    for ((request_id, chunk_id), outcome) in &candidate_outcomes {
        if *outcome == FeedbackOutcome::Used
            && let Some(kind) = route_summary
                .candidate_kinds
                .get(&(request_id.clone(), chunk_id.clone()))
        {
            *summary
                .used_candidates_by_kind
                .entry(kind.clone())
                .or_default() += 1;
            summary.used_candidate_count += 1;
        }
    }
    summary.judged_candidate_count = candidate_outcomes.len();
    summary
}

fn effectiveness_conclusion(
    metrics: &AnalysisMetrics,
    workspace_filter: Option<&str>,
    malformed_line_count: usize,
) -> EffectivenessConclusion {
    let scope = workspace_filter.map_or_else(
        || "collection".to_owned(),
        |workspace| format!("workspace `{workspace}`"),
    );
    if metrics.route_count == 0 {
        return EffectivenessConclusion {
            status: "insufficient_data".to_owned(),
            confidence: "none".to_owned(),
            findings: vec![format!("No route events are available for the selected {scope}.")],
            recommendations: vec![
                "Run route with local event logging enabled, then record feedback after judging candidates."
                    .to_owned(),
            ],
        };
    }

    let coverage = metrics.feedback_route_coverage_percent.unwrap_or_default();
    let mut findings = vec![format!(
        "{} of {} route request(s) have feedback coverage ({coverage:.1}%).",
        metrics.routes_with_feedback, metrics.route_count
    )];
    let mut recommendations = Vec::new();
    if metrics.high_risk_route_count > 0 {
        findings.push(format!(
            "{} of {} high-risk route request(s) returned L3 authority candidates.",
            metrics.high_risk_routes_with_l3, metrics.high_risk_route_count
        ));
    }
    if malformed_line_count > 0 {
        findings.push(format!(
            "{malformed_line_count} event-log record(s) were malformed and excluded from metrics."
        ));
        recommendations.push(
            "Repair or rotate the local event log before using the result for a longitudinal decision."
                .to_owned(),
        );
    }
    if metrics.orphan_feedback_count > 0 {
        findings.push(format!(
            "{} feedback chunk reference(s) could not be matched to the selected route events.",
            metrics.orphan_feedback_count
        ));
        recommendations.push(
            "Use the request_id and chunk_id emitted by the same route invocation for feedback."
                .to_owned(),
        );
    }
    if metrics.judged_candidate_count == 0 {
        recommendations.push(
            "Record used, background, rejected, or needs-l3 outcomes for every candidate actually judged."
                .to_owned(),
        );
    } else if !metrics.feedback_outcomes.contains_key("background")
        && !metrics.feedback_outcomes.contains_key("rejected")
        && !metrics.feedback_outcomes.contains_key("needs_l3")
    {
        recommendations.push(
            "Record background, rejected, and needs-l3 outcomes when applicable; used-only feedback cannot measure ranking precision."
                .to_owned(),
        );
    }
    if metrics.routes_without_feedback > 0 {
        recommendations.push(
            "Close feedback for route requests whose candidates were actually judged.".to_owned(),
        );
    }
    if metrics.l2_candidate_count == 0 {
        recommendations.push(
            "If L2 context remains empty for recurring work, promote confirmed and traceable task conclusions into memory."
                .to_owned(),
        );
    }

    let confidence = if metrics.judged_candidate_count >= 30 && coverage >= 80.0 {
        "high"
    } else if metrics.judged_candidate_count >= 10 && coverage >= 50.0 {
        "medium"
    } else {
        "low"
    };
    let has_discriminating_feedback = metrics.feedback_outcomes.contains_key("background")
        || metrics.feedback_outcomes.contains_key("rejected")
        || metrics.feedback_outcomes.contains_key("needs_l3");
    let status = if malformed_line_count > 0
        || metrics.orphan_feedback_count > 0
        || coverage < 50.0
        || (metrics.high_risk_route_count > metrics.high_risk_routes_with_l3)
    {
        "needs_attention"
    } else if confidence == "low" {
        "early_signal"
    } else if !has_discriminating_feedback {
        "insufficient_discrimination"
    } else {
        "effective"
    };
    EffectivenessConclusion {
        status: status.to_owned(),
        confidence: confidence.to_owned(),
        findings,
        recommendations,
    }
}

fn duration_summary(durations: &[u64]) -> Option<DurationSummary> {
    (!durations.is_empty()).then(|| DurationSummary {
        count: durations.len(),
        min: *durations.iter().min().unwrap_or(&0),
        max: *durations.iter().max().unwrap_or(&0),
        average: round_percent(as_f64(durations.iter().sum::<u64>()) / as_f64(durations.len())),
    })
}

fn percentage(numerator: usize, denominator: usize) -> Option<f64> {
    (denominator != 0).then(|| round_percent(as_f64(numerator) * 100.0 / as_f64(denominator)))
}

fn as_f64(value: impl IntoF64) -> f64 {
    value.into_f64()
}

trait IntoF64 {
    fn into_f64(self) -> f64;
}

impl IntoF64 for usize {
    fn into_f64(self) -> f64 {
        // usize is at most 64 bits; on 64-bit platforms this may exceed f64 precision,
        // but memory event counts stay well below 2^53.
        #[allow(clippy::cast_precision_loss)]
        {
            self as f64
        }
    }
}

impl IntoF64 for u64 {
    fn into_f64(self) -> f64 {
        // u64 may exceed f64 precision, but route durations and counts stay below 2^53.
        #[allow(clippy::cast_precision_loss)]
        {
            self as f64
        }
    }
}

fn round_percent(value: f64) -> f64 {
    (value * 100.0).round() / 100.0
}

fn feedback_command(args: FeedbackArgs, event_log: Option<&EventLog>) -> Result<()> {
    let event_log = event_log.context(
        "feedback requires the local event log; omit `--no-event-log` and use the same `--event-log` path as route",
    )?;
    let route_event = event_log.route_event(&args.request_id)?;
    let accepted_chunk_ids = validate_feedback_chunks(&route_event, args.outcome, args.chunk_ids)?;
    event_log.append(&FeedbackEvent {
        schema_version: 1,
        event_type: "route_feedback".to_owned(),
        timestamp_epoch_millis: epoch_millis(),
        request_id: args.request_id.clone(),
        outcome: args.outcome,
        chunk_ids: accepted_chunk_ids.clone(),
    })?;
    let output = FeedbackOutput {
        request_id: args.request_id,
        outcome: args.outcome,
        accepted_chunk_ids,
    };
    print_output(&output, args.json)
}

fn validate_feedback_chunks(
    route_event: &RouteInvocationEvent,
    outcome: FeedbackOutcome,
    mut chunk_ids: Vec<String>,
) -> Result<Vec<String>> {
    if outcome.requires_chunks() && chunk_ids.is_empty() {
        bail!("feedback outcome requires at least one --chunk-id");
    }
    if !outcome.requires_chunks() && !chunk_ids.is_empty() {
        bail!("blocked feedback must not include --chunk-id");
    }

    let routed_chunk_ids = route_event
        .l2_context
        .iter()
        .chain(&route_event.l3_authorities)
        .map(|candidate| candidate.chunk_id.as_str())
        .collect::<HashSet<_>>();
    let invalid_chunk_ids = chunk_ids
        .iter()
        .filter(|chunk_id| !routed_chunk_ids.contains(chunk_id.as_str()))
        .cloned()
        .collect::<Vec<_>>();
    if !invalid_chunk_ids.is_empty() {
        bail!(
            "feedback chunk IDs were not returned by route request `{}`: {}",
            route_event.request_id,
            invalid_chunk_ids.join(", ")
        );
    }

    chunk_ids.sort();
    chunk_ids.dedup();
    Ok(chunk_ids)
}

fn event_log_for_cli(root: &Path, cli: &Cli) -> Option<EventLog> {
    if cli.no_event_log {
        return None;
    }
    let requested_path = cli
        .event_log
        .clone()
        .unwrap_or_else(|| PathBuf::from(EVENT_LOG_PATH));
    let path = if requested_path.is_absolute() {
        requested_path
    } else {
        root.join(requested_path)
    };
    Some(EventLog { path })
}

impl EventLog {
    fn append<T: Serialize>(&self, event: &T) -> Result<()> {
        let parent = self.path.parent().context("event log must have a parent")?;
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .with_context(|| format!("open event log {}", self.path.display()))?;
        file.lock_exclusive()
            .with_context(|| format!("lock event log {}", self.path.display()))?;
        let result = (|| {
            let event = serde_json::to_string(event)?;
            writeln!(file, "{event}")
                .with_context(|| format!("write event log {}", self.path.display()))?;
            file.sync_data()
                .with_context(|| format!("sync event log {}", self.path.display()))
        })();
        let unlock_result = FileExt::unlock(&file)
            .with_context(|| format!("unlock event log {}", self.path.display()));
        result?;
        unlock_result
    }

    fn route_event(&self, request_id: &str) -> Result<RouteInvocationEvent> {
        let raw = fs::read_to_string(&self.path)
            .with_context(|| format!("read event log {}", self.path.display()))?;
        for line in raw.lines().rev().filter(|line| !line.trim().is_empty()) {
            let value: serde_json::Value = serde_json::from_str(line)
                .with_context(|| format!("parse event log {}", self.path.display()))?;
            if value.get("event_type").and_then(serde_json::Value::as_str) == Some("route_invoked")
                && value.get("request_id").and_then(serde_json::Value::as_str) == Some(request_id)
            {
                return serde_json::from_value(value).context("parse route invocation event");
            }
        }
        bail!(
            "route request `{request_id}` was not found in {}",
            self.path.display()
        )
    }
}

fn emit_command_invocation(
    event_log: Option<&EventLog>,
    command: &str,
    started: Instant,
    manifest: &Manifest,
    workspace: Option<String>,
    result_count: Option<usize>,
) -> Result<()> {
    if let Some(event_log) = event_log {
        event_log.append(&CommandInvocationEvent {
            schema_version: 1,
            event_type: "command_invoked".to_owned(),
            timestamp_epoch_millis: epoch_millis(),
            command: command.to_owned(),
            duration_millis: elapsed_millis(started),
            index_fingerprint: manifest_fingerprint(manifest),
            workspace,
            result_count,
        })?;
    }
    Ok(())
}

fn event_candidates(results: &[SearchResult]) -> Vec<EventCandidate> {
    results
        .iter()
        .enumerate()
        .map(|(index, result)| EventCandidate {
            chunk_id: result.chunk_id.clone(),
            rank: index + 1,
            kind: result.kind.clone(),
        })
        .collect()
}

fn chunk_id_for_parts(
    workspace: &str,
    path: &str,
    kind: &str,
    line_start: u64,
    line_end: u64,
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(workspace);
    hasher.update([0]);
    hasher.update(path);
    hasher.update([0]);
    hasher.update(kind);
    hasher.update([0]);
    hasher.update(line_start.to_le_bytes());
    hasher.update(line_end.to_le_bytes());
    format!("chunk_{:x}", hasher.finalize())
}

fn new_request_id() -> String {
    let sequence = REQUEST_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let mut hasher = Sha256::new();
    hasher.update(epoch_millis().to_le_bytes());
    hasher.update(std::process::id().to_le_bytes());
    hasher.update(sequence.to_le_bytes());
    format!("route_{:x}", hasher.finalize())
}

fn value_fingerprint(value: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(value.as_bytes());
    format!("sha256:{:x}", hasher.finalize())
}

fn manifest_fingerprint(manifest: &Manifest) -> String {
    let encoded = serde_json::to_vec(manifest).unwrap_or_default();
    value_fingerprint(&String::from_utf8_lossy(&encoded))
}

fn elapsed_millis(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

fn epoch_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

fn print_output<T: Serialize>(value: &T, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(value)?);
    } else {
        let rendered = serde_json::to_string_pretty(value)?;
        println!("{rendered}");
    }
    Ok(())
}

fn collect_sources(root: &Path, config: &Config) -> Result<Vec<PathBuf>> {
    let ignored = config
        .ignore_paths
        .iter()
        .map(|path| normalize_reference_path(path))
        .collect::<Vec<_>>();
    let mut sources = Vec::new();
    for workspace in enabled_workspaces(config) {
        let workspace_root = root.join(&workspace.path);
        let agents = workspace_root.join("AGENTS.md");
        if agents.is_file() {
            sources.push(agents);
        }
        for directory in ["memory", "tasks"] {
            let scan_root = workspace_root.join(directory);
            if !scan_root.is_dir() {
                continue;
            }
            for entry in WalkDir::new(&scan_root).into_iter().filter_map(Result::ok) {
                if !entry.file_type().is_file() || !is_markdown(entry.path()) {
                    continue;
                }
                let absolute = entry.path().to_path_buf();
                let relative_root = relative_path(root, &absolute)?;
                let relative_workspace = relative_path(&workspace_root, &absolute)?;
                if ignored.iter().any(|ignored_path| {
                    path_matches_ignore(&relative_root, ignored_path)
                        || path_matches_ignore(&relative_workspace, ignored_path)
                }) {
                    continue;
                }
                sources.push(absolute);
            }
        }
    }
    sources.sort();
    sources.dedup();
    Ok(sources)
}

fn path_matches_ignore(path: &str, ignored: &str) -> bool {
    path == ignored
        || path.starts_with(&format!("{ignored}/"))
        || path.contains(&format!("/{ignored}/"))
}

fn source_hashes(root: &Path, sources: &[PathBuf]) -> Result<BTreeMap<String, String>> {
    sources
        .iter()
        .map(|path| Ok((relative_path(root, path)?, file_hash(path)?)))
        .collect()
}

fn file_hash(path: &Path) -> Result<String> {
    let bytes = fs::read(path).with_context(|| format!("read {}", path.display()))?;
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    Ok(format!("{:x}", hasher.finalize()))
}

fn read_manifest(path: &Path) -> Result<Manifest> {
    if !path.is_file() {
        return Ok(Manifest::default());
    }
    let raw =
        fs::read_to_string(path).with_context(|| format!("read manifest {}", path.display()))?;
    serde_json::from_str(&raw).context("parse manifest")
}

fn write_manifest(path: &Path, manifest: &Manifest) -> Result<()> {
    let parent = path.parent().context("manifest must have parent")?;
    fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    let raw = serde_json::to_string_pretty(manifest)?;
    fs::write(path, format!("{raw}\n")).with_context(|| format!("write {}", path.display()))
}

fn workspace_for_path<'a>(
    root: &Path,
    config: &'a Config,
    path: &Path,
) -> Result<&'a WorkspaceConfig> {
    enabled_workspaces(config)
        .find(|workspace| path.starts_with(root.join(&workspace.path)))
        .context("source is not under an enabled workspace")
}

fn parse_source(workspace: &str, relative_path: &str, path: &Path) -> Result<Vec<Chunk>> {
    if relative_path == "AGENTS.md" {
        parse_agents(workspace, relative_path, path)
    } else if relative_path.starts_with("memory/") {
        let default_kind = classify_memory_kind(path)?;
        parse_markdown_chunks(workspace, &default_kind, relative_path, path)
    } else if relative_path.starts_with("tasks/") {
        parse_markdown_chunks(workspace, "task_experience", relative_path, path)
    } else {
        Ok(Vec::new())
    }
}

fn external_authorities_from_chunks(chunks: &[Chunk]) -> Vec<Chunk> {
    chunks
        .iter()
        .filter(|chunk| chunk.kind != "external_authority" && !chunk.source_urls.is_empty())
        .map(|chunk| Chunk {
            workspace: chunk.workspace.clone(),
            kind: "external_authority".to_owned(),
            path: chunk.path.clone(),
            heading_path: format!("{} > 权威外部来源", chunk.heading_path),
            line_start: chunk.line_start,
            line_end: chunk.line_end,
            title: format!("{}：权威外部来源", chunk.title),
            body: format!(
                "源章节：{}\n权威外部来源：{}",
                chunk.heading_path,
                chunk.source_urls.join("\n")
            ),
            entities: chunk.entities.clone(),
            environment: chunk.environment.clone(),
            lifecycle: chunk.lifecycle.clone(),
            source_urls: chunk.source_urls.clone(),
            verified_at: chunk.verified_at.clone(),
            canonical_ref: chunk.canonical_ref.clone(),
        })
        .collect()
}

fn parse_agents(workspace: &str, relative_path: &str, path: &Path) -> Result<Vec<Chunk>> {
    let mut chunks = parse_markdown_chunks(workspace, "stable_fact", relative_path, path)?;
    let rows = parse_navigation_rows(workspace, path)?;
    for row in rows {
        let body = format!(
            "任务主题：{}\n优先加载的 L2 知识：{}\nL3 核验/扩展边界：{}",
            row.topic, row.l2_knowledge, row.l3_boundary
        );
        let entities = extract_entities(&format!(
            "{} {} {}",
            row.topic, row.l2_knowledge, row.l3_boundary
        ));
        let environment = extract_environment(&format!("{} {}", row.topic, row.l3_boundary));
        let lifecycle = extract_lifecycle(&format!("{} {}", row.topic, row.l3_boundary));
        let source_urls = extract_urls(&format!("{} {}", row.l2_knowledge, row.l3_boundary));
        let canonical_ref = format!("{workspace}:{relative_path}#知识导航:{}", row.line_start);
        chunks.push(Chunk {
            workspace: workspace.to_owned(),
            kind: "route".to_owned(),
            path: relative_path.to_owned(),
            heading_path: "知识导航".to_owned(),
            line_start: row.line_start,
            line_end: row.line_end,
            title: row.topic.clone(),
            body: body.clone(),
            entities: entities.clone(),
            environment: environment.clone(),
            lifecycle: lifecycle.clone(),
            source_urls: source_urls.clone(),
            verified_at: None,
            canonical_ref: Some(canonical_ref.clone()),
        });
        chunks.push(Chunk {
            workspace: workspace.to_owned(),
            kind: "external_authority".to_owned(),
            path: relative_path.to_owned(),
            heading_path: "知识导航 > L3 核验/扩展边界".to_owned(),
            line_start: row.line_start,
            line_end: row.line_end,
            title: format!("{}：L3 权威核验", row.topic),
            body: format!("任务主题：{}\n{}", row.topic, row.l3_boundary),
            entities,
            environment,
            lifecycle,
            source_urls,
            verified_at: None,
            canonical_ref: Some(canonical_ref),
        });
    }
    Ok(chunks)
}

fn parse_navigation_rows(_workspace: &str, path: &Path) -> Result<Vec<NavigationRow>> {
    let content = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let lines = content.lines().collect::<Vec<_>>();
    let mut in_navigation = false;
    let mut rows = Vec::new();
    for (index, line) in lines.iter().enumerate() {
        let trimmed = line.trim();
        if trimmed.starts_with('#') {
            in_navigation = trimmed == "## 知识导航";
            continue;
        }
        if !in_navigation
            || !trimmed.starts_with('|')
            || trimmed.contains("---")
            || trimmed.contains("任务主题")
        {
            continue;
        }
        let cells = trimmed
            .trim_matches('|')
            .split('|')
            .map(str::trim)
            .collect::<Vec<_>>();
        if cells.len() < 3 {
            continue;
        }
        rows.push(NavigationRow {
            topic: cells[0].to_owned(),
            l2_knowledge: cells[1].to_owned(),
            l3_boundary: cells[2].to_owned(),
            line_start: (index + 1) as u64,
            line_end: (index + 1) as u64,
        });
    }
    Ok(rows)
}

fn classify_memory_kind(path: &Path) -> Result<String> {
    let content = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let lower = content.to_lowercase();
    let path_lower = path.to_string_lossy().to_lowercase();
    let runbook_terms = [
        "操作步骤",
        "流程",
        "检查清单",
        "排查",
        "执行前检查",
        "运行规范",
        "迁移",
        "验收",
        "创建",
        "初始化",
        "runbook",
    ];
    if runbook_terms
        .iter()
        .any(|term| lower.contains(term) || path_lower.contains(term))
    {
        Ok("runbook".to_owned())
    } else {
        Ok("stable_fact".to_owned())
    }
}

fn parse_markdown_chunks(
    workspace: &str,
    kind: &str,
    relative_path: &str,
    path: &Path,
) -> Result<Vec<Chunk>> {
    let content = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let lines = content.lines().collect::<Vec<_>>();
    let headings = markdown_headings(&lines);
    let metadata = parse_metadata(&content);
    let title = headings.first().map_or_else(
        || {
            path.file_stem()
                .and_then(|name| name.to_str())
                .unwrap_or("untitled")
                .to_owned()
        },
        |heading| heading.title.clone(),
    );

    let mut chunks = Vec::new();
    if headings.is_empty() {
        let body = content.trim().to_owned();
        if !body.is_empty() {
            chunks.push(make_chunk(
                workspace,
                kind,
                relative_path,
                title.clone(),
                title,
                1,
                lines.len().max(1) as u64,
                body,
                &metadata,
            ));
        }
        return Ok(chunks);
    }

    for (index, heading) in headings.iter().enumerate() {
        let next = headings.get(index + 1);
        let line_end = next
            .map_or(lines.len(), |next_heading| {
                next_heading.line.saturating_sub(1)
            })
            .max(heading.line);
        let body = lines[heading.line - 1..line_end].join("\n");
        if body.trim().is_empty() {
            continue;
        }
        let heading_path = heading_path(&headings, index);
        chunks.push(make_chunk(
            workspace,
            kind,
            relative_path,
            heading_path,
            heading.title.clone(),
            heading.line as u64,
            line_end as u64,
            body,
            &metadata,
        ));
    }
    Ok(chunks)
}

#[allow(clippy::too_many_arguments)]
fn make_chunk(
    workspace: &str,
    kind: &str,
    path: &str,
    heading_path: String,
    title: String,
    line_start: u64,
    line_end: u64,
    body: String,
    metadata: &Metadata,
) -> Chunk {
    let combined = format!("{heading_path}\n{body}");
    Chunk {
        workspace: workspace.to_owned(),
        kind: kind.to_owned(),
        path: path.to_owned(),
        heading_path,
        line_start,
        line_end,
        title,
        entities: extract_entities(&combined),
        environment: metadata
            .environment
            .clone()
            .into_iter()
            .chain(extract_environment(&combined))
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect(),
        lifecycle: metadata
            .lifecycle
            .clone()
            .into_iter()
            .chain(extract_lifecycle(&combined))
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect(),
        source_urls: metadata
            .source_urls
            .clone()
            .into_iter()
            .chain(extract_urls(&combined))
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect(),
        verified_at: metadata.verified_at.clone(),
        canonical_ref: metadata.canonical_ref.clone(),
        body,
    }
}

#[derive(Clone, Debug)]
struct Heading {
    level: usize,
    line: usize,
    title: String,
}

fn markdown_headings(lines: &[&str]) -> Vec<Heading> {
    let mut headings = Vec::new();
    let mut in_fence = false;
    for (index, line) in lines.iter().enumerate() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            in_fence = !in_fence;
            continue;
        }
        if in_fence {
            continue;
        }
        let hashes = trimmed
            .chars()
            .take_while(|character| *character == '#')
            .count();
        if hashes > 0 && hashes <= 6 && trimmed.chars().nth(hashes) == Some(' ') {
            headings.push(Heading {
                level: hashes,
                line: index + 1,
                title: trimmed[hashes..].trim().to_owned(),
            });
        }
    }
    headings
}

fn heading_path(headings: &[Heading], index: usize) -> String {
    let current = &headings[index];
    let mut parts = vec![current.title.clone()];
    let mut level = current.level;
    for heading in headings[..index].iter().rev() {
        if heading.level < level {
            parts.push(heading.title.clone());
            level = heading.level;
        }
    }
    parts.reverse();
    parts.join(" > ")
}

#[derive(Clone, Debug, Default)]
struct Metadata {
    environment: Vec<String>,
    lifecycle: Vec<String>,
    source_urls: Vec<String>,
    verified_at: Option<String>,
    canonical_ref: Option<String>,
}

fn parse_metadata(content: &str) -> Metadata {
    let mut metadata = Metadata::default();
    for line in content.lines().take(20) {
        let trimmed = line.trim().trim_start_matches('>').trim();
        if let Some(value) = trimmed.strip_prefix("适用范围：") {
            metadata.environment.extend(extract_environment(value));
        }
        if let Some(value) = trimmed.strip_prefix("最近核验时间：") {
            metadata.verified_at = Some(value.trim().to_owned());
        }
        if let Some(value) = trimmed.strip_prefix("权威来源：") {
            metadata.source_urls.extend(extract_urls(value));
        }
        if let Some(value) = trimmed.strip_prefix("canonical：") {
            metadata.canonical_ref = Some(value.trim().to_owned());
        }
    }
    metadata
}

fn extract_entities(content: &str) -> Vec<String> {
    let mut entities = BTreeSet::new();
    let code = Regex::new(r"`([^`\n]{2,160})`").expect("valid code regex");
    for captures in code.captures_iter(content) {
        let candidate = captures[1].trim();
        if is_entity(candidate) {
            entities.insert(candidate.to_owned());
        }
    }
    let identifier =
        Regex::new(r"\b[A-Za-z][A-Za-z0-9_./:-]{2,}\b").expect("valid identifier regex");
    for captures in identifier.captures_iter(content) {
        let candidate = &captures[0];
        if candidate.contains('_')
            || candidate.contains('-')
            || candidate.contains('/')
            || candidate.contains('.')
            || candidate
                .chars()
                .any(|character| character.is_ascii_digit())
        {
            entities.insert(candidate.to_owned());
        }
    }
    entities.into_iter().take(64).collect()
}

fn is_entity(candidate: &str) -> bool {
    !candidate.contains('\n')
        && candidate.len() <= 160
        && candidate
            .chars()
            .any(|character| character.is_ascii_alphanumeric() || character == '_')
}

fn extract_environment(content: &str) -> Vec<String> {
    let mut environments = BTreeSet::new();
    let lower = content.to_lowercase();
    for known in [
        "生产", "线上", "线下", "online", "offline", "prod", "test", "bjep", "hlep", "hl2ep",
    ] {
        if lower.contains(&known.to_lowercase()) || content.contains(known) {
            environments.insert(known.to_owned());
        }
    }
    let clusters =
        Regex::new(r"\b(?:k8s|catalog-v3|influxdb)-[A-Za-z0-9-]+\b").expect("valid cluster regex");
    for captures in clusters.captures_iter(content) {
        environments.insert(captures[0].to_owned());
    }
    environments.into_iter().collect()
}

fn extract_lifecycle(content: &str) -> Vec<String> {
    let lifecycle_terms = [
        "创建",
        "初始化",
        "上线",
        "下线",
        "迁移",
        "切换",
        "验收",
        "排查",
        "恢复",
        "删除",
        "运行",
        "重构",
        "核验",
    ];
    lifecycle_terms
        .into_iter()
        .filter(|term| content.contains(term))
        .map(str::to_owned)
        .collect()
}

fn extract_urls(content: &str) -> Vec<String> {
    // A concrete host is required so examples such as `http://<VIP>:<PORT>` do not
    // become misleading L3 authority entries.
    let url = Regex::new(r"https?://[A-Za-z0-9][A-Za-z0-9.-]*(?::\d+)?(?:/[^\s)>\]`{}]*)?")
        .expect("valid URL regex");
    url.find_iter(content)
        .map(|match_| {
            match_
                .as_str()
                .trim_end_matches(['.', ',', ';', ':'])
                .to_owned()
        })
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn extract_markdown_paths(content: &str) -> Vec<String> {
    let path = Regex::new(r"`((?:memory|tasks)/[^`\s]+?\.md)`").expect("valid markdown path regex");
    path.captures_iter(content)
        .map(|captures| normalize_reference_path(&captures[1]))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn normalize_reference_path(path: &str) -> String {
    path.trim()
        .trim_matches('`')
        .trim_start_matches("./")
        .replace('\\', "/")
}

fn build_index(index_path: &Path, chunks: &[Chunk]) -> Result<()> {
    let (schema, fields) = Fields::build_schema();
    let index = Index::create_in_dir(index_path, schema)?;
    register_tokenizers(&index);
    let mut writer = index.writer(50_000_000)?;
    for chunk in chunks {
        let id = chunk_id_for_parts(
            &chunk.workspace,
            &chunk.path,
            &chunk.kind,
            chunk.line_start,
            chunk.line_end,
        );
        let mut document = TantivyDocument::default();
        document.add_text(fields.id, id);
        document.add_text(fields.workspace, &chunk.workspace);
        document.add_text(fields.kind, &chunk.kind);
        document.add_text(fields.path, &chunk.path);
        document.add_text(fields.heading_path, &chunk.heading_path);
        document.add_u64(fields.line_start, chunk.line_start);
        document.add_u64(fields.line_end, chunk.line_end);
        document.add_text(fields.title, &chunk.title);
        document.add_text(fields.body, &chunk.body);
        document.add_text(fields.title_cjk, &chunk.title);
        document.add_text(fields.body_cjk, &chunk.body);
        document.add_text(fields.heading_path_cjk, &chunk.heading_path);
        for entity in &chunk.entities {
            document.add_text(fields.entities, entity);
        }
        for environment in &chunk.environment {
            document.add_text(fields.environment, environment);
        }
        for lifecycle in &chunk.lifecycle {
            document.add_text(fields.lifecycle, lifecycle);
        }
        for source_url in &chunk.source_urls {
            document.add_text(fields.source_urls, source_url);
        }
        if let Some(verified_at) = &chunk.verified_at {
            document.add_text(fields.verified_at, verified_at);
        }
        if let Some(canonical_ref) = &chunk.canonical_ref {
            document.add_text(fields.canonical_ref, canonical_ref);
        }
        writer.add_document(document)?;
    }
    writer.commit()?;
    Ok(())
}

fn open_index(root: &Path, config: &Config) -> Result<Index> {
    let path = root.join(&config.index_dir);
    let index =
        Index::open_in_dir(&path).with_context(|| format!("open index {}", path.display()))?;
    register_tokenizers(&index);
    Ok(index)
}

fn register_tokenizers(index: &Index) {
    index.tokenizers().register(
        "code",
        TextAnalyzer::builder(RawTokenizer::default())
            .filter(LowerCaser)
            .build(),
    );
    index.tokenizers().register(
        "mixed",
        TextAnalyzer::builder(SimpleTokenizer::default())
            .filter(RemoveLongFilter::limit(80))
            .filter(LowerCaser)
            .build(),
    );
    index.tokenizers().register(
        "cjk_ngram",
        TextAnalyzer::builder(NgramTokenizer::new(2, 4, false).expect("valid ngram tokenizer"))
            .filter(LowerCaser)
            .build(),
    );
}

fn ensure_index(root: &Path, config: &Config) -> Result<()> {
    let index_path = root.join(&config.index_dir);
    let manifest_path = root.join(&config.manifest_path);
    if !index_path.exists() || !manifest_path.exists() {
        bail!(
            "MemRoute index is missing. Run `memroute --root {} index` first.",
            root.display()
        );
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn search_index(
    index: &Index,
    fields: &Fields,
    query_text: &str,
    workspace: Option<&str>,
    kinds: &[String],
    entities: &[String],
    environment: Option<&str>,
    limit: usize,
) -> Result<Vec<SearchResult>> {
    search_scoped(
        index,
        fields,
        query_text,
        &workspace.into_iter().map(str::to_owned).collect::<Vec<_>>(),
        kinds,
        entities,
        environment,
        limit,
    )
}

#[allow(clippy::too_many_arguments)]
fn search_scoped(
    index: &Index,
    fields: &Fields,
    query_text: &str,
    workspaces: &[String],
    kinds: &[String],
    entities: &[String],
    environment: Option<&str>,
    limit: usize,
) -> Result<Vec<SearchResult>> {
    let reader = index
        .reader_builder()
        .reload_policy(ReloadPolicy::OnCommitWithDelay)
        .try_into()?;
    let query = build_query(
        index,
        fields,
        query_text,
        workspaces,
        kinds,
        entities,
        environment,
    );
    execute_search(&reader, fields, query.as_ref(), limit)
}

fn build_query(
    index: &Index,
    fields: &Fields,
    query_text: &str,
    workspaces: &[String],
    kinds: &[String],
    entities: &[String],
    environment: Option<&str>,
) -> Box<dyn Query> {
    let parser = QueryParser::for_index(index, fields.searchable());
    let mut should_clauses: Vec<(Occur, Box<dyn Query>)> = Vec::new();
    for term in query_terms(query_text) {
        if let Ok(query) = parser.parse_query(&term) {
            should_clauses.push((Occur::Should, Box::new(query)));
        }
        // 含 CJK 的词额外查询 n-gram 副本字段。
        // SimpleTokenizer 把连续中文当作单个 token，无法做子串匹配；
        // n-gram 字段则把「显卡驱动」拆成「显卡」「卡驱」「驱动」等 2-4 字片段，
        // 从而能命中「重启后显卡驱动不正常」。纯 ASCII 词不加此子句，避免拖低精度。
        if term.chars().any(is_cjk) {
            let grams = cjk_ngrams(&term, 2, 4);
            if !grams.is_empty() {
                let mut gram_clauses: Vec<(Occur, Box<dyn Query>)> = Vec::new();
                for gram in &grams {
                    for field in [fields.title_cjk, fields.body_cjk, fields.heading_path_cjk] {
                        gram_clauses.push((
                            Occur::Should,
                            Box::new(TermQuery::new(
                                Term::from_field_text(field, gram),
                                IndexRecordOption::WithFreqs,
                            )),
                        ));
                    }
                }
                // 任一 n-gram 命中即可（Should 在 BooleanQuery 中默认可满足其一）
                should_clauses.push((Occur::Should, Box::new(BooleanQuery::from(gram_clauses))));
            }
        }
    }
    for entity in entities {
        should_clauses.push((
            Occur::Should,
            Box::new(TermQuery::new(
                Term::from_field_text(fields.entities, entity),
                IndexRecordOption::WithFreqs,
            )),
        ));
    }
    let base_query: Box<dyn Query> = if should_clauses.is_empty() {
        Box::new(AllQuery)
    } else {
        Box::new(BooleanQuery::from(should_clauses))
    };
    let mut clauses: Vec<(Occur, Box<dyn Query>)> = vec![(Occur::Must, base_query)];
    if !workspaces.is_empty() {
        clauses.push((Occur::Must, exact_any_query(fields.workspace, workspaces)));
    }
    if !kinds.is_empty() {
        clauses.push((Occur::Must, exact_any_query(fields.kind, kinds)));
    }
    if !entities.is_empty() {
        clauses.push((Occur::Must, text_all_query(fields.entities, entities)));
    }
    if let Some(environment) = environment.filter(|value| !value.trim().is_empty()) {
        clauses.push((
            Occur::Must,
            text_all_query(fields.environment, &[environment.to_owned()]),
        ));
    }
    Box::new(BooleanQuery::from(clauses))
}

fn exact_any_query(field: Field, values: &[String]) -> Box<dyn Query> {
    let clauses = values
        .iter()
        .map(|value| {
            (
                Occur::Should,
                Box::new(TermQuery::new(
                    Term::from_field_text(field, value),
                    IndexRecordOption::Basic,
                )) as Box<dyn Query>,
            )
        })
        .collect::<Vec<_>>();
    Box::new(BooleanQuery::from(clauses))
}

fn text_all_query(field: Field, values: &[String]) -> Box<dyn Query> {
    let clauses = values
        .iter()
        .map(|value| {
            (
                Occur::Must,
                Box::new(TermQuery::new(
                    Term::from_field_text(field, &value.to_lowercase()),
                    IndexRecordOption::WithFreqs,
                )) as Box<dyn Query>,
            )
        })
        .collect::<Vec<_>>();
    Box::new(BooleanQuery::from(clauses))
}

fn query_terms(query: &str) -> Vec<String> {
    let sanitized = sanitize_query(query);
    let mut terms = sanitized
        .split_whitespace()
        .filter(|term| !term.is_empty())
        .map(str::to_owned)
        .collect::<Vec<_>>();
    if terms.is_empty() && !sanitized.trim().is_empty() {
        terms.push(sanitized);
    }
    terms
}

fn is_cjk(character: char) -> bool {
    matches!(character as u32,
        0x3400..=0x4DBF      // CJK 扩展 A
        | 0x4E00..=0x9FFF    // CJK 基本区
        | 0xF900..=0xFAFF    // CJK 兼容
        | 0x3040..=0x30FF    // 日文假名
        | 0xAC00..=0xD7AF    // 韩文音节
        | 0x20000..=0x2FA1F  // CJK 扩展 B-F
    )
}

/// 生成 min..=max 字符长度的 n-gram，用于匹配 `cjk_ngram` 分词器建的中文索引。
/// 与 Tantivy `NgramTokenizer::new(min, max, false)` 的行为保持一致：
/// 只切完整字符边界，不做前缀填充。
fn cjk_ngrams(text: &str, min: usize, max: usize) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    let mut grams = Vec::new();
    for size in min..=max {
        if size == 0 || size > chars.len() {
            continue;
        }
        for window in chars.windows(size) {
            grams.push(window.iter().collect::<String>());
        }
    }
    grams.sort();
    grams.dedup();
    grams
}

fn sanitize_query(query: &str) -> String {
    query
        .chars()
        .map(|character| match character {
            ':' | '(' | ')' | '[' | ']' | '{' | '}' | '"' | '^' | '~' | '*' => ' ',
            _ => character,
        })
        .collect()
}

fn execute_search(
    reader: &IndexReader,
    fields: &Fields,
    query: &dyn Query,
    limit: usize,
) -> Result<Vec<SearchResult>> {
    let searcher = reader.searcher();
    let documents = searcher.search(query, &TopDocs::with_limit(limit.max(1)))?;
    documents
        .into_iter()
        .map(|(score, address)| {
            let document: TantivyDocument = searcher.doc(address)?;
            document_to_result(score, &document, fields)
        })
        .collect()
}

fn document_to_result(
    score: f32,
    document: &TantivyDocument,
    fields: &Fields,
) -> Result<SearchResult> {
    let chunk_id = first_text(document, fields.id)?;
    let workspace = first_text(document, fields.workspace)?;
    let kind = first_text(document, fields.kind)?;
    let path = first_text(document, fields.path)?;
    let heading_path = first_text(document, fields.heading_path)?;
    let line_start = first_u64(document, fields.line_start)?;
    let line_end = first_u64(document, fields.line_end)?;
    let title = first_text(document, fields.title)?;
    let body = first_text(document, fields.body)?;
    Ok(SearchResult {
        score,
        chunk_id,
        workspace,
        kind,
        path,
        heading_path,
        line_start,
        line_end,
        title,
        entities: all_text(document, fields.entities),
        environment: all_text(document, fields.environment),
        lifecycle: all_text(document, fields.lifecycle),
        source_urls: all_text(document, fields.source_urls),
        verified_at: optional_text(document, fields.verified_at),
        canonical_ref: optional_text(document, fields.canonical_ref),
        excerpt: excerpt(&body, 280),
    })
}

fn first_text(document: &TantivyDocument, field: Field) -> Result<String> {
    optional_text(document, field).context("expected stored text field")
}

fn optional_text(document: &TantivyDocument, field: Field) -> Option<String> {
    document
        .get_first(field)
        .and_then(|value| value.as_str())
        .map(str::to_owned)
}

fn all_text(document: &TantivyDocument, field: Field) -> Vec<String> {
    document
        .get_all(field)
        .filter_map(|value| value.as_str().map(str::to_owned))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn first_u64(document: &TantivyDocument, field: Field) -> Result<u64> {
    document
        .get_first(field)
        .and_then(|value| value.as_u64())
        .context("expected stored u64 field")
}

fn excerpt(body: &str, max_chars: usize) -> String {
    let normalized = body.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut chars = normalized.chars();
    let excerpt = chars.by_ref().take(max_chars).collect::<String>();
    if chars.next().is_some() {
        format!("{excerpt}…")
    } else {
        excerpt
    }
}

fn build_route_query(args: &RouteArgs) -> String {
    let generic_entities = args
        .entities
        .iter()
        .filter_map(|entity| entity.rsplit_once('-').map(|(prefix, _)| prefix.to_owned()))
        .collect::<Vec<_>>()
        .join(" ");
    [
        Some(args.query.as_str()),
        args.intent.as_deref(),
        args.environment.as_deref(),
        (!args.entities.is_empty())
            .then(|| args.entities.join(" "))
            .as_deref(),
        (!generic_entities.is_empty()).then_some(generic_entities.as_str()),
    ]
    .into_iter()
    .flatten()
    .filter(|part| !part.trim().is_empty())
    .collect::<Vec<_>>()
    .join(" ")
}

fn search_l3_authorities(
    index: &Index,
    fields: &Fields,
    query_text: &str,
    workspaces: &[String],
    route_matches: &[SearchResult],
    limit: usize,
) -> Result<Vec<SearchResult>> {
    let route_refs = route_matches
        .iter()
        .filter_map(|result| result.canonical_ref.clone())
        .collect::<Vec<_>>();
    let mut results = if route_refs.is_empty() {
        Vec::new()
    } else {
        let reader = index
            .reader_builder()
            .reload_policy(ReloadPolicy::OnCommitWithDelay)
            .try_into()?;
        let query = BooleanQuery::from(vec![
            (Occur::Must, exact_any_query(fields.workspace, workspaces)),
            (
                Occur::Must,
                exact_any_query(fields.kind, &["external_authority".to_owned()]),
            ),
            (
                Occur::Must,
                exact_any_query(fields.canonical_ref, &route_refs),
            ),
        ]);
        execute_search(&reader, fields, &query, limit)?
    };

    let semantic = search_scoped(
        index,
        fields,
        query_text,
        workspaces,
        &["external_authority".to_owned()],
        &[],
        None,
        limit,
    )?;
    results.extend(semantic);
    let mut seen = HashSet::new();
    results.retain(|result| {
        seen.insert(format!(
            "{}:{}:{}:{}",
            result.workspace, result.path, result.line_start, result.title
        ))
    });
    results.truncate(limit);
    Ok(results)
}

fn extract_markdown_paths_from_source(
    root: &Path,
    config: &Config,
    result: &SearchResult,
) -> Vec<String> {
    let Some(workspace) =
        enabled_workspaces(config).find(|workspace| workspace.name == result.workspace)
    else {
        return Vec::new();
    };
    let source_path = root.join(&workspace.path).join(&result.path);
    let Ok(content) = fs::read_to_string(source_path) else {
        return Vec::new();
    };
    let lines = content.lines().collect::<Vec<_>>();
    let start = usize::try_from(result.line_start.saturating_sub(1)).unwrap_or(lines.len());
    let end = usize::try_from(result.line_end)
        .unwrap_or(lines.len())
        .min(lines.len());
    if start >= end {
        return Vec::new();
    }
    extract_markdown_paths(&lines[start..end].join("\n"))
}

fn rank_route_candidates(
    candidates: Vec<SearchResult>,
    preferred_paths: &HashSet<String>,
    owning_workspace: &str,
    limit: usize,
) -> Vec<SearchResult> {
    let mut scored = candidates
        .into_iter()
        .map(|result| {
            let kind_bonus = match result.kind.as_str() {
                "stable_fact" => 60.0,
                "runbook" => 45.0,
                "task_experience" => 5.0,
                _ => 0.0,
            };
            let route_bonus = if preferred_paths
                .iter()
                .any(|path| result.path == *path || result.path.ends_with(path))
            {
                80.0
            } else {
                0.0
            };
            let workspace_bonus = if result.workspace == owning_workspace {
                20.0
            } else {
                0.0
            };
            (
                result.score + kind_bonus + route_bonus + workspace_bonus,
                result,
            )
        })
        .collect::<Vec<_>>();
    scored.sort_by(|left, right| right.0.total_cmp(&left.0));
    let mut seen = HashSet::new();
    scored
        .into_iter()
        .filter_map(|(_, result)| {
            let key = format!(
                "{}:{}:{}",
                result.workspace, result.path, result.heading_path
            );
            seen.insert(key).then_some(result)
        })
        .take(limit)
        .collect()
}

fn all_documents(index: &Index, fields: &Fields) -> Result<Vec<SearchResult>> {
    let reader = index
        .reader_builder()
        .reload_policy(ReloadPolicy::OnCommitWithDelay)
        .try_into()?;
    execute_search(&reader, fields, &AllQuery, 100_000)
}

fn search_exact_path(
    index: &Index,
    fields: &Fields,
    workspace: Option<&str>,
    path: &str,
    limit: usize,
) -> Result<Vec<SearchResult>> {
    let reader = index
        .reader_builder()
        .reload_policy(ReloadPolicy::OnCommitWithDelay)
        .try_into()?;
    let mut clauses: Vec<(Occur, Box<dyn Query>)> = vec![(
        Occur::Must,
        Box::new(TermQuery::new(
            Term::from_field_text(fields.path, path),
            IndexRecordOption::Basic,
        )),
    )];
    if let Some(workspace) = workspace {
        clauses.push((
            Occur::Must,
            Box::new(TermQuery::new(
                Term::from_field_text(fields.workspace, workspace),
                IndexRecordOption::Basic,
            )),
        ));
    }
    execute_search(&reader, fields, &BooleanQuery::from(clauses), limit)
}

fn require_workspace<'a>(config: &'a Config, name: &str) -> Result<&'a WorkspaceConfig> {
    enabled_workspaces(config)
        .find(|workspace| workspace.name == name)
        .with_context(|| format!("unknown or disabled workspace `{name}`"))
}

fn split_workspace_path(config: &Config, requested: &str) -> (Option<String>, String) {
    for workspace in enabled_workspaces(config) {
        let prefix = format!("{}/", workspace.name);
        if let Some(path) = requested.strip_prefix(&prefix) {
            return (Some(workspace.name.clone()), path.to_owned());
        }
    }
    (None, requested.to_owned())
}

fn relative_path(root: impl AsRef<Path>, path: impl AsRef<Path>) -> Result<String> {
    path.as_ref()
        .strip_prefix(root.as_ref())
        .with_context(|| {
            format!(
                "{} is not under {}",
                path.as_ref().display(),
                root.as_ref().display()
            )
        })?
        .components()
        .filter_map(|component| match component {
            Component::Normal(value) => value.to_str(),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/")
        .pipe(Ok)
}

trait Pipe: Sized {
    fn pipe<T>(self, f: impl FnOnce(Self) -> T) -> T {
        f(self)
    }
}
impl<T> Pipe for T {}

fn is_markdown(path: &Path) -> bool {
    path.extension().and_then(|extension| extension.to_str()) == Some("md")
}

fn epoch_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn is_promotion_candidate(chunk: &Chunk) -> bool {
    let body = chunk.body.to_lowercase();
    let confirmed = [
        "已确认",
        "结论",
        "根因",
        "规范",
        "必须",
        "禁止",
        "标准",
        "注意事项",
    ]
    .iter()
    .any(|term| body.contains(term));
    let has_evidence =
        !chunk.source_urls.is_empty() || body.contains("来源") || body.contains("证据");
    confirmed && (has_evidence || chunk.body.chars().count() > 180)
}

fn promotion_reason(chunk: &Chunk) -> String {
    if chunk.source_urls.is_empty() {
        "section contains confirmed/standard language and enough task detail to review; add or verify an authority before promotion".to_owned()
    } else {
        "section contains confirmed/standard language and an external source reference; review whether it is reusable beyond this task".to_owned()
    }
}

fn suggest_kind(chunk: &Chunk) -> String {
    if extract_lifecycle(&chunk.body).iter().any(|value| {
        matches!(
            value.as_str(),
            "创建" | "初始化" | "迁移" | "验收" | "排查" | "恢复"
        )
    }) {
        "runbook".to_owned()
    } else {
        "stable_fact".to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn headings_exclude_fenced_code() {
        let lines = vec!["# One", "```text", "## Not a heading", "```", "## Two"];
        let headings = markdown_headings(&lines);
        assert_eq!(headings.len(), 2);
        assert_eq!(headings[1].title, "Two");
    }

    #[test]
    fn chunking_preserves_heading_path_and_line_range() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("knowledge.md");
        fs::write(&path, "# Root\nintro\n## Child\nbody\n### Leaf\nleaf\n").unwrap();
        let chunks =
            parse_markdown_chunks("demo", "stable_fact", "memory/knowledge.md", &path).unwrap();
        let child = chunks.iter().find(|chunk| chunk.title == "Child").unwrap();
        assert_eq!(child.heading_path, "Root > Child");
        assert_eq!(child.line_start, 3);
        assert_eq!(child.line_end, 4);
    }

    #[test]
    fn navigation_parser_reads_table_rows_only() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("AGENTS.md");
        fs::write(
            &path,
            "# Rules\n## 知识导航\n| 任务主题 | 优先加载的 L2 知识 | L3 核验/扩展边界 |\n|---|---|---|\n| Topic | `memory/topic.md` | https://example.test |\n### Other\n| ignored | no | no |\n",
        )
        .unwrap();
        let rows = parse_navigation_rows("demo", &path).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].topic, "Topic");
    }

    #[test]
    fn url_extraction_ignores_placeholder_endpoints() {
        let urls = extract_urls("real: https://example.com/docs/123; example: http://<VIP>:<PORT>");
        assert_eq!(urls, vec!["https://example.com/docs/123"]);
    }

    #[test]
    fn agent_navigation_emits_route_and_l3_entries() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("AGENTS.md");
        fs::write(
            &path,
            "# Rules\n## 知识导航\n| 任务主题 | 优先加载的 L2 知识 | L3 核验/扩展边界 |\n|---|---|---|\n| Topic | `memory/topic.md` | Kubernetes API |\n",
        )
        .unwrap();
        let chunks = parse_agents("demo", "AGENTS.md", &path).unwrap();
        assert_eq!(
            chunks.iter().filter(|chunk| chunk.kind == "route").count(),
            1
        );
        assert_eq!(
            chunks
                .iter()
                .filter(|chunk| chunk.kind == "external_authority")
                .count(),
            1
        );
    }

    #[test]
    fn event_log_append_serializes_concurrent_writers_as_jsonl() {
        let temp = TempDir::new().unwrap();
        let event_log = EventLog {
            path: temp.path().join("events.jsonl"),
        };
        let workers = (0..16)
            .map(|index| {
                let event_log = event_log.clone();
                std::thread::spawn(move || {
                    event_log
                        .append(&CommandInvocationEvent {
                            schema_version: 1,
                            event_type: "command_invoked".to_owned(),
                            timestamp_epoch_millis: index,
                            command: "index".to_owned(),
                            duration_millis: 0,
                            index_fingerprint: "sha256:test".to_owned(),
                            workspace: None,
                            result_count: None,
                        })
                        .unwrap();
                })
            })
            .collect::<Vec<_>>();
        for worker in workers {
            worker.join().unwrap();
        }

        let contents = fs::read_to_string(&event_log.path).unwrap();
        let events = contents
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(events.len(), 16);
        assert!(
            events
                .iter()
                .all(|event| event["event_type"] == "command_invoked")
        );
    }

    #[test]
    fn event_log_selection_defaults_to_memroute_and_honors_opt_out() {
        let root = Path::new("/collection");
        let mut cli = Cli {
            root: None,
            event_log: None,
            no_event_log: false,
            command: Command::Index(IndexArgs { force: false }),
        };

        assert_eq!(
            event_log_for_cli(root, &cli).unwrap().path,
            root.join(EVENT_LOG_PATH)
        );

        cli.event_log = Some(PathBuf::from("local/usage.jsonl"));
        assert_eq!(
            event_log_for_cli(root, &cli).unwrap().path,
            root.join("local/usage.jsonl")
        );

        cli.no_event_log = true;
        assert!(event_log_for_cli(root, &cli).is_none());
    }

    #[test]
    fn feedback_validation_accepts_returned_chunks_and_deduplicates() {
        let route_event = RouteInvocationEvent {
            schema_version: 1,
            event_type: "route_invoked".to_owned(),
            timestamp_epoch_millis: 1,
            request_id: "route_test".to_owned(),
            task_id: None,
            workspace: "demo".to_owned(),
            intent_fingerprint: None,
            risk: "normal".to_owned(),
            scopes: vec!["demo".to_owned()],
            duration_millis: 1,
            index_fingerprint: "sha256:index".to_owned(),
            l2_context: vec![EventCandidate {
                chunk_id: "chunk_l2".to_owned(),
                rank: 1,
                kind: "stable_fact".to_owned(),
            }],
            l3_authorities: vec![EventCandidate {
                chunk_id: "chunk_l3".to_owned(),
                rank: 1,
                kind: "external_authority".to_owned(),
            }],
        };
        let accepted = validate_feedback_chunks(
            &route_event,
            FeedbackOutcome::Used,
            vec![
                "chunk_l3".to_owned(),
                "chunk_l2".to_owned(),
                "chunk_l2".to_owned(),
            ],
        )
        .unwrap();
        assert_eq!(accepted, vec!["chunk_l2", "chunk_l3"]);
    }

    #[test]
    fn feedback_validation_rejects_unknown_or_invalid_chunk_sets() {
        let route_event = RouteInvocationEvent {
            schema_version: 1,
            event_type: "route_invoked".to_owned(),
            timestamp_epoch_millis: 1,
            request_id: "route_test".to_owned(),
            task_id: None,
            workspace: "demo".to_owned(),
            intent_fingerprint: None,
            risk: "normal".to_owned(),
            scopes: vec!["demo".to_owned()],
            duration_millis: 1,
            index_fingerprint: "sha256:index".to_owned(),
            l2_context: vec![EventCandidate {
                chunk_id: "chunk_l2".to_owned(),
                rank: 1,
                kind: "stable_fact".to_owned(),
            }],
            l3_authorities: Vec::new(),
        };
        assert!(
            validate_feedback_chunks(
                &route_event,
                FeedbackOutcome::Used,
                vec!["unknown_chunk".to_owned()],
            )
            .is_err()
        );
        assert!(validate_feedback_chunks(&route_event, FeedbackOutcome::Used, Vec::new()).is_err());
        assert!(
            validate_feedback_chunks(
                &route_event,
                FeedbackOutcome::Blocked,
                vec!["chunk_l2".to_owned()],
            )
            .is_err()
        );
    }

    #[test]
    fn analysis_reports_metrics_and_effectiveness_for_workspace() {
        let parsed = ParsedEventLog {
            physical_line_count: 4,
            concatenated_event_lines: 0,
            malformed_line_count: 0,
            events: vec![
                LoggedEvent::CommandInvoked(CommandInvocationEvent {
                    schema_version: 1,
                    event_type: "command_invoked".to_owned(),
                    timestamp_epoch_millis: 1,
                    command: "route".to_owned(),
                    duration_millis: 25,
                    index_fingerprint: "sha256:index".to_owned(),
                    workspace: Some("demo".to_owned()),
                    result_count: Some(2),
                }),
                LoggedEvent::RouteInvoked(RouteInvocationEvent {
                    schema_version: 1,
                    event_type: "route_invoked".to_owned(),
                    timestamp_epoch_millis: 2,
                    request_id: "route_demo".to_owned(),
                    task_id: None,
                    workspace: "demo".to_owned(),
                    intent_fingerprint: None,
                    risk: "high".to_owned(),
                    scopes: vec!["demo".to_owned()],
                    duration_millis: 20,
                    index_fingerprint: "sha256:index".to_owned(),
                    l2_context: vec![EventCandidate {
                        chunk_id: "chunk_l2".to_owned(),
                        rank: 1,
                        kind: "stable_fact".to_owned(),
                    }],
                    l3_authorities: vec![EventCandidate {
                        chunk_id: "chunk_l3".to_owned(),
                        rank: 1,
                        kind: "external_authority".to_owned(),
                    }],
                }),
                LoggedEvent::RouteFeedback(FeedbackEvent {
                    schema_version: 1,
                    event_type: "route_feedback".to_owned(),
                    timestamp_epoch_millis: 3,
                    request_id: "route_demo".to_owned(),
                    outcome: FeedbackOutcome::Used,
                    chunk_ids: vec!["chunk_l2".to_owned()],
                }),
                LoggedEvent::RouteFeedback(FeedbackEvent {
                    schema_version: 1,
                    event_type: "route_feedback".to_owned(),
                    timestamp_epoch_millis: 4,
                    request_id: "route_demo".to_owned(),
                    outcome: FeedbackOutcome::Rejected,
                    chunk_ids: vec!["chunk_l3".to_owned()],
                }),
            ],
        };

        let output = analyze_events(parsed, Some("demo"));
        assert_eq!(output.metrics.route_count, 1);
        assert_eq!(output.metrics.routes_with_feedback, 1);
        assert_eq!(output.metrics.feedback_route_coverage_percent, Some(100.0));
        assert_eq!(output.metrics.judged_candidate_count, 2);
        assert_eq!(output.metrics.used_candidate_count, 1);
        assert_eq!(
            output.metrics.used_candidate_rate_among_judged_percent,
            Some(50.0)
        );
        assert_eq!(output.metrics.high_risk_routes_with_l3, 1);
        assert_eq!(output.metrics.used_candidates_by_kind["stable_fact"], 1);
        assert_eq!(output.conclusion.status, "early_signal");
    }

    #[test]
    fn event_log_parser_tolerates_concatenated_and_malformed_records() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("events.jsonl");
        fs::write(
            &path,
            concat!(
                r#"{"schema_version":1,"event_type":"command_invoked","timestamp_epoch_millis":1,"command":"index","duration_millis":1,"index_fingerprint":"sha256:index"}"#,
                r#"{"schema_version":1,"event_type":"route_feedback","timestamp_epoch_millis":2,"request_id":"route_demo","outcome":"blocked","chunk_ids":[]}"#,
                "\n{not-json}\n"
            ),
        )
        .unwrap();

        let parsed = parse_event_log(&path).unwrap();
        assert_eq!(parsed.events.len(), 2);
        assert_eq!(parsed.physical_line_count, 2);
        assert_eq!(parsed.concatenated_event_lines, 1);
        assert_eq!(parsed.malformed_line_count, 1);
    }

    #[test]
    fn route_event_candidates_do_not_include_source_content() {
        let results = vec![SearchResult {
            score: 1.0,
            chunk_id: "chunk_test".to_owned(),
            workspace: "demo".to_owned(),
            kind: "stable_fact".to_owned(),
            path: "memory/private.md".to_owned(),
            heading_path: "Sensitive heading".to_owned(),
            line_start: 1,
            line_end: 2,
            title: "Sensitive title".to_owned(),
            entities: vec!["secret-entity".to_owned()],
            environment: vec!["production".to_owned()],
            lifecycle: Vec::new(),
            source_urls: vec!["https://private.example".to_owned()],
            verified_at: None,
            canonical_ref: None,
            excerpt: "sensitive source text".to_owned(),
        }];
        let candidates = event_candidates(&results);
        let serialized = serde_json::to_string(&candidates).unwrap();
        assert_eq!(candidates[0].chunk_id, "chunk_test");
        assert!(!serialized.contains("private.md"));
        assert!(!serialized.contains("sensitive source text"));
        assert!(!serialized.contains("secret-entity"));
    }

    #[test]
    fn route_ranking_prefers_navigated_stable_knowledge() {
        let candidates = vec![
            SearchResult {
                score: 100.0,
                chunk_id: "chunk_demo_task".to_owned(),
                workspace: "demo".to_owned(),
                kind: "task_experience".to_owned(),
                path: "tasks/a/task.md".to_owned(),
                heading_path: "Task".to_owned(),
                line_start: 1,
                line_end: 10,
                title: "Task".to_owned(),
                entities: vec![],
                environment: vec![],
                lifecycle: vec![],
                source_urls: vec![],
                verified_at: None,
                canonical_ref: None,
                excerpt: String::new(),
            },
            SearchResult {
                score: 30.0,
                chunk_id: "chunk_demo_fact".to_owned(),
                workspace: "demo".to_owned(),
                kind: "stable_fact".to_owned(),
                path: "memory/topic.md".to_owned(),
                heading_path: "Fact".to_owned(),
                line_start: 1,
                line_end: 10,
                title: "Fact".to_owned(),
                entities: vec![],
                environment: vec![],
                lifecycle: vec![],
                source_urls: vec![],
                verified_at: None,
                canonical_ref: None,
                excerpt: String::new(),
            },
        ];
        let ranked = rank_route_candidates(
            candidates,
            &HashSet::from(["memory/topic.md".to_owned()]),
            "demo",
            2,
        );
        assert_eq!(ranked[0].kind, "stable_fact");
    }
}
