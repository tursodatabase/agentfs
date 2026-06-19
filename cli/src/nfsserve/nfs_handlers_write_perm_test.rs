//! Regression tests for write-permission handling in the NFS WRITE handler.
//!
//! These cover the macOS `agentfs run` failure where `fsync`/`close` on a file
//! whose mode bits lack owner-write (e.g. git loose objects created with
//! `git_mkstemp_mode(tmp, 0444)`) was rejected with NFS3ERR_ACCES even though the
//! descriptor was opened for writing. NFSv3 is stateless and never sees open(2),
//! so WRITE must not be gated on the file's current mode bits.

use std::io::Cursor;
use std::sync::Arc;

use num_traits::ToPrimitive;
use tokio::sync::Mutex;

use crate::nfs::AgentNFS;
use crate::nfsserve::context::RPCContext;
use crate::nfsserve::nfs::{self, nfsstat3};
use crate::nfsserve::rpc::{auth_unix, rpc_msg};
use crate::nfsserve::transaction_tracker::TransactionTracker;
use crate::nfsserve::xdr::XDR;

// Private items of the parent (nfs_handlers) module under test.
use super::{nfsproc3_write, stable_how, WRITE3args};

use agentfs_sdk::{AgentFS, AgentFSOptions, FileSystem};

/// Arbitrary non-root uid/gid that own the test files. The exact values are
/// not significant — they only need to be non-zero so owner mode bits are
/// actually enforced (root would bypass the permission check). 501/20 happen
/// to be macOS-conventional.
const OWNER_UID: u32 = 501;
const OWNER_GID: u32 = 20;

pub(super) fn status_code(status: &nfsstat3) -> u32 {
    status.to_u32().expect("nfsstat3 -> u32")
}

pub(super) async fn make_context() -> (RPCContext, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("tempdir");
    let db_path = dir.path().join("delta.db");
    let agentfs = AgentFS::open(AgentFSOptions::with_path(db_path.to_str().unwrap()))
        .await
        .expect("open AgentFS");
    let fs: Arc<Mutex<dyn FileSystem>> = Arc::new(Mutex::new(agentfs.fs));
    let vfs = Arc::new(AgentNFS::new(fs));

    let ctx = RPCContext {
        local_port: 0,
        client_addr: "127.0.0.1:0".to_string(),
        auth: auth_unix {
            stamp: 0,
            machinename: Vec::new(),
            uid: OWNER_UID,
            gid: OWNER_GID,
            gids: Vec::new(),
        },
        vfs,
        mount_signal: None,
        export_name: Arc::new("/".to_string()),
        transaction_tracker: Arc::new(TransactionTracker::new(std::time::Duration::from_secs(60))),
    };
    (ctx, dir)
}

/// Create a file in the root directory with the given mode, owned by OWNER_UID,
/// and return its fileid.
pub(super) async fn create_file_with_mode(ctx: &RPCContext, name: &str, mode: u32) -> nfs::fileid3 {
    let root = ctx.vfs.root_dir();
    let attr = nfs::sattr3 {
        mode: nfs::set_mode3::mode(mode),
        ..Default::default()
    };
    let (fileid, fattr) = ctx
        .vfs
        .create(root, &name.as_bytes().into(), attr, &ctx.auth)
        .await
        .expect("create file");
    assert_eq!(fattr.mode & 0o777, mode & 0o777, "mode should be set");
    fileid
}

/// Drive nfsproc3_write and return the NFS status code from the reply.
async fn run_write(ctx: &RPCContext, fileid: nfs::fileid3, offset: u64, data: &[u8]) -> nfsstat3 {
    let args = WRITE3args {
        file: ctx.vfs.id_to_fh(fileid),
        offset,
        count: data.len() as u32,
        stable: stable_how::FILE_SYNC as u32,
        data: data.to_vec(),
    };

    let mut input = Vec::new();
    args.serialize(&mut input).expect("serialize WRITE3args");

    let mut out = Vec::new();
    let mut reader = Cursor::new(input);
    nfsproc3_write(0x1234, &mut reader, &mut out, ctx)
        .await
        .expect("handler ran");

    // Reply layout: rpc_msg (success reply) followed by the nfsstat3 status.
    let mut cursor = Cursor::new(out);
    let mut reply = rpc_msg::default();
    reply.deserialize(&mut cursor).expect("deserialize rpc_msg");
    let mut status = nfsstat3::NFS3_OK;
    status.deserialize(&mut cursor).expect("deserialize status");
    status
}

/// THE REGRESSION: a file whose mode lacks owner-write (0o444) must still accept
/// a WRITE from its owner. Pre-fix this returned NFS3ERR_ACCES, which surfaced as
/// "Permission denied" on git's fsync/close of a loose object.
#[tokio::test]
async fn write_succeeds_on_readonly_mode_file() {
    let (ctx, _dir) = make_context().await;
    let fileid = create_file_with_mode(&ctx, "loose-object", 0o444).await;

    let status = run_write(&ctx, fileid, 0, &[b'x'; 64]).await;
    assert_eq!(
        status_code(&status),
        status_code(&nfsstat3::NFS3_OK),
        "WRITE on a 0444-mode file owned by the caller must succeed \
         (regression: was NFS3ERR_ACCES = {})",
        status_code(&nfsstat3::NFS3ERR_ACCES)
    );

    // Guard against a vacuous test: confirm the bytes actually landed.
    let (read_back, _eof) = ctx.vfs.read(fileid, 0, 64).await.expect("read back");
    assert_eq!(
        read_back,
        vec![b'x'; 64],
        "written data should be persisted"
    );
}

/// A writable-mode file (0o644) must also accept WRITE — guards against the fix
/// regressing the ordinary path.
#[tokio::test]
async fn write_succeeds_on_writable_mode_file() {
    let (ctx, _dir) = make_context().await;
    let fileid = create_file_with_mode(&ctx, "normal", 0o644).await;

    let status = run_write(&ctx, fileid, 0, b"hello").await;
    assert_eq!(
        status_code(&status),
        status_code(&nfsstat3::NFS3_OK),
        "WRITE on a 0644 file must succeed"
    );

    let (read_back, _eof) = ctx.vfs.read(fileid, 0, 5).await.expect("read back");
    assert_eq!(read_back, b"hello".to_vec());
}
