//! Input and output types of the grep tool, shared with the crates that draw them

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::serde_lenient::{deserialize_lenient_bool, deserialize_lenient_u64};

/// Emits `"type": "number"` with no format or minimum, which is what the CLI schema has for numbers
pub struct LenientNumberSchema;

impl schemars::JsonSchema for LenientNumberSchema {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "lenient_number_schema".into()
    }

    fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({ "type": "number" })
    }
}

/// [`LenientNumberSchema`] with `"minimum": 0`, for `head_limit` and `offset`
pub struct LenientNumberSchemaMin0;

impl schemars::JsonSchema for LenientNumberSchemaMin0 {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "lenient_number_min0_schema".into()
    }

    fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({ "type": "number", "minimum": 0 })
    }
}

/// Reads an `Option<u32>` from a JSON number or a numeric string
pub fn deserialize_lenient_u32<'de, D>(deserializer: D) -> Result<Option<u32>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    deserialize_lenient_u64(deserializer)?
        .map(|u| {
            u32::try_from(u).map_err(|_| serde::de::Error::custom("number out of range for u32"))
        })
        .transpose()
}

/// The grep tool's `output_mode` values, in the CLI schema's spelling
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub enum GrepOutputMode {
    #[serde(rename = "content")]
    Content,
    #[serde(rename = "files_with_matches")]
    FilesWithMatches,
    #[serde(rename = "count")]
    Count,
}

/// The grep tool's arguments under the names the model sends them
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct GrepToolInput {
    #[schemars(description = "The regular expression pattern to search for in file contents")]
    pub pattern: String,

    #[schemars(
        description = "File or directory to search in (rg pattern -- PATH). Defaults to the workspace root."
    )]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,

    #[schemars(
        description = "Glob pattern to filter files (e.g. \"*.js\", \"*.{ts,tsx}\") - maps to rg --glob"
    )]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub glob: Option<String>,

    #[schemars(
        description = "Output mode: \"content\" shows matching lines (supports -A/-B/-C context, -n line numbers, head_limit), \"files_with_matches\" shows file paths (supports head_limit), \"count\" shows match counts (supports head_limit). Defaults to \"content\"."
    )]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_mode: Option<GrepOutputMode>,

    #[schemars(
        description = "Number of lines to show before each match (rg -B). Requires output_mode: \"content\", ignored otherwise."
    )]
    #[schemars(with = "LenientNumberSchema")]
    #[serde(
        default,
        deserialize_with = "deserialize_lenient_u32",
        rename = "-B",
        skip_serializing_if = "Option::is_none"
    )]
    pub before_context: Option<u32>,

    #[schemars(
        description = "Number of lines to show after each match (rg -A). Requires output_mode: \"content\", ignored otherwise."
    )]
    #[schemars(with = "LenientNumberSchema")]
    #[serde(
        default,
        deserialize_with = "deserialize_lenient_u32",
        rename = "-A",
        skip_serializing_if = "Option::is_none"
    )]
    pub after_context: Option<u32>,

    #[schemars(
        description = "Number of lines to show before and after each match (rg -C). Requires output_mode: \"content\", ignored otherwise."
    )]
    #[schemars(with = "LenientNumberSchema")]
    #[serde(
        default,
        deserialize_with = "deserialize_lenient_u32",
        rename = "-C",
        skip_serializing_if = "Option::is_none"
    )]
    pub context: Option<u32>,

    // The CLI spec text verbatim, missing period included
    #[schemars(description = "Case insensitive search (rg -i) Defaults to false")]
    #[serde(default, rename = "-i", deserialize_with = "deserialize_lenient_bool")]
    pub case_insensitive: bool,

    #[schemars(
        description = "File type to search (rg --type). Common types: js, py, rust, go, java, etc. More efficient than include for standard file types."
    )]
    #[serde(default, rename = "type", skip_serializing_if = "Option::is_none")]
    pub file_type: Option<String>,

    #[schemars(
        description = "Limit output size. For \"content\" mode: limits total matches shown. For \"files_with_matches\" and \"count\" modes: limits number of files."
    )]
    #[schemars(with = "LenientNumberSchemaMin0")]
    #[serde(
        default,
        deserialize_with = "deserialize_lenient_u32",
        skip_serializing_if = "Option::is_none"
    )]
    pub head_limit: Option<u32>,

    #[schemars(
        description = "Skip first N entries. For \"content\" mode: skips first N matches. For \"files_with_matches\" and \"count\" modes: skips first N files. Use with head_limit for pagination."
    )]
    #[schemars(with = "LenientNumberSchemaMin0")]
    #[serde(
        default,
        deserialize_with = "deserialize_lenient_u32",
        skip_serializing_if = "Option::is_none"
    )]
    pub offset: Option<u32>,

    #[schemars(
        description = "Enable multiline mode where . matches newlines and patterns can span lines (rg -U --multiline-dotall). Default: false."
    )]
    #[serde(default, deserialize_with = "deserialize_lenient_bool")]
    pub multiline: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct GrepLineMatch {
    pub line_number: usize,
    pub content: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct GrepFileMatch {
    pub path: String,
    pub matches: Vec<GrepLineMatch>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct GrepSearchOutput {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub exit_code: i32,
    pub match_count: usize,
    #[serde(default)]
    pub file_matches: Vec<GrepFileMatch>,
}
