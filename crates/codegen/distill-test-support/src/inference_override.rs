//! What answers an inference request ahead of the fallback modes, tried in this order: a named
//! expectation matching the request, the endpoint's compatibility FIFO, the required auth check,
//! the request's conversation script, then the concurrency cap.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use axum::Json;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use serde_json::json;

use crate::conversation_replay::ConversationReplay;
use crate::failure::ObservedFailure;
use crate::inference_request::{
    InferenceEndpoint, InferenceRequest, InferenceRequestKind, RepostIdentity, model_name,
};
use crate::scripted::{BoxWait, ScriptedResponse, TerminalWait};

/// Typed match criteria for one named inference response.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InferenceRequestMatcher {
    endpoint: InferenceEndpoint,
    kind: InferenceRequestKind,
}

impl InferenceRequestMatcher {
    /// Match a user-facing agent turn on the selected endpoint.
    pub fn foreground(endpoint: InferenceEndpoint) -> Self {
        InferenceRequestMatcher {
            endpoint,
            kind: InferenceRequestKind::Foreground,
        }
    }

    /// Match title, classifier, prompt-suggestion, or other side-channel work.
    pub fn auxiliary(endpoint: InferenceEndpoint) -> Self {
        InferenceRequestMatcher {
            endpoint,
            kind: InferenceRequestKind::Auxiliary,
        }
    }

    fn matches(self, request: &InferenceRequest<'_>) -> bool {
        self.endpoint == request.endpoint() && self.kind == request.kind()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExpectationPhase {
    Pending,
    Received,
    Blocked,
    Satisfied,
}

struct ExpectationControl {
    name: String,
    phase_tx: tokio::sync::watch::Sender<ExpectationPhase>,
    claims_tx: tokio::sync::watch::Sender<usize>,
    release_tx: tokio::sync::watch::Sender<bool>,
}

impl ExpectationControl {
    fn set_phase(&self, phase: ExpectationPhase) {
        self.phase_tx.send_replace(phase);
    }

    fn release(&self) {
        self.release_tx.send_replace(true);
    }

    fn claim(&self) {
        self.claims_tx.send_modify(|claims| *claims += 1);
    }

    async fn wait_for_release(&self) {
        let mut release_rx = self.release_tx.subscribe();
        if *release_rx.borrow_and_update() {
            return;
        }
        release_rx
            .wait_for(|released| *released)
            .await
            .expect("expectation release sender lives with the claimed response");
    }

    async fn wait_claims(&self, target: usize) {
        let mut claims_rx = self.claims_tx.subscribe();
        claims_rx
            .wait_for(|claims| *claims >= target)
            .await
            .expect("expectation claims sender lives with the control");
    }
}

/// Handle for one registered inference expectation: lets a test wait on its phases and release its terminal barrier.
#[must_use = "expectation handles provide synchronization and satisfaction checks"]
pub struct InferenceExpectation {
    control: Arc<ExpectationControl>,
    phase_rx: tokio::sync::watch::Receiver<ExpectationPhase>,
}

impl InferenceExpectation {
    pub fn name(&self) -> &str {
        &self.control.name
    }

    pub fn is_satisfied(&self) -> bool {
        *self.phase_rx.borrow() == ExpectationPhase::Satisfied
    }

    /// Wait until one request atomically claims this expectation.
    pub async fn wait_received(&mut self) {
        self.wait_for(ExpectationPhase::Received).await;
    }

    /// Wait until the response reaches its terminal-event barrier.
    pub async fn wait_blocked(&mut self) {
        self.wait_for(ExpectationPhase::Blocked).await;
    }

    /// Wait until the primary response crosses its terminal event.
    pub async fn wait_satisfied(&mut self) {
        self.wait_for(ExpectationPhase::Satisfied).await;
    }

    /// Wait until `target` requests have claimed this expectation, overlapping duplicates included.
    pub async fn wait_claims(&self, target: usize) {
        self.control.wait_claims(target).await;
    }

    /// Release this expectation's terminal barrier.
    pub fn release(&self) {
        self.control.release();
    }

    /// Panic with the expectation name and phase unless satisfied.
    pub fn assert_satisfied(&self) {
        assert!(
            self.is_satisfied(),
            "inference expectation `{}` was not satisfied (state: {:?})",
            self.name(),
            *self.phase_rx.borrow()
        );
    }

    /// Describe the expectation for aggregation in test failure output.
    pub fn diagnostic(&self) -> String {
        format!(
            "inference expectation `{}` (state: {:?})",
            self.name(),
            *self.phase_rx.borrow()
        )
    }

    async fn wait_for(&mut self, target: ExpectationPhase) {
        if self
            .phase_rx
            .wait_for(|phase| Self::phase_reached(*phase, target))
            .await
            .is_err()
        {
            panic!(
                "inference expectation `{}` closed before reaching {target:?} (state: {:?})",
                self.control.name,
                *self.phase_rx.borrow()
            );
        }
    }

    fn phase_reached(current: ExpectationPhase, target: ExpectationPhase) -> bool {
        match target {
            ExpectationPhase::Pending => true,
            ExpectationPhase::Received => current != ExpectationPhase::Pending,
            ExpectationPhase::Blocked => matches!(
                current,
                ExpectationPhase::Blocked | ExpectationPhase::Satisfied
            ),
            ExpectationPhase::Satisfied => current == ExpectationPhase::Satisfied,
        }
    }
}

impl Drop for InferenceExpectation {
    fn drop(&mut self) {
        self.control.release();
    }
}

struct PendingExpectation {
    matcher: InferenceRequestMatcher,
    response: ScriptedResponse,
    block_before_terminal: bool,
    control: Arc<ExpectationControl>,
}

struct CallState {
    response: ScriptedResponse,
    block_before_terminal: bool,
    control: Arc<ExpectationControl>,
    active: usize,
    primary_crossed_terminal: bool,
}

#[derive(Default)]
struct ExpectationState {
    pending: VecDeque<PendingExpectation>,
    in_flight: HashMap<RepostIdentity, CallState>,
}

type Expectations = Arc<std::sync::Mutex<ExpectationState>>;
type ScriptQueues = Arc<std::sync::Mutex<HashMap<String, VecDeque<ScriptedResponse>>>>;

#[derive(Clone)]
struct ConcurrencyCap {
    slots: Arc<tokio::sync::Semaphore>,
    hold: Duration,
    retry_after_secs: u64,
}

#[derive(Clone)]
pub(crate) struct InferenceOverrides {
    expectations: Expectations,
    scripted: ScriptQueues,
    conversation_scripts: ConversationReplay,
    completion_gate: Arc<CompletionGate>,
    required_token: Option<Arc<str>>,
    concurrency_cap: Arc<std::sync::Mutex<Option<ConcurrencyCap>>>,
}

impl InferenceOverrides {
    pub(crate) fn new(required_token: Option<String>) -> Self {
        InferenceOverrides {
            expectations: Arc::new(std::sync::Mutex::new(ExpectationState::default())),
            scripted: Arc::new(std::sync::Mutex::new(HashMap::new())),
            conversation_scripts: ConversationReplay::default(),
            completion_gate: Arc::new(CompletionGate::default()),
            required_token: required_token.map(Arc::from),
            concurrency_cap: Arc::new(std::sync::Mutex::new(None)),
        }
    }

    pub(crate) fn conversation_scripts(&self) -> &ConversationReplay {
        &self.conversation_scripts
    }

    pub(crate) fn set_concurrency_cap(&self, cap: usize, hold: Duration, retry_after_secs: u64) {
        *self.concurrency_cap.lock().unwrap() = Some(ConcurrencyCap {
            slots: Arc::new(tokio::sync::Semaphore::new(cap)),
            hold,
            retry_after_secs,
        });
    }

    /// Reports to `note_failure` what the mock decided on its own before any hold, so a stall still
    /// shows it; an enqueued answer is what the test asked for, not a failure.
    pub(crate) async fn response_override(
        &self,
        request: &InferenceRequest<'_>,
        delay: Option<Duration>,
        note_failure: impl FnOnce(ObservedFailure),
    ) -> Option<Response> {
        if let Some(claimed) = self.claim_expectation(request) {
            let (response, wait) = claimed.into_parts();
            return Some(response.into_response_paced(delay, Some(wait)).await);
        }

        if let Some(response) = self.pop_scripted(request.endpoint().path()) {
            let wait = if response.is_sse() {
                self.fallback_terminal_wait(request)
            } else {
                None
            };
            return Some(response.into_response_paced(delay, wait).await);
        }

        if let Some(rejection) = self.auth_rejection(request.headers()) {
            note_failure(ObservedFailure::Status(401));
            return Some(rejection);
        }

        if let Some(served) = self.conversation_scripts.respond(request) {
            if let Some(observed) = served.observed {
                note_failure(observed);
            }
            if let Some(hold) = served.hold {
                tokio::time::sleep(hold).await;
            }
            let response = served
                .reply
                .into_response(request.endpoint(), model_name(request.body()));
            // A refusal or a dropped connection has no terminal event to hold on the gate.
            let wait = if response.is_sse() {
                self.fallback_terminal_wait(request)
            } else {
                None
            };
            return Some(response.into_response_paced(delay, wait).await);
        }

        let cap = self.concurrency_cap.lock().unwrap().clone()?;
        match Arc::clone(&cap.slots).try_acquire_owned() {
            Ok(permit) => {
                tokio::time::sleep(cap.hold).await;
                drop(permit);
                None
            }
            Err(_) => {
                note_failure(ObservedFailure::Status(429));
                let mut reply = ScriptedResponse::text(429, "concurrent request cap exceeded");
                reply
                    .headers
                    .push(("retry-after".to_owned(), cap.retry_after_secs.to_string()));
                Some(reply.into_response_paced(delay, None).await)
            }
        }
    }

    pub(crate) fn register_expectation(
        &self,
        name: impl Into<String>,
        matcher: InferenceRequestMatcher,
        response: ScriptedResponse,
        block_before_terminal: bool,
    ) -> InferenceExpectation {
        response.validate();
        let name = name.into();
        let mut expectations = self.expectations.lock().unwrap();
        assert!(
            expectations
                .pending
                .iter()
                .all(|expectation| expectation.control.name != name)
                && expectations
                    .in_flight
                    .values()
                    .all(|expectation| expectation.control.name != name),
            "duplicate inference expectation name `{name}`"
        );
        let (phase_tx, phase_rx) = tokio::sync::watch::channel(ExpectationPhase::Pending);
        let (claims_tx, _claims_rx) = tokio::sync::watch::channel(0);
        let (release_tx, _release_rx) = tokio::sync::watch::channel(!block_before_terminal);
        let control = Arc::new(ExpectationControl {
            name,
            phase_tx,
            claims_tx,
            release_tx,
        });
        expectations.pending.push_back(PendingExpectation {
            matcher,
            response,
            block_before_terminal,
            control: control.clone(),
        });
        InferenceExpectation { control, phase_rx }
    }

    pub(crate) fn enqueue_response(&self, path: impl Into<String>, response: ScriptedResponse) {
        response.validate();
        self.scripted
            .lock()
            .unwrap()
            .entry(path.into())
            .or_default()
            .push_back(response);
    }

    pub(crate) fn pop_scripted(&self, path: &str) -> Option<ScriptedResponse> {
        self.scripted
            .lock()
            .unwrap()
            .get_mut(path)
            .and_then(VecDeque::pop_front)
    }

    pub(crate) fn fallback_terminal_wait(
        &self,
        request: &InferenceRequest<'_>,
    ) -> Option<TerminalWait> {
        if request.kind() != InferenceRequestKind::Foreground {
            return None;
        }
        let completion_gate = self.completion_gate.clone();
        Some(Box::new(move || {
            Box::pin(async move { completion_gate.wait_if_held().await })
        }))
    }

    pub(crate) fn hold_completions(&self) {
        self.completion_gate.hold();
    }

    pub(crate) fn release_completions(&self) {
        self.completion_gate.release();
    }

    fn claim_expectation(&self, request: &InferenceRequest<'_>) -> Option<ClaimedExpectation> {
        let mut expectations = self.expectations.lock().unwrap();
        if let Some(identity) = request.repost_identity()
            && let Some(call) = expectations.in_flight.get_mut(identity)
            && call.active > 0
        {
            call.active += 1;
            call.control.claim();
            return Some(ClaimedExpectation {
                response: call.response.clone(),
                lease: ClaimLease::new(
                    self.expectations.clone(),
                    Some(identity.clone()),
                    call.control.clone(),
                    call.block_before_terminal,
                    ClaimRole::Replay,
                ),
            });
        }

        let index = expectations
            .pending
            .iter()
            .position(|expectation| expectation.matcher.matches(request))?;
        let expectation = expectations
            .pending
            .remove(index)
            .expect("matched expectation index must remain valid");
        expectation.control.set_phase(ExpectationPhase::Received);
        expectation.control.claim();
        let lease = ClaimLease::new(
            self.expectations.clone(),
            request.repost_identity().cloned(),
            expectation.control.clone(),
            expectation.block_before_terminal,
            ClaimRole::Primary,
        );
        if let Some(identity) = request.repost_identity().cloned() {
            let replaced = expectations.in_flight.insert(
                identity,
                CallState {
                    response: expectation.response.clone(),
                    block_before_terminal: expectation.block_before_terminal,
                    control: expectation.control,
                    active: 1,
                    primary_crossed_terminal: false,
                },
            );
            assert!(replaced.is_none(), "duplicate in flight repost identity");
        }
        Some(ClaimedExpectation {
            response: expectation.response,
            lease,
        })
    }

    pub(crate) fn auth_rejection(&self, headers: &HeaderMap) -> Option<Response> {
        let expected = self.required_token.as_deref()?;
        let valid = headers
            .get("authorization")
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| {
                value
                    .strip_prefix("Bearer ")
                    .or_else(|| value.strip_prefix("bearer "))
                    .is_some_and(|token| token == expected)
            });
        if valid {
            return None;
        }
        Some(
            (
                StatusCode::UNAUTHORIZED,
                Json(json!({
                    "error": "missing API key; set the x-api-key header or Authorization: Bearer header"
                })),
            )
                .into_response(),
        )
    }
}

#[derive(Clone)]
enum ClaimRole {
    Primary,
    Replay,
}

struct ClaimedExpectation {
    response: ScriptedResponse,
    lease: ClaimLease,
}

impl ClaimedExpectation {
    fn into_parts(self) -> (ScriptedResponse, TerminalWait) {
        let response = self.response;
        let mut lease = self.lease;
        let wait = Box::new(move || {
            Box::pin(async move {
                if lease.block_before_terminal {
                    lease.mark_blocked();
                    lease.control.wait_for_release().await;
                }
                lease.crossed_terminal = true;
                lease.finish();
            }) as BoxWait
        });
        (response, wait)
    }
}

struct ClaimLease {
    expectations: Expectations,
    repost_identity: Option<RepostIdentity>,
    control: Arc<ExpectationControl>,
    block_before_terminal: bool,
    role: ClaimRole,
    crossed_terminal: bool,
    finished: bool,
}

impl ClaimLease {
    fn new(
        expectations: Expectations,
        repost_identity: Option<RepostIdentity>,
        control: Arc<ExpectationControl>,
        block_before_terminal: bool,
        role: ClaimRole,
    ) -> Self {
        ClaimLease {
            expectations,
            repost_identity,
            control,
            block_before_terminal,
            role,
            crossed_terminal: false,
            finished: false,
        }
    }

    fn mark_blocked(&self) {
        if matches!(&self.role, ClaimRole::Primary)
            && *self.control.phase_tx.borrow() != ExpectationPhase::Satisfied
        {
            self.control.set_phase(ExpectationPhase::Blocked);
        }
    }

    fn finish(&mut self) {
        if self.finished {
            return;
        }
        self.finished = true;
        self.update_shared_state();
    }

    fn update_shared_state(&self) {
        let Some(identity) = self.repost_identity.as_ref() else {
            if matches!(&self.role, ClaimRole::Primary) && self.crossed_terminal {
                self.control.set_phase(ExpectationPhase::Satisfied);
            }
            return;
        };
        let mut expectations = self.expectations.lock().unwrap();
        let Some(call) = expectations.in_flight.get_mut(identity) else {
            return;
        };
        assert!(call.active > 0, "claim active count underflow");
        call.active -= 1;
        if matches!(&self.role, ClaimRole::Primary) && self.crossed_terminal {
            call.primary_crossed_terminal = true;
        }
        if call.active == 0 {
            let control = call.control.clone();
            let satisfied = call.primary_crossed_terminal;
            expectations.in_flight.remove(identity);
            if satisfied {
                control.set_phase(ExpectationPhase::Satisfied);
            }
        }
    }
}

impl Drop for ClaimLease {
    fn drop(&mut self) {
        self.finish();
    }
}

#[derive(Default)]
struct CompletionGate {
    held: AtomicBool,
    notify: tokio::sync::Notify,
}

impl CompletionGate {
    fn hold(&self) {
        self.held.store(true, Ordering::SeqCst);
    }

    fn release(&self) {
        self.held.store(false, Ordering::SeqCst);
        self.notify.notify_waiters();
    }

    async fn wait_if_held(&self) {
        loop {
            let notified = self.notify.notified();
            if !self.held.load(Ordering::SeqCst) {
                return;
            }
            notified.await;
        }
    }
}
