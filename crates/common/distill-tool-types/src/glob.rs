//! Input type of the glob tool, shared with the crates that draw its results

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// The glob tool's arguments under the names the model sends them
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct GlobToolInput {
    #[schemars(
        description = "Absolute path to directory to search for files in. If not provided, defaults to the workspace root."
    )]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_directory: Option<String>,

    #[schemars(
        description = "The glob pattern to match files against.\nPatterns not starting with \"**/\" are automatically prepended with \"**/\" to enable recursive searching.\n\nExamples:\n\t- \"*.js\" (becomes \"**/*.js\") - find all .js files\n\t- \"**/node_modules/**\" - find all node_modules directories\n\t- \"**/test/**/test_*.ts\" - find all test_*.ts files in any test directory"
    )]
    pub glob_pattern: String,
}
