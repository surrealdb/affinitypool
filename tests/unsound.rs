use std::{
	pin::Pin,
	sync::mpsc,
	task::{self, Context, Poll},
};

struct PollAndLeak<T>(Option<Pin<Box<T>>>);

impl<T: Future<Output = ()>> Future for PollAndLeak<T> {
	type Output = ();

	fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
		dbg!("CALLED");
		let mut future = self.get_mut().0.take().unwrap();
		dbg!("CALLED 2");
		let res = dbg!(future.as_mut().poll(cx));
		dbg!("CALLED 3");
		match res {
			Poll::Ready(_) => Poll::Ready(()),
			Poll::Pending => {
				std::mem::forget(future);
				Poll::Ready(())
			}
		}
	}
}

#[test]
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
				let future = affinitypool::spawn_local(move || {
					println!("THREAD STARTING");
					//  Wait for the spawning thread to drop the v reference
					recv.recv().unwrap();
					println!("THREAD ACCESSING");
					// Access the reference.
					println!("{}", v_ref);
					println!("THREAD FINISHED");
					// Notify that we actually did so.
					sendback.send(()).unwrap();
				});

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
