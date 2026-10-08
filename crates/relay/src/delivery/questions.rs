//! Serialized, identity-checked native question answers.
use super::{
    service::{
        blocking, chat_target, failed, internal, workspace_and_chats, Inner, NOT_A_TAB,
        NO_SESSION_WORKSPACE,
    },
    WriteAnswer,
};
use crate::{
    contract::Priority,
    transcript::questions::QuestionAnswer,
    ui::{actor::UiRunError, driver::UiError},
};
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AnswerQuestionsRequest {
    pub workspace_id: String,
    pub request_id: String,
    pub answers: Vec<QuestionAnswer>,
}

pub(crate) async fn answer(
    inner: Arc<Inner>,
    session_id: String,
    request: AnswerQuestionsRequest,
    priority: Priority,
) -> WriteAnswer {
    let key = serde_json::to_string(&(
        session_id.as_str(),
        request.workspace_id.as_str(),
        request.request_id.as_str(),
    ))
    .expect("string tuple serializes");
    let memo = inner.question_once.clone();
    memo.run(
        Some(&key),
        |answer| answer.status == 200 || answer.body.get("submitted") == Some(&json!(true)),
        run(inner, session_id, request, priority),
    )
    .await
    .unwrap_or_else(internal)
}

async fn run(
    inner: Arc<Inner>,
    session_id: String,
    request: AnswerQuestionsRequest,
    priority: Priority,
) -> WriteAnswer {
    let id = session_id.clone();
    let ws = request.workspace_id.clone();
    let located = blocking(&inner.reads, "questions.target", move |reads| {
        workspace_and_chats(reads, Some(&ws), Some(&id))
    })
    .await;
    let (workspace, chats) = match located {
        Err(a) => return a,
        Ok(None) => return WriteAnswer::error(404, NO_SESSION_WORKSPACE),
        Ok(Some(v)) => v,
    };
    let Some((target, _)) = chat_target(&workspace, &chats, &session_id) else {
        return WriteAnswer::error(409, NOT_A_TAB);
    };
    let reads = Arc::clone(&inner.reads);
    let job = move |driver: &mut dyn crate::ui::driver::UiDriver| {
        let question = reads
            .pending_question(&session_id)
            .map_err(|_| WriteAnswer::error(500, "Could not read the question"))?
            .filter(|q| q.id == request.request_id)
            .ok_or_else(|| WriteAnswer::error(409, &UiError::QuestionStale.to_string()))?;
        if !question.validate_answers(&request.answers) {
            return Err(WriteAnswer::error(400, "Invalid question answers"));
        }
        let guard =
            || reads.pending_question(&session_id).ok().flatten().as_ref() == Some(&question);
        driver
            .answer_questions(&target, &question, &request.answers, &guard)
            .map_err(|error| {
                if error == UiError::QuestionSubmissionUnknown {
                    WriteAnswer::json(502, json!({"error":error.to_string(),"submitted":true}))
                } else {
                    WriteAnswer::error(
                        if error == UiError::QuestionStale {
                            409
                        } else {
                            502
                        },
                        &error.to_string(),
                    )
                }
            })?;
        Ok(WriteAnswer::json(
            200,
            json!({"ok":true,"submitted":true,"requestId":question.id,"answers":request.answers}),
        ))
    };
    match inner.ui.run(priority, job).await {
        Ok(Ok(answer)) => answer,
        Ok(Err(answer)) => answer,
        Err(error @ UiRunError::Busy { waiting }) => super::agent::busy(&error, waiting),
        Err(error) => failed(&error.to_string()),
    }
}
