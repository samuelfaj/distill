// Modified for Distill by Samuel Fajreldines, 2026.
use agent_client_protocol as acp;
use distill_tools::implementations::distill::{
    IMAGE_GEN_TOOL_NAME, IMAGINE_COMMAND_NAME, imagine_instruction, imagine_usage_message,
};

use crate::slash::command::{CommandExecCtx, CommandResult, SlashCommand, slash_meta};

const REQUIRED_TOOLS: &[&str] = &[IMAGE_GEN_TOOL_NAME];

pub struct ImagineCommand;

impl SlashCommand for ImagineCommand {
    slash_meta! {
        name: IMAGINE_COMMAND_NAME,
        description: "Generate an image from a text description",
        usage: "/imagine <description>",
        takes_args: true,
        args_required: true,
        arg_placeholder: "description of the image to generate",
        required_tools: REQUIRED_TOOLS,
    }

    fn run(&self, _ctx: &mut CommandExecCtx, args: &str) -> CommandResult {
        let prompt = args.trim();
        if prompt.is_empty() {
            return CommandResult::Message(imagine_usage_message().to_string());
        }

        CommandResult::InjectSkill {
            display_text: format!("/imagine {prompt}"),
            prompt_blocks: vec![acp::ContentBlock::Text(acp::TextContent::new(
                imagine_instruction(prompt),
            ))],
            display_as_skill: false,
            scheduled_task_preview: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requires_image_gen_tool() {
        assert_eq!(ImagineCommand.required_tools(), &["image_gen"]);
    }

    #[test]
    fn empty_prompt_returns_usage() {
        let models = crate::acp::model_state::ModelState::default();
        let mut ctx = super::super::tests::make_ctx(&models);
        let result = ImagineCommand.run(&mut ctx, "");
        assert!(matches!(result, CommandResult::Message(_)));
    }

    #[test]
    fn whitespace_prompt_returns_usage() {
        let models = crate::acp::model_state::ModelState::default();
        let mut ctx = super::super::tests::make_ctx(&models);
        let result = ImagineCommand.run(&mut ctx, "   ");
        assert!(matches!(result, CommandResult::Message(_)));
    }

    #[test]
    fn valid_prompt_returns_inject_skill() {
        let models = crate::acp::model_state::ModelState::default();
        let mut ctx = super::super::tests::make_ctx(&models);
        let result = ImagineCommand.run(&mut ctx, "a golden sunset");
        match result {
            CommandResult::InjectSkill {
                display_text,
                prompt_blocks,
                display_as_skill,
                ..
            } => {
                assert_eq!(display_text, "/imagine a golden sunset");
                assert!(!display_as_skill);
                assert_eq!(prompt_blocks.len(), 1);
                let [acp::ContentBlock::Text(t)] = prompt_blocks.as_slice() else {
                    panic!("expected Text block, got {prompt_blocks:?}");
                };
                let text = &t.text;
                assert!(text.contains("image_gen"));
                assert!(text.contains("a golden sunset"));
            }
            other => panic!("expected InjectSkill, got {other:?}"),
        }
    }
}
