use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::Response;

use super::AppState;

/// Middleware that ensures write requests are handled by the Raft leader.
///
/// If this node is the leader, the request proceeds normally. Otherwise,
/// returns a 307 Temporary Redirect to the leader's API address so the
/// client retries there. Returns 503 if no leader is known.
pub async fn require_leader(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    // Fast path: if we're the leader, proceed.
    if state.raft.ensure_linearizable().await.is_ok() {
        return Ok(next.run(request).await);
    }

    // Look up leader address.
    let leader_id = state.raft.raft().current_leader().await;
    let leader_addr =
        leader_id.and_then(|id| state.raft.read_state(|fsm| fsm.nodes.get(&id).map(|n| n.address)));

    match leader_addr {
        Some(addr) => {
            let uri = request.uri();
            let location = format!(
                "http://{addr}{}",
                uri.path_and_query().map(|pq| pq.as_str()).unwrap_or("/")
            );
            Response::builder()
                .status(StatusCode::TEMPORARY_REDIRECT)
                .header("location", &location)
                .body(axum::body::Body::empty())
                .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
        }
        None => Err(StatusCode::SERVICE_UNAVAILABLE),
    }
}
