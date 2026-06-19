//! Regression tests for guarded SETATTR handling.
//!
//! A stale `sattrguard3::obj_ctime` must cause `NFS3ERR_NOT_SYNC`, must not
//! apply the requested mutation, and must produce exactly one NFS reply.

use crate::nfsserve::nfs::{self, nfsstat3};
use crate::nfsserve::rpc::rpc_msg;
use crate::nfsserve::xdr::XDR;
use std::io::Cursor;

use super::nfs_handlers_write_perm_test::{create_file_with_mode, make_context, status_code};
use super::{nfsproc3_setattr, sattrguard3, SETATTR3args};

fn stale_guard(ctime: nfs::nfstime3) -> sattrguard3 {
    sattrguard3::obj_ctime(nfs::nfstime3 {
        seconds: ctime.seconds.wrapping_add(1),
        nseconds: ctime.nseconds,
    })
}

async fn run_setattr(
    ctx: &crate::nfsserve::context::RPCContext,
    fileid: nfs::fileid3,
    new_attribute: nfs::sattr3,
    guard: sattrguard3,
) -> (nfsstat3, usize, usize) {
    let args = SETATTR3args {
        object: ctx.vfs.id_to_fh(fileid),
        new_attribute,
        guard,
    };

    let mut input = Vec::new();
    args.serialize(&mut input).expect("serialize SETATTR3args");

    let mut out = Vec::new();
    let mut reader = Cursor::new(input);
    nfsproc3_setattr(0x5678, &mut reader, &mut out, ctx)
        .await
        .expect("handler ran");

    let total_len = out.len();
    let mut cursor = Cursor::new(out);
    let mut reply = rpc_msg::default();
    reply.deserialize(&mut cursor).expect("deserialize rpc_msg");
    let mut status = nfsstat3::NFS3_OK;
    status.deserialize(&mut cursor).expect("deserialize status");
    let mut wcc = nfs::wcc_data::default();
    wcc.deserialize(&mut cursor).expect("deserialize wcc_data");

    (status, cursor.position() as usize, total_len)
}

#[tokio::test]
async fn stale_guarded_setattr_returns_not_sync_once_and_does_not_mutate() {
    let (ctx, _dir) = make_context().await;
    let fileid = create_file_with_mode(&ctx, "guarded", 0o644).await;
    let attr_before = ctx.vfs.getattr(fileid).await.expect("getattr before");

    let new_attribute = nfs::sattr3 {
        mode: nfs::set_mode3::mode(0o600),
        ..Default::default()
    };
    let (status, consumed_len, total_len) =
        run_setattr(&ctx, fileid, new_attribute, stale_guard(attr_before.ctime)).await;

    assert_eq!(
        status_code(&status),
        status_code(&nfsstat3::NFS3ERR_NOT_SYNC),
        "stale guard must reject SETATTR with NFS3ERR_NOT_SYNC"
    );
    assert_eq!(
        consumed_len, total_len,
        "handler must serialize exactly one SETATTR reply on guard mismatch"
    );

    let attr_after = ctx.vfs.getattr(fileid).await.expect("getattr after");
    assert_eq!(
        attr_after.mode & 0o777,
        attr_before.mode & 0o777,
        "stale guarded SETATTR must not apply the requested mode change"
    );
}
