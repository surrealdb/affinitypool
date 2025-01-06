use crate::Threadpool;
use std::sync::OnceLock;

pub(crate) static THREADPOOL: OnceLock<Threadpool> = OnceLock::new();
