//! Context-free helpers: functions that find the running context on their own
//! ([`crate::context::current`]), so tests never pass `cx` around.

use std::future::{Future, IntoFuture};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, bail};

use super::Actor;
use crate::context::{Ctx, current};
use crate::http::{Request, Response};
use crate::report::StepReport;

/// A named actor from the configuration; see [`Actor::from_config`].
///
/// # Errors
///
/// See [`Actor::from_config`].
pub fn actor(name: &str) -> Result<Actor> {
    Actor::from_config(name)
}

/// The trace of the current test (see [`crate::trace`]).
///
/// # Errors
///
/// Fails outside a running fixture, hook, test or cleanup.
pub fn current_trace() -> Result<crate::trace::TraceContext> {
    Ok(current()?.trace())
}

/// The current context (for the rare case you need it explicitly).
///
/// # Errors
///
/// Fails outside a running fixture, hook, test or cleanup.
pub fn cx() -> Result<Ctx> {
    current()
}

/// A `--var` / `RunConfig::vars` value, else the environment variable.
///
/// # Errors
///
/// Fails outside a running fixture, hook, test or cleanup, or when the variable
/// is set neither as a run variable nor in the environment.
pub fn env(key: &str) -> Result<String> {
    current()?.env(key)
}

/// Registers a prepared request as a cleanup of the current scope. It runs when
/// the scope ends (LIFO), whether the test passed or not.
///
/// ```no_run
/// # use apitest::prelude::*;
/// # fn demo(patient: Actor) -> Result<()> {
/// undo(patient.call("deletePatient").expect(204))?;
/// # Ok(())
/// # }
/// ```
///
/// # Errors
///
/// Fails outside a running fixture, hook, test or cleanup.
pub fn undo(req: Request<'static>) -> Result<()> {
    current()?.undo(req);
    Ok(())
}

/// Registers an arbitrary async cleanup of the current scope.
///
/// # Errors
///
/// Fails outside a running fixture, hook, test or cleanup.
pub fn defer<F, Fut>(label: impl Into<String>, f: F) -> Result<()>
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: Future<Output = Result<()>> + Send + 'static,
{
    current()?.defer(label, move |_cx| f());
    Ok(())
}

/// Runs a named step: it is logged, timed, listed in the report (with ✓/✗ for
/// failed tests and in `JUnit` `system-out`), and its name is added to the error.
/// Accepts any future or request: `step("…", async { … })`, `step("…", req.expect(201))`.
///
/// Outside a running context the step still runs, it is just not logged or reported.
///
/// # Errors
///
/// Returns the error of `work`, with the step name added as context.
pub fn step<T, F>(name: impl Into<String>, work: F) -> impl Future<Output = Result<T>>
where
    F: IntoFuture<Output = Result<T>>,
{
    // Convert eagerly: the returned future then only holds concrete types, which
    // keeps it `Send` (rustc cannot always prove that through generic `IntoFuture`s).
    let name = name.into();
    let work = work.into_future();

    async move {
        let cx = current().ok();
        if let Some(cx) = &cx {
            cx.log(format!("▸ {name}"));
        }

        let started = Instant::now();
        let result = work.await;
        if let Some(cx) = &cx {
            cx.record_step(StepReport {
                name: name.clone(),
                duration_ms: started.elapsed().as_secs_f64() * 1000.0,
                error: result.as_ref().err().map(|e| format!("{e:#}")),
            });
        }

        result.with_context(|| format!("step '{name}' failed"))
    }
}

/// Backoff state for [`eventually!`](crate::eventually): 50 ms growing by 1.5×
/// per attempt up to 1 s, until `timeout` has elapsed.
#[doc(hidden)]
pub struct Retry {
    /// When the first attempt started; the timeout counts from here.
    started: Instant,
    /// Total time budget for all attempts.
    timeout: Duration,
    /// Sleep before the next attempt. Never above 1 s.
    delay: Duration,
    /// Failed attempts so far, reported in the final error.
    attempts: u32,
}

impl Retry {
    /// Starts the clock for a retry loop with the given total budget.
    #[must_use]
    pub fn new(timeout: Duration) -> Self {
        Retry {
            started: Instant::now(),
            timeout,
            delay: Duration::from_millis(50),
            attempts: 0,
        }
    }

    /// Waits before the next attempt, or returns the final error once the time is up.
    ///
    /// # Errors
    ///
    /// Returns `err` (with the attempt count and elapsed time as context) when the
    /// next delay would exceed the timeout.
    pub async fn wait(&mut self, err: anyhow::Error) -> Result<()> {
        self.attempts += 1;
        if self.started.elapsed() + self.delay > self.timeout {
            return Err(err.context(format!(
                "still failing after {} attempts in {:.1}s",
                self.attempts,
                self.started.elapsed().as_secs_f64()
            )));
        }

        tokio::time::sleep(self.delay).await;
        self.delay = self.delay.mul_f32(1.5).min(Duration::from_secs(1));
        Ok(())
    }
}

/// Retries an expression until it succeeds (default: up to 10 s, backoff 50 ms → 1 s).
/// The expression is an async block body: it may use `.await` and `?`, and its
/// last expression is the `Result` to check. Evaluates to that `Result`.
///
/// ```no_run
/// # use apitest::prelude::*;
/// # use std::time::Duration;
/// # #[derive(serde::Deserialize)]
/// # struct Mail { body: String }
/// # #[derive(serde::Deserialize)]
/// # struct Job { done: bool }
/// # async fn demo(mailbox: Actor, api: Actor, email: String, id: String) -> Result<()> {
/// let mail: Mail = eventually!(mailbox.get("/messages").query("to", &email).expect(200).await?.get("/0"))?;
/// let job: Job = eventually!(within: Duration::from_secs(60), {
///     let job: Job = api.call("getJob").path("id", &id).expect(200).await?.get("")?;
///     anyhow::ensure!(job.done, "job still running");
///     Ok(job)
/// })?;
/// # Ok(())
/// # }
/// ```
///
/// A macro rather than a function taking a closure: the retried block is inlined
/// into the test's own future, so borrowing locals just works and the test stays `Send`.
#[macro_export]
macro_rules! eventually {
    (within: $timeout:expr, $($body:tt)+) => {{
        let mut __retry = $crate::dsl::Retry::new($timeout);
        loop {
            let __attempt: $crate::__private_anyhow::Result<_> = async { $($body)+ }.await;
            match __attempt {
                ::core::result::Result::Ok(v) => break ::core::result::Result::Ok(v),
                ::core::result::Result::Err(e) => {
                    if let ::core::result::Result::Err(e) = __retry.wait(e).await {
                        break ::core::result::Result::Err(e);
                    }
                }
            }
        }
    }};
    ($($body:tt)+) => {
        $crate::eventually!(within: ::std::time::Duration::from_secs(10), $($body)+)
    };
}

/// `GET` an absolute URL, e.g. a link from an e-mail, following redirects.
///
/// # Errors
///
/// Fails outside a running context, when the request cannot be sent, or when the
/// response status is 4xx/5xx (the error shows the first 300 characters of the body).
pub async fn visit(url: &str) -> Result<Response> {
    let resp = Actor::anywhere().get(url).await?;
    if resp.status >= 400 {
        bail!(
            "visiting {url} returned {}: {}",
            resp.status,
            resp.text.chars().take(300).collect::<String>()
        );
    }

    Ok(resp)
}
