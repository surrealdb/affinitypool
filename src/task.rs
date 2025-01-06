pub(crate) type Task = Box<dyn BoxedFn + Send + 'static>;

pub(crate) trait BoxedFn {
	fn run(self: Box<Self>);
}

impl<F: FnOnce()> BoxedFn for F {
	fn run(self: Box<F>) {
		(*self)()
	}
}
