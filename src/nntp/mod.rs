//! NNTP client: single connections ([`conn`]) and the multi-provider
//! connection pool ([`pool`]).

pub mod conn;
pub mod pool;

pub use conn::{NntpConnection, NntpError, NntpTimeouts};
pub use pool::{test_provider, NntpPool, PoolOptions, PooledConn};
