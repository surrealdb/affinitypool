use thiserror::Error;

#[derive(Error, Debug)]
pub enum Error {
	#[error("A global threadpool has already been initialised")]
	GlobalThreadpoolExists,
}
