//! Single integration-test binary for the protocol/format core (NNTP, yEnc,
//! NZB, RAR, VFS) and the streaming layer. Shared helpers live in `support`.

mod support;

mod nntp_client;
mod provider_endpoint;
mod rar_fixtures;
mod stream_api;
mod vfs_e2e;
