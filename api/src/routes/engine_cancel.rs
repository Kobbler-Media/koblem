//! The per-engine `POST /{engine}/cancel` policy, addressed by kind.
//!
//! 🔴 One policy, two doors. `routes::queue::cancel` is the same decision addressed
//! by job id; a second copy per engine is how `queued` ended up unreachable from
//! every engine's own cancel while the queue panel handled it fine.

use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use koblem_client::types::{Job, JobKind, JobState};
use serde_json::json;

use crate::AppState;
use crate::queue::runner::Interrupt;
use crate::repositories::gpu_jobs as store;

/// The row an engine-wide cancel means: what is on the card, or failing that the
/// next one of that kind due to start.
pub(crate) async fn current(state: &AppState, is_match: impl Fn(&Job) -> bool) -> Option<Job> {
    let live = store::live(&state.db).await.unwrap_or_default();
    if let Some(row) = live.into_iter().find(&is_match) {
        return Some(row);
    }
    let pending = store::pending(&state.db).await.unwrap_or_default();
    pending.into_iter().find(is_match)
}

/// 200 cancelled outright, 202 asked to stop, 409 too late.
pub(crate) async fn cancel_row(state: &AppState, row: &Job) -> Response {
    match row.state {
        JobState::Queued | JobState::Held => {
            state.queue.abandon_pending(&state.db, &row.id).await;
            Json(json!({ "job_id": row.id })).into_response()
        }
        // 🔴 Through the dispatcher, never straight at the engine: it owns the run, so it is the
        // only thing that can stop it AND file the terminal state.
        JobState::Running => match state.queue.get() {
            Some(d) if d.interrupt(&row.id, Interrupt::Cancel) => {
                (StatusCode::ACCEPTED, Json(json!({ "job_id": row.id }))).into_response()
            }
            _ => err(
                StatusCode::CONFLICT,
                "that render is not running on this process, or has already finished on the GPU",
            ),
        },
        // 🔴 Past the engine/disk boundary. The GPU cost is already paid and the artifact is
        // landing; killing it here loses the only copy.
        JobState::Collecting => err(
            StatusCode::CONFLICT,
            "this render has finished on the GPU and is saving its output — it cannot be \
             cancelled now",
        ),
        _ => err(StatusCode::CONFLICT, "this render has already finished"),
    }
}

pub(crate) async fn cancel_kind(state: &AppState, kind: JobKind, none_msg: &str) -> Response {
    let Some(row) = current(state, |j| j.kind == kind).await else {
        return err(StatusCode::NOT_FOUND, none_msg);
    };
    cancel_row(state, &row).await
}

fn err(status: StatusCode, msg: &str) -> Response {
    (status, Json(json!({ "error": msg }))).into_response()
}
