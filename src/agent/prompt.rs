//! Prompt text shared by every agent adapter.

use crate::forge::ReplyTarget;

use super::{AgentContext, AgentRequest};

pub(super) fn build_prompt(request: &AgentRequest, context: &AgentContext) -> String {
    let mut prompt = String::new();
    prompt.push_str("You are an autonomous coding agent invoked from a forge comment.\n\n");

    if let Some(forge) = context.forge {
        prompt.push_str(&format!("Forge: {forge}\n"));
    }
    prompt.push_str(&format!("Location: {}\n", request.location));
    if !context.repository.is_empty() {
        prompt.push_str(&format!("Repository: {}\n", context.repository));
    }
    if let Some(title) = &context.title {
        prompt.push_str(&format!("Title: {title}\n"));
    }
    prompt.push_str(&format!(
        "Your working directory: {}\n\n",
        context.workspace.display()
    ));
    prompt.push_str("Requested work:\n");
    prompt.push_str(request.message.trim());
    prompt.push('\n');

    if context.is_issue() && !context.requester.is_empty() {
        prompt.push_str(&format!(
            "\nIf you create a pull request, request a review from the caller (@{}) on that pull request.\n",
            context.requester
        ));
    }

    if let ReplyTarget::ReviewComment(target) = &context.reply_target {
        prompt.push_str(&format!(
            "\nThis mention is an inline pull-request review comment. Post any reply in \
             the same review thread (review id {}, file `{}`, line {}) instead of \
             opening a new top-level comment.\n",
            target.review_id, target.path, target.line
        ));
    }
    prompt.push_str(
        "\nUse the tools available to you (forge CLI/API, git, shell, filesystem) to \
         gather context, make changes, run tests, and commit/push when appropriate. \
         Reply on the forge when you are done. Do not start that reply with \
         `forge-bot:` — that prefix marks the gateway's own status comments, not \
         your answer. Forge credentials are available in the environment.\n",
    );

    prompt
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::forge::ReviewCommentTarget;
    use crate::location::ForgeKind;

    fn sample() -> (AgentRequest, AgentContext) {
        (
            AgentRequest {
                location: "https://forge.example/o/r/pulls/4#issuecomment-9"
                    .parse()
                    .unwrap(),
                message: " fix this ".into(),
            },
            AgentContext {
                forge: Some(ForgeKind::Forgejo),
                repository: "o/r".into(),
                title: Some("A title".into()),
                workspace: "/tmp/ws".into(),
                requester: "alice".into(),
                is_pull_request: true,
                reply_target: ReplyTarget::ReviewComment(ReviewCommentTarget {
                    review_id: 7,
                    path: "src/lib.rs".into(),
                    line: 42,
                    extra_lines_count: 0,
                }),
                ..Default::default()
            },
        )
    }

    #[test]
    fn prompt_includes_request_context_and_reply_guidance() {
        let (request, context) = sample();
        assert_eq!(
            build_prompt(&request, &context),
            concat!(
                "You are an autonomous coding agent invoked from a forge comment.\n\n",
                "Forge: forgejo\n",
                "Location: https://forge.example/o/r/pulls/4#issuecomment-9\n",
                "Repository: o/r\n",
                "Title: A title\n",
                "Your working directory: /tmp/ws\n\n",
                "Requested work:\nfix this\n",
                "\nThis mention is an inline pull-request review comment. Post any reply in the same review thread (review id 7, file `src/lib.rs`, line 42) instead of opening a new top-level comment.\n",
                "\nUse the tools available to you (forge CLI/API, git, shell, filesystem) to gather context, make changes, run tests, and commit/push when appropriate. Reply on the forge when you are done. Do not start that reply with `forge-bot:` — that prefix marks the gateway's own status comments, not your answer. Forge credentials are available in the environment.\n",
            )
        );
    }

    #[test]
    fn prompt_forbids_the_gateway_prefix() {
        let (request, context) = sample();
        let prompt = build_prompt(&request, &context);
        assert!(
            prompt.contains("Do not start that reply with `forge-bot:`"),
            "agents must not prefix their own reply with the gateway marker: {prompt}"
        );
    }

    #[test]
    fn pull_request_conversation_omits_review_request_and_thread_guidance() {
        let request = AgentRequest {
            location: "https://forge.example/o/r/pulls/4".parse().unwrap(),
            message: "fix this".into(),
        };
        let context = AgentContext {
            repository: "o/r".into(),
            requester: "alice".into(),
            is_pull_request: true,
            ..Default::default()
        };

        let prompt = build_prompt(&request, &context);
        assert!(prompt.contains("Repository: o/r"));
        assert!(prompt.contains("fix this"));
        assert!(!prompt.contains("request a review from the caller"));
        assert!(prompt.contains("Reply on the forge"));
        assert!(!prompt.contains("inline pull-request review comment"));
    }

    #[test]
    fn issue_mention_requests_review_from_caller() {
        let request = AgentRequest {
            location: "https://forge.example/o/r/issues/4#issuecomment-9"
                .parse()
                .unwrap(),
            message: "fix this".into(),
        };
        let context = AgentContext {
            repository: "o/r".into(),
            requester: "alice".into(),
            is_pull_request: false,
            ..Default::default()
        };

        assert!(
            build_prompt(&request, &context).contains("request a review from the caller (@alice)")
        );
    }
}
