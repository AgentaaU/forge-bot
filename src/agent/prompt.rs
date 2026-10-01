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
    let working_directory = context
        .host_user
        .as_deref()
        .and_then(|user| context.executor.account(user))
        .map(|account| account.home.as_path())
        .unwrap_or(context.workspace.as_path());
    prompt.push_str(&format!(
        "Your working directory: {}\n\n",
        working_directory.display()
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

    if context.is_reviewer {
        let submitter_mention = context
            .pull_request_author
            .as_ref()
            .map(|submitter| format!("'@{submitter}'"))
            .unwrap_or_else(|| {
                "the PR submitter's login (read it from the forge PR author)".into()
            });
        prompt.push_str(&format!(
            "\nYou are acting as a reviewer. On a pull request, inspect the current diff and run relevant checks. \
             If changes are needed, post an actionable comment mentioning only {submitter_mention}, asking them to fix the findings and mention you again after updating. \
             When the review is OK, rebase the branch onto the current base and squash it to one commit, \
             verify the final diff and checks, then approve the final head and enable auto-merge using fast-forward only. \
             Do not approve or enable auto-merge while findings or checks remain unresolved.\n",
        ));
    } else if let Some(reviewer) = &context.reviewer {
        prompt.push_str(&format!(
            "\nWhenever you submit or update a pull request, post a separate comment on that PR \
             mentioning only @{reviewer} and asking them to review the current head. Include the changes \
             and validation results. Use a new comment after each update so the reviewer is invoked again.\n"
        ));
    }

    if let Some(human) = &context.human {
        prompt.push_str(&format!(
            "\nIf you need a human to do something you cannot (a privilege request, \
             an account registration, a secret, ...), post a comment mentioning only \
             @{human} and say exactly what you need and why. Continue with everything \
             else that is not blocked rather than waiting for the human.\n"
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
         Reply on the forge when you are done. Forge credentials are available in \
         the environment.\n",
    );

    prompt
}

/// Prompt injected into a live run for a same-thread follow-up.
///
/// Deliberately short: the agent already has the workspace and its earlier
/// context, so a follow-up only needs the new instruction and where it came
/// from.
pub(super) fn build_follow_up_prompt(request: &AgentRequest) -> String {
    format!(
        "A follow-up comment arrived in this thread while you were working. Treat it \
         as a continuation of the same task and act on it before you finish.\n\n\
         Follow-up at {}:\n{}",
        request.location,
        request.message.trim()
    )
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
                host_user: Some("worker".into()),
                executor: std::sync::Arc::new(crate::executor::Executor::with_accounts_for_tests(
                    std::collections::BTreeMap::from([(
                        "worker".into(),
                        crate::executor::HostAccount {
                            name: "worker".into(),
                            uid: 1000,
                            gid: 1000,
                            home: "/srv/worker".into(),
                        },
                    )]),
                )),
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
    fn submitter_and_reviewer_get_handoff_instructions() {
        let (request, mut context) = sample();
        context.reviewer = Some("review-bot".into());
        let prompt = build_prompt(&request, &context);
        assert!(prompt.contains("Whenever you submit or update a pull request"));
        assert!(prompt.contains("mentioning only @review-bot"));
        context.is_reviewer = true;
        let prompt = build_prompt(&request, &context);
        assert!(prompt.contains("PR submitter's login"));
        assert!(prompt.contains("squash it to one commit"));
        assert!(prompt.contains("approve the final head"));
        assert!(prompt.contains("fast-forward only"));
        assert!(!prompt.contains("mentioning only @review-bot"));
    }

    #[test]
    fn human_guidance_names_the_configured_human() {
        let (request, mut context) = sample();
        context.human = Some("alice".into());
        let prompt = build_prompt(&request, &context);
        assert!(prompt.contains("If you need a human"));
        assert!(prompt.contains("mentioning only @alice"));

        context.human = None;
        assert!(!build_prompt(&request, &context).contains("If you need a human"));
    }

    #[test]
    fn reviewer_prompt_names_resolved_pr_author_instead_of_requester() {
        let (request, mut context) = sample();
        context.is_reviewer = true;
        context.pull_request_author = Some("submitter-bot".into());
        let prompt = build_prompt(&request, &context);
        assert!(prompt.contains("mentioning only '@submitter-bot'"));
        assert!(!prompt.contains("read it from the forge PR author"));
        assert!(!prompt.contains("mentioning only @alice"));
    }

    #[test]
    fn follow_up_prompt_carries_only_the_new_instruction() {
        let request = AgentRequest {
            location: "https://forge.example/o/r/issues/4#issuecomment-11"
                .parse()
                .unwrap(),
            message: "  also run the linter ".into(),
        };
        let prompt = build_follow_up_prompt(&request);
        assert!(prompt.contains("follow-up comment arrived"));
        assert!(prompt.contains("https://forge.example/o/r/issues/4#issuecomment-11"));
        assert!(prompt.contains("also run the linter"));
        // It must not repeat the full initial prompt.
        assert!(!prompt.contains("You are an autonomous coding agent"));
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
                "Your working directory: /srv/worker\n\n",
                "Requested work:\nfix this\n",
                "\nThis mention is an inline pull-request review comment. Post any reply in the same review thread (review id 7, file `src/lib.rs`, line 42) instead of opening a new top-level comment.\n",
                "\nUse the tools available to you (forge CLI/API, git, shell, filesystem) to gather context, make changes, run tests, and commit/push when appropriate. Reply on the forge when you are done. Forge credentials are available in the environment.\n",
            )
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
