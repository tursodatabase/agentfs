// this is just a complete enumeration of everything in the RFC
#![allow(dead_code)]
// And its nice to keep the original RFC names and case
#![allow(non_camel_case_types)]

use super::xdr::*;
use byteorder::{ReadBytesExt, WriteBytesExt};
use num_derive::{FromPrimitive, ToPrimitive};
use num_traits::cast::FromPrimitive;
use std::io::{Read, Write};
// Transcribed from RFC 1057 Appendix A

pub const PROGRAM: u32 = 100005;
pub const VERSION: u32 = 3;

pub const MNTPATHLEN: u32 = 1024; /* Maximum bytes in a path name */
pub const MNTNAMLEN: u32 = 255; /* Maximum bytes in a name */
pub const FHSIZE3: u32 = 64; /* Maximum bytes in a V3 file handle */

pub type fhandle3 = Vec<u8>;
pub type dirpath = Vec<u8>;
pub type name = Vec<u8>;

#[allow(non_camel_case_types)]
#[derive(Copy, Clone, Debug, FromPrimitive, ToPrimitive)]
#[repr(u32)]
pub enum mountstat3 {
    MNT3_OK = 0,                 /* no error */
    MNT3ERR_PERM = 1,            /* Not owner */
    MNT3ERR_NOENT = 2,           /* No such file or directory */
    MNT3ERR_IO = 5,              /* I/O error */
    MNT3ERR_ACCES = 13,          /* Permission denied */
    MNT3ERR_NOTDIR = 20,         /* Not a directory */
    MNT3ERR_INVAL = 22,          /* Invalid argument */
    MNT3ERR_NAMETOOLONG = 63,    /* Filename too long */
    MNT3ERR_NOTSUPP = 10004,     /* Operation not supported */
    MNT3ERR_SERVERFAULT = 10006, /* A failure on the server */
}
XDREnumSerde!(mountstat3);
