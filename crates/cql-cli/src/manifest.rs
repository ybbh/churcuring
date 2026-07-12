//! Parsing of the project manifest `cql.toml` and the verification config
//! `verify.toml`.
//!
//! cql.toml (doc/todo.md Phase 4):
//! ```toml
//! [package]
//! name = "shop"
//! version = "0.1.0"
//!
//! [build]
//! source_root = "src"        # default "src"
//! out_dir = "target/cql"     # default "target/cql"
//! backend = "rust"           # default "rust"; also "mududb" (proposal stage)
//!
//! [mududb]                   # when backend = "mududb" (backend-mududb.md §8, draft)
//! app = "shop"
//! sql_adapter = "off"        # "off" | "sqlite" | "postgres" | "mysql"
//! ```
//!
//! verify.toml (doc/model-check.md §6.3): depth/domain/trace/fairness.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;

/// Project manifest parsed from `cql.toml` (see module docs for the format).
#[derive(Debug, Clone, Deserialize)]
pub struct Manifest {
    pub package: Package,
    #[serde(default)]
    pub build: Build,
    #[serde(default)]
    #[allow(dead_code)] // enabled once the Phase 6 (mududb backend) lands
    pub mududb: Option<Mududb>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Package {
    pub name: String,
    #[serde(default = "default_version")]
    #[allow(dead_code)] // reserved: the version is not used by any subcommand yet
    pub version: String,
}

fn default_version() -> String {
    "0.1.0".to_string()
}

#[derive(Debug, Clone, Deserialize)]
pub struct Build {
    #[serde(default = "default_source_root")]
    pub source_root: String,
    #[serde(default = "default_out_dir")]
    pub out_dir: String,
    #[serde(default = "default_backend")]
    pub backend: String,
}

fn default_source_root() -> String {
    "src".to_string()
}
fn default_out_dir() -> String {
    "target/cql".to_string()
}
fn default_backend() -> String {
    "rust".to_string()
}

impl Default for Build {
    fn default() -> Self {
        Self {
            source_root: default_source_root(),
            out_dir: default_out_dir(),
            backend: default_backend(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)] // enabled once the Phase 6 (mududb backend) lands
pub struct Mududb {
    pub app: Option<String>,
    #[serde(default = "default_sql_adapter")]
    pub sql_adapter: String,
}

fn default_sql_adapter() -> String {
    "off".to_string()
}

/// Locate the project root: walk up from `start` to the directory containing
/// cql.toml; returns None if not found (zero-config single-file mode).
pub fn find_project_root(start: &Path) -> Option<PathBuf> {
    let mut dir = if start.is_file() {
        start.parent()?.to_path_buf()
    } else {
        start.to_path_buf()
    };
    loop {
        if dir.join("cql.toml").is_file() {
            return Some(dir);
        }
        if !dir.pop() {
            return None;
        }
    }
}

/// Load and parse `cql.toml` from a project root.
pub fn load_manifest(project_root: &Path) -> Result<Manifest, String> {
    let path = project_root.join("cql.toml");
    let text = std::fs::read_to_string(&path)
        .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    toml::from_str(&text).map_err(|e| format!("invalid {}: {e}", path.display()))
}

/// Model-checking configuration parsed from `verify.toml`
/// (doc/model-check.md §6.3). A missing file yields all defaults.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct VerifyConfig {
    #[serde(default)]
    pub depth: DepthConfig,
    /// Domain bounds: `users.rows = 3`, `users.id = "1..3"`,
    /// `orders.amount = [0.0, 50.0]` — keys are "<table>.<field>" or
    /// "<table>.rows", values are heterogeneous and kept as `toml::Value` for
    /// mc_lower to interpret.
    #[serde(default)]
    pub domain: HashMap<String, toml::Value>,
    #[serde(default)]
    pub trace: TraceConfig,
    #[serde(default)]
    pub fairness: FairnessConfig,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DepthConfig {
    #[serde(default = "default_depth")]
    pub default: u32,
    /// Per-operator overrides by operator name (same as `with depth`).
    #[serde(flatten)]
    pub per_operator: HashMap<String, u32>,
}

fn default_depth() -> u32 {
    32
}

impl Default for DepthConfig {
    fn default() -> Self {
        Self {
            default: default_depth(),
            per_operator: HashMap::new(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct TraceConfig {
    #[serde(default = "default_trace_len")]
    pub length: u32,
}

fn default_trace_len() -> u32 {
    8
}

impl Default for TraceConfig {
    fn default() -> Self {
        Self {
            length: default_trace_len(),
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct FairnessConfig {
    #[serde(default)]
    pub weak: Vec<String>,
    #[serde(default)]
    pub strong: Vec<String>,
}

/// Load `verify.toml` from a project root, falling back to defaults when the
/// file does not exist.
pub fn load_verify_config(project_root: &Path) -> Result<VerifyConfig, String> {
    let path = project_root.join("verify.toml");
    if !path.is_file() {
        return Ok(VerifyConfig::default());
    }
    let text = std::fs::read_to_string(&path)
        .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    toml::from_str(&text).map_err(|e| format!("invalid {}: {e}", path.display()))
}
