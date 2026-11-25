//! Per-client admission for model calls: concurrency slots and the token
//! budget reservation.
//!
//! Runs after `auth_middleware` on the model-call routes only (chat,
//! responses, messages). `/v1/models` and `count_tokens` are cheap metadata
//! calls that clients fire alongside a running stream, so they are exempt.
//!
//! The budget reservation needs the parsed body, so `prepare_request` makes
//! it and parks it in the [`BudgetSlot`] this layer put on the request. The
//! layer owns the slot and the concurrency permit and hands both to the
//! response body: they are released when the body ends or is dropped, after
//! the call's ledger row (and budget charge) has been written.

use std::pin::Pin;
use std::sync::{Arc, Mutex, PoisonError};
use std::task::{Context, Poll};

use axum::body::{Body, Bytes};
use axum::extract::{Request, State};
use axum::http::{HeaderValue, StatusCode};
use axum::middleware::Next;
use axum::response::Response;
use http_body::{Frame, SizeHint};

use crate::limits::{BudgetReservation, ConcurrencyPermit};
use crate::util;

use super::log::plog;
use super::prepare::{AuthedClient, auth_error};
use super::state::AppState;

/// Where `prepare_request` parks a budget reservation for this call.
#[derive(Clone, Default)]
pub(super) struct BudgetSlot(Arc<Mutex<Option<BudgetReservation>>>);

impl BudgetSlot {
    pub(super) fn fill(&self, reservation: BudgetReservation) {
        *self.0.lock().unwrap_or_else(PoisonError::into_inner) = Some(reservation);
    }
}

pub(super) async fn admission_middleware(
    State(state): State<AppState>,
    mut req: Request,
    next: Next,
) -> Response {
    let Some(AuthedClient(client)) = req.extensions().get::<AuthedClient>().cloned() else {
        return next.run(req).await;
    };
    let permit = match client.max_concurrency {
        None => None,
        Some(limit) => match state.concurrency().try_acquire(&client.name, limit) {
            Some(permit) => Some(permit),
            None => {
                let request_id = util::new_request_id();
                plog(
                    "warn",
                    "client_concurrency_limit",
                    &request_id,
                    Some(&client.name),
                    None,
                    Some("concurrency_limit"),
                );
                let message = format!(
                    "this key allows {limit} concurrent request(s) and all are in use; retry when one finishes"
                );
                let mut resp = auth_error(
                    req.uri().path(),
                    StatusCode::TOO_MANY_REQUESTS,
                    "rate_limit_error",
                    "concurrency_limit",
                    &message,
                    &request_id,
                );
                resp.headers_mut()
                    .insert("retry-after", HeaderValue::from_static("1"));
                return resp;
            }
        },
    };
    let budget = client.budget_tokens.map(|_| {
        let slot = BudgetSlot::default();
        req.extensions_mut().insert(slot.clone());
        slot
    });
    let response = next.run(req).await;
    if permit.is_none() && budget.is_none() {
        return response;
    }
    hold_until_body_done(
        response,
        Held {
            _permit: permit,
            _budget: budget,
        },
    )
}

/// Guards released together when the response body is done.
struct Held {
    _permit: Option<ConcurrencyPermit>,
    _budget: Option<BudgetSlot>,
}

/// Keep `held` alive until the response body is finished or dropped.
fn hold_until_body_done(response: Response, held: Held) -> Response {
    let (parts, body) = response.into_parts();
    Response::from_parts(
        parts,
        Body::new(GuardedBody {
            inner: body,
            _held: held,
        }),
    )
}

/// A body that owns guards; dropping the body (completion, client
/// disconnect, or error) drops them.
struct GuardedBody {
    inner: Body,
    _held: Held,
}

impl http_body::Body for GuardedBody {
    type Data = Bytes;
    type Error = axum::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        Pin::new(&mut self.inner).poll_frame(cx)
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}
