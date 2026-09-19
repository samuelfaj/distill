use super::*;
use crate::session::slash_commands::BUILTIN_COMMANDS;

fn text_block(text: &str) -> acp::ContentBlock {
    acp::ContentBlock::Text(acp::TextContent::new(text))
}

#[test]
fn model_authored_resolution_uses_exact_canonical_metadata() {
    assert!(matches!(
        resolve(&[text_block("/compact preserve auth")], BUILTIN_COMMANDS),
        AuthorityResolution::StaticBuiltin(BuiltinAction::Compact {
            user_context: Some(context),
        }) if context == "preserve auth"
    ));

    for text in ["/Compact", "/COMPACT", "/yolo", "/context", "/feedback"] {
        assert!(matches!(
            resolve(&[text_block(text)], BUILTIN_COMMANDS),
            AuthorityResolution::ModelAuthoredSkillCandidate { .. }
        ));
    }
}

#[test]
fn parse_slash_prefix_extracts_name_and_args() {
    assert_eq!(
        parse_slash_prefix(&[text_block("/Compact keep aliases")]),
        Some(("Compact", "keep aliases"))
    );
    assert_eq!(parse_slash_prefix(&[text_block("not a slash")]), None);
}

#[test]
fn parse_slash_prefix_distinguishes_paths_from_commands() {
    assert_eq!(
        parse_slash_prefix(&[text_block(
            "/workspace/project/src/lib.rs has a compile error"
        )]),
        None
    );
    assert_eq!(
        parse_slash_prefix(&[text_block("/memory status")]),
        Some(("memory", "status"))
    );
}
