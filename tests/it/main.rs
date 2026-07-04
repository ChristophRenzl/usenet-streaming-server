//! Single integration-test binary for the protocol/format core (NNTP, yEnc,
//! NZB, RAR, VFS). Shared helpers live in `support`.

mod support;

mod nntp_client;
mod provider_endpoint;
mod rar_fixtures;
mod vfs_e2e;
