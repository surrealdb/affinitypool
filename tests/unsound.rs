//! Executable demonstration of the `spawn_local` soundness hazard.
//!
//! # The bug
//!
//! `affinitypool::spawn_local` (and `Threadpool::spawn_local`) accept a
//! closure that may **borrow** non-`'static` data from the caller's
//! stack. To make that lifetime erasure work, the returned future runs a
//! blocking cancellation in its `Drop` impl: dropping the future parks
//! the current thread until the worker has stopped running the closure,
//! so the closure's borrows are guaranteed dead before they expire.
//!
//! That guarantee relies entirely on the destructor running, and Rust
//! does **not** guarantee destructors run. Safe code can skip one with
//! `std::mem::forget`, `Box::leak`, a `ManuallyDrop`, an `Rc`/`Arc`
//! cycle, or by nesting the future inside another future that is itself
//! leaked. If the future is leaked while it still borrows caller data,
//! the worker goes on to read that data after it has gone out of scope:
//! a data race and a use-after-free. This is the classic
//! "leak ⇒ unsoundness" hole — the same one that sank the pre-1.0
//! `std::thread::scoped` API.
//!
//! This test reproduces exactly that. `PollAndLeak` polls the spawn
//! future once (so the closure is scheduled and the worker parks on a
//! channel) and then `mem::forget`s it instead of dropping it. The
//! borrow region for `v` then ends, `v` is mutated, and only afterwards
//! is the worker told to read `&v`.
//!
//! # Why there is no fix in async Rust
//!
//! There is no sound, fully safe `spawn_local` in today's async Rust. A
//! future is a *value*, and safe code may always leak a value, so a
//! future's destructor can never be a load-bearing safety barrier. The
//! only construct whose completion safe code cannot skip is a returning
//! stack frame — which is why the one sound design is a *synchronous*
//! scoped API (`std::thread::scope` / `rayon::scope`), where the join
//! happens as the scope call returns. That shape cannot be expressed
//! over `.await`: awaiting hands control to an executor that is never
//! obliged to poll the future again.
//!
//! # How to use `spawn_local` safely
//!
//! Because the hazard cannot be designed out, `spawn_local` is `unsafe`.
//! The way to ensure safety is to use it correctly: **never leak the
//! returned future while it borrows non-`'static` data** — always let it
//! drop, or drive it to completion, before the borrows end. The ordinary
//! patterns (`pool.spawn_local(..).await`, or just dropping the future)
//! all uphold this; only a deliberate leak like the one below breaks it.
//! If the closure captures only `'static` data, use the safe
//! `affinitypool::spawn` instead.
//!
//! This test is run in CI but is **not** allowed to block it: it
//! exercises undefined behaviour, whose observable result is not
//! guaranteed (it usually "passes" because the freed stack slot still
//! reads back as a valid integer). It exists as an executable record of
//! the hazard, not as a correctness check — see the non-blocking
//! `unsound` job in `.github/workflows/ci.yml`.

use std::{
	pin::Pin,
	sync::mpsc,
	task::{Context, Poll},
};

/// A future adapter that polls its inner future exactly once and then,
/// if it is still `Pending`, **leaks** it via [`std::mem::forget`]
/// instead of dropping it — deliberately skipping the `SpawnFuture`
/// destructor that `spawn_local`'s soundness depends on.
struct PollAndLeak<T>(Option<Pin<Box<T>>>);

impl<T: Future<Output = ()>> Future for PollAndLeak<T> {
	type Output = ();

	fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
		let mut future = self.get_mut().0.take().unwrap();
		let res = future.as_mut().poll(cx);
		match res {
			Poll::Ready(_) => Poll::Ready(()),
			Poll::Pending => {
				std::mem::forget(future);
				Poll::Ready(())
			}
		}
	}
}

/// Drives `spawn_local` into a use-after-free by leaking its future
/// mid-flight. See the module docs for the full explanation.
///
/// `#[ignore]`d so the default `cargo test` run never executes it; CI
/// runs it explicitly in a dedicated, non-blocking job.
#[test]
#[ignore = "demonstrates undefined behaviour; run explicitly, non-blocking (see the `unsound` CI job)"]
fn trigger_unsoundness() {
	affinitypool::Builder::new().build().build_global().unwrap();
	let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();

	rt.block_on(async {
		let (send, recv) = mpsc::channel();
		let (sendback, recvback) = mpsc::channel();

		{
			let mut v = 1;
			{
				let v_ref = &v;
				println!("MAIN THREAD SPAWNING");
				// SAFETY: this call is intentionally UNSOUND — it exists to
				// demonstrate the hazard. `spawn_local`'s contract (do not
				// leak the returned future while it borrows `v`) is
				// deliberately violated below by `PollAndLeak`, which polls
				// once and then `mem::forget`s the future. Do NOT copy this
				// pattern; correct callers `.await` or drop the future.
				let future = unsafe {
					affinitypool::spawn_local(move || {
						println!("THREAD STARTING");
						//  Wait for the spawning thread to drop the v reference
						recv.recv().unwrap();
						println!("THREAD ACCESSING");
						// Access the reference.
						println!("{}", v_ref);
						println!("THREAD FINISHED");
						// Notify that we actually did so.
						sendback.send(()).unwrap();
					})
				};

				println!("MAIN THREAD POLLING ONCE");
				let future = PollAndLeak(Some(Box::pin(future)));
				future.await;
			}
			v = 2;
			std::hint::black_box(v);
		}

		// The thread has not finished yet
		println!("MAIN THREAD DROPPED REFERENCE");
		assert!(recvback.try_recv().is_err());
		println!("TELL THREAD TO CONTINUE");
		send.send(()).unwrap();
		// But it will after we send the communication over the channel.
		// At this point the thread will have accessed the reference to the already dropped v.
		println!("CHECK IF THREAD ACTUALLY RAN");
		assert!(recvback.recv().is_ok());
		println!("TEST FINISHED");
	})
}
