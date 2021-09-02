// Copyright (C) 2021 Alibaba Cloud. All rights reserved.
// Copyright 2019 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE-BSD-3-Clause file.

use std::io::{self, IoSlice, Write};
use std::mem::size_of;
use std::sync::Arc;
use std::time::Duration;

use vm_memory::ByteValued;

use super::{
    MetricsHook, Server, ServerUtil, ServerVersion, SrvContext, ZcReader, ZcWriter, DIRENT_PADDING,
    MAX_BUFFER_SIZE, MAX_REQ_PAGES,
};
use crate::abi::linux_abi::*;
#[cfg(feature = "virtiofs")]
use crate::abi::virtio_fs::{RemovemappingIn, RemovemappingOne, SetupmappingIn};
use crate::api::filesystem::{DirEntry, Entry, FileSystem, GetxattrReply, ListxattrReply};
use crate::async_util::AsyncDrive;
use crate::transport::{FsCacheReqHandler, Reader, Writer};
use crate::{bytes_to_cstr, encode_io_error_kind, BitmapSlice, Error, Result};

impl<F: FileSystem<S> + Sync, D: AsyncDrive, S: BitmapSlice> Server<F, D, S> {
    /// Main entrance to handle requests from the transport layer.
    ///
    /// It receives Fuse requests from transport layers, parses the request according to Fuse ABI,
    /// invokes filesystem drivers to server the requests, and eventually send back the result to
    /// the transport layer.
    #[allow(unused_variables)]
    pub fn handle_message(
        &self,
        mut r: Reader<'_, S>,
        w: Writer<'_, S>,
        vu_req: Option<&mut dyn FsCacheReqHandler>,
        hook: Option<&dyn MetricsHook>,
    ) -> Result<usize> {
        let in_header: InHeader = r.read_obj().map_err(Error::DecodeMessage)?;
        let mut ctx = SrvContext::<F, D, S>::new(in_header, r, w);
        if ctx.in_header.len > MAX_BUFFER_SIZE {
            return ctx.reply_error_explicit(io::Error::from_raw_os_error(libc::ENOMEM));
        }

        trace!(
            "fuse: new req {:?}: {:?}",
            Opcode::from(in_header.opcode),
            in_header
        );

        hook.map_or((), |h| h.collect(&in_header));

        let res = match in_header.opcode {
            x if x == Opcode::Lookup as u32 => self.lookup(ctx),
            x if x == Opcode::Forget as u32 => self.forget(ctx), // No reply.
            x if x == Opcode::Getattr as u32 => self.getattr(ctx),
            x if x == Opcode::Setattr as u32 => self.setattr(ctx),
            x if x == Opcode::Readlink as u32 => self.readlink(ctx),
            x if x == Opcode::Symlink as u32 => self.symlink(ctx),
            x if x == Opcode::Mknod as u32 => self.mknod(ctx),
            x if x == Opcode::Mkdir as u32 => self.mkdir(ctx),
            x if x == Opcode::Unlink as u32 => self.unlink(ctx),
            x if x == Opcode::Rmdir as u32 => self.rmdir(ctx),
            x if x == Opcode::Rename as u32 => self.rename(ctx),
            x if x == Opcode::Link as u32 => self.link(ctx),
            x if x == Opcode::Open as u32 => self.open(ctx),
            x if x == Opcode::Read as u32 => self.read(ctx),
            x if x == Opcode::Write as u32 => self.write(ctx),
            x if x == Opcode::Statfs as u32 => self.statfs(ctx),
            x if x == Opcode::Release as u32 => self.release(ctx),
            x if x == Opcode::Fsync as u32 => self.fsync(ctx),
            x if x == Opcode::Setxattr as u32 => self.setxattr(ctx),
            x if x == Opcode::Getxattr as u32 => self.getxattr(ctx),
            x if x == Opcode::Listxattr as u32 => self.listxattr(ctx),
            x if x == Opcode::Removexattr as u32 => self.removexattr(ctx),
            x if x == Opcode::Flush as u32 => self.flush(ctx),
            x if x == Opcode::Init as u32 => self.init(ctx),
            x if x == Opcode::Opendir as u32 => self.opendir(ctx),
            x if x == Opcode::Readdir as u32 => self.readdir(ctx),
            x if x == Opcode::Releasedir as u32 => self.releasedir(ctx),
            x if x == Opcode::Fsyncdir as u32 => self.fsyncdir(ctx),
            x if x == Opcode::Getlk as u32 => self.getlk(ctx),
            x if x == Opcode::Setlk as u32 => self.setlk(ctx),
            x if x == Opcode::Setlkw as u32 => self.setlkw(ctx),
            x if x == Opcode::Access as u32 => self.access(ctx),
            x if x == Opcode::Create as u32 => self.create(ctx),
            x if x == Opcode::Bmap as u32 => self.bmap(ctx),
            x if x == Opcode::Ioctl as u32 => self.ioctl(ctx),
            x if x == Opcode::Poll as u32 => self.poll(ctx),
            x if x == Opcode::NotifyReply as u32 => self.notify_reply(ctx),
            x if x == Opcode::BatchForget as u32 => self.batch_forget(ctx),
            x if x == Opcode::Fallocate as u32 => self.fallocate(ctx),
            x if x == Opcode::Readdirplus as u32 => self.readdirplus(ctx),
            x if x == Opcode::Rename2 as u32 => self.rename2(ctx),
            x if x == Opcode::Lseek as u32 => self.lseek(ctx),
            #[cfg(feature = "virtiofs")]
            x if x == Opcode::SetupMapping as u32 => self.setupmapping(ctx, vu_req),
            #[cfg(feature = "virtiofs")]
            x if x == Opcode::RemoveMapping as u32 => self.removemapping(ctx, vu_req),
            // Group reqeusts don't need reply together
            x => match x {
                x if x == Opcode::Interrupt as u32 => {
                    self.interrupt(ctx);
                    Ok(0)
                }
                x if x == Opcode::Destroy as u32 => {
                    self.destroy();
                    Ok(0)
                }
                _ => ctx.reply_error(io::Error::from_raw_os_error(libc::ENOSYS)),
            },
        };

        // Pass `None` because current API handler's design does not allow us to catch
        // the `out_header`. Hopefully, we can reach to `out_header` after some
        // refactoring work someday.
        hook.map_or((), |h| h.release(None));

        res
    }

    fn lookup(&self, mut ctx: SrvContext<'_, F, D, S>) -> Result<usize> {
        let buf = ServerUtil::get_message_body(&mut ctx.r, &ctx.in_header, 0)?;
        let name = bytes_to_cstr(buf.as_ref())?;
        let version = self.vers.load();
        let result = self.fs.lookup(ctx.context(), ctx.nodeid(), name);

        match result {
            // before ABI 7.4 inode == 0 was invalid, only ENOENT means negative dentry
            Ok(entry)
                if version.minor < KERNEL_MINOR_VERSION_LOOKUP_NEGATIVE_ENTRY_ZERO
                    && entry.inode == 0 =>
            {
                ctx.reply_error(io::Error::from_raw_os_error(libc::ENOENT))
            }
            Ok(entry) => {
                let out = EntryOut::from(entry);

                ctx.reply_ok(Some(out), None)
            }
            Err(e) => ctx.reply_error(e),
        }
    }

    pub(super) fn forget(&self, mut ctx: SrvContext<'_, F, D, S>) -> Result<usize> {
        let ForgetIn { nlookup } = ctx.r.read_obj().map_err(Error::DecodeMessage)?;

        self.fs.forget(ctx.context(), ctx.nodeid(), nlookup);

        // There is no reply for forget messages.
        Ok(0)
    }

    fn getattr(&self, mut ctx: SrvContext<'_, F, D, S>) -> Result<usize> {
        let GetattrIn { flags, fh, .. } = ctx.r.read_obj().map_err(Error::DecodeMessage)?;
        let handle = if (flags & GETATTR_FH) != 0 {
            Some(fh.into())
        } else {
            None
        };
        let result = self.fs.getattr(ctx.context(), ctx.nodeid(), handle);

        ctx.handle_attr_result(result)
    }

    fn setattr(&self, mut ctx: SrvContext<'_, F, D, S>) -> Result<usize> {
        let setattr_in: SetattrIn = ctx.r.read_obj().map_err(Error::DecodeMessage)?;
        let handle = if setattr_in.valid & FATTR_FH != 0 {
            Some(setattr_in.fh.into())
        } else {
            None
        };
        let valid = SetattrValid::from_bits_truncate(setattr_in.valid);
        let st: libc::stat64 = setattr_in.into();
        let result = self
            .fs
            .setattr(ctx.context(), ctx.nodeid(), st, handle, valid);

        ctx.handle_attr_result(result)
    }

    pub(super) fn readlink(&self, mut ctx: SrvContext<'_, F, D, S>) -> Result<usize> {
        match self.fs.readlink(ctx.context(), ctx.nodeid()) {
            Ok(linkname) => {
                // We need to disambiguate the option type here even though it is `None`.
                ctx.reply_ok(None::<u8>, Some(&linkname))
            }
            Err(e) => ctx.reply_error(e),
        }
    }

    pub(super) fn symlink(&self, mut ctx: SrvContext<'_, F, D, S>) -> Result<usize> {
        let buf = ServerUtil::get_message_body(&mut ctx.r, &ctx.in_header, 0)?;
        // The name and linkname are encoded one after another and separated by a nul character.
        let (name, linkname) = ServerUtil::extract_two_cstrs(&buf)?;

        match self.fs.symlink(ctx.context(), linkname, ctx.nodeid(), name) {
            Ok(entry) => ctx.reply_ok(Some(EntryOut::from(entry)), None),
            Err(e) => ctx.reply_error(e),
        }
    }

    pub(super) fn mknod(&self, mut ctx: SrvContext<'_, F, D, S>) -> Result<usize> {
        let MknodIn {
            mode, rdev, umask, ..
        } = ctx.r.read_obj().map_err(Error::DecodeMessage)?;
        let buf = ServerUtil::get_message_body(&mut ctx.r, &ctx.in_header, size_of::<MknodIn>())?;
        let name = bytes_to_cstr(buf.as_ref())?;

        match self
            .fs
            .mknod(ctx.context(), ctx.nodeid(), name, mode, rdev, umask)
        {
            Ok(entry) => ctx.reply_ok(Some(EntryOut::from(entry)), None),
            Err(e) => ctx.reply_error(e),
        }
    }

    pub(super) fn mkdir(&self, mut ctx: SrvContext<'_, F, D, S>) -> Result<usize> {
        let MkdirIn { mode, umask } = ctx.r.read_obj().map_err(Error::DecodeMessage)?;
        let buf = ServerUtil::get_message_body(&mut ctx.r, &ctx.in_header, size_of::<MkdirIn>())?;
        let name = bytes_to_cstr(buf.as_ref())?;

        match self
            .fs
            .mkdir(ctx.context(), ctx.nodeid(), name, mode, umask)
        {
            Ok(entry) => ctx.reply_ok(Some(EntryOut::from(entry)), None),
            Err(e) => ctx.reply_error(e),
        }
    }

    pub(super) fn unlink(&self, mut ctx: SrvContext<'_, F, D, S>) -> Result<usize> {
        let buf = ServerUtil::get_message_body(&mut ctx.r, &ctx.in_header, 0)?;
        let name = bytes_to_cstr(buf.as_ref())?;

        match self.fs.unlink(ctx.context(), ctx.nodeid(), name) {
            Ok(()) => ctx.reply_ok(None::<u8>, None),
            Err(e) => ctx.reply_error(e),
        }
    }

    pub(super) fn rmdir(&self, mut ctx: SrvContext<'_, F, D, S>) -> Result<usize> {
        let buf = ServerUtil::get_message_body(&mut ctx.r, &ctx.in_header, 0)?;
        let name = bytes_to_cstr(buf.as_ref())?;

        match self.fs.rmdir(ctx.context(), ctx.nodeid(), name) {
            Ok(()) => ctx.reply_ok(None::<u8>, None),
            Err(e) => ctx.reply_error(e),
        }
    }

    pub(super) fn do_rename(
        &self,
        mut ctx: SrvContext<'_, F, D, S>,
        msg_size: usize,
        newdir: u64,
        flags: u32,
    ) -> Result<usize> {
        let buf = ServerUtil::get_message_body(&mut ctx.r, &ctx.in_header, msg_size)?;
        let (oldname, newname) = ServerUtil::extract_two_cstrs(&buf)?;

        match self.fs.rename(
            ctx.context(),
            ctx.nodeid(),
            oldname,
            newdir.into(),
            newname,
            flags,
        ) {
            Ok(()) => ctx.reply_ok(None::<u8>, None),
            Err(e) => ctx.reply_error(e),
        }
    }

    pub(super) fn rename(&self, mut ctx: SrvContext<'_, F, D, S>) -> Result<usize> {
        let RenameIn { newdir } = ctx.r.read_obj().map_err(Error::DecodeMessage)?;

        self.do_rename(ctx, size_of::<RenameIn>(), newdir, 0)
    }

    pub(super) fn rename2(&self, mut ctx: SrvContext<'_, F, D, S>) -> Result<usize> {
        let Rename2In { newdir, flags, .. } = ctx.r.read_obj().map_err(Error::DecodeMessage)?;

        let flags =
            flags & (libc::RENAME_EXCHANGE | libc::RENAME_NOREPLACE | libc::RENAME_WHITEOUT) as u32;

        self.do_rename(ctx, size_of::<Rename2In>(), newdir, flags)
    }

    pub(super) fn link(&self, mut ctx: SrvContext<'_, F, D, S>) -> Result<usize> {
        let LinkIn { oldnodeid } = ctx.r.read_obj().map_err(Error::DecodeMessage)?;
        let buf = ServerUtil::get_message_body(&mut ctx.r, &ctx.in_header, size_of::<LinkIn>())?;
        let name = bytes_to_cstr(buf.as_ref())?;

        match self
            .fs
            .link(ctx.context(), oldnodeid.into(), ctx.nodeid(), name)
        {
            Ok(entry) => ctx.reply_ok(Some(EntryOut::from(entry)), None),
            Err(e) => ctx.reply_error(e),
        }
    }

    fn open(&self, mut ctx: SrvContext<'_, F, D, S>) -> Result<usize> {
        let OpenIn { flags, fuse_flags } = ctx.r.read_obj().map_err(Error::DecodeMessage)?;

        match self.fs.open(ctx.context(), ctx.nodeid(), flags, fuse_flags) {
            Ok((handle, opts)) => {
                let out = OpenOut {
                    fh: handle.map(Into::into).unwrap_or(0),
                    open_flags: opts.bits(),
                    ..Default::default()
                };

                ctx.reply_ok(Some(out), None)
            }
            Err(e) => ctx.reply_error(e),
        }
    }

    fn read(&self, mut ctx: SrvContext<'_, F, D, S>) -> Result<usize> {
        let ReadIn {
            fh,
            offset,
            size,
            read_flags,
            lock_owner,
            flags,
            ..
        } = ctx.r.read_obj().map_err(Error::DecodeMessage)?;

        if size > MAX_BUFFER_SIZE {
            return ctx.reply_error_explicit(io::Error::from_raw_os_error(libc::ENOMEM));
        }

        let owner = if read_flags & READ_LOCKOWNER != 0 {
            Some(lock_owner)
        } else {
            None
        };

        // Split the writer into 2 pieces: one for the `OutHeader` and the rest for the data.
        let w2 = match ctx.w.split_at(size_of::<OutHeader>()) {
            Ok(v) => v,
            Err(_e) => return Err(Error::InvalidHeaderLength),
        };
        let mut data_writer = ZcWriter(w2);

        match self.fs.read(
            ctx.context(),
            ctx.nodeid(),
            fh.into(),
            &mut data_writer,
            size,
            offset,
            owner,
            flags,
        ) {
            Ok(count) => {
                // Don't use `reply_ok` because we need to set a custom size length for the
                // header.
                let out = OutHeader {
                    len: (size_of::<OutHeader>() + count) as u32,
                    error: 0,
                    unique: ctx.unique(),
                };

                ctx.w
                    .write_all(out.as_slice())
                    .map_err(Error::EncodeMessage)?;
                ctx.w
                    .commit(Some(&data_writer.0))
                    .map_err(Error::EncodeMessage)?;
                Ok(out.len as usize)
            }
            Err(e) => ctx.reply_error_explicit(e),
        }
    }

    fn write(&self, mut ctx: SrvContext<'_, F, D, S>) -> Result<usize> {
        let WriteIn {
            fh,
            offset,
            size,
            fuse_flags,
            lock_owner,
            flags,
            ..
        } = ctx.r.read_obj().map_err(Error::DecodeMessage)?;

        if size > MAX_BUFFER_SIZE {
            return ctx.reply_error_explicit(io::Error::from_raw_os_error(libc::ENOMEM));
        }

        let owner = if fuse_flags & WRITE_LOCKOWNER != 0 {
            Some(lock_owner)
        } else {
            None
        };

        let delayed_write = fuse_flags & WRITE_CACHE != 0;

        let mut data_reader = ZcReader(ctx.take_reader());

        match self.fs.write(
            ctx.context(),
            ctx.nodeid(),
            fh.into(),
            &mut data_reader,
            size,
            offset,
            owner,
            delayed_write,
            flags,
            fuse_flags,
        ) {
            Ok(count) => {
                let out = WriteOut {
                    size: count as u32,
                    ..Default::default()
                };

                ctx.reply_ok(Some(out), None)
            }
            Err(e) => ctx.reply_error_explicit(e),
        }
    }

    pub(super) fn statfs(&self, mut ctx: SrvContext<'_, F, D, S>) -> Result<usize> {
        match self.fs.statfs(ctx.context(), ctx.nodeid()) {
            Ok(st) => ctx.reply_ok(Some(Kstatfs::from(st)), None),
            Err(e) => ctx.reply_error(e),
        }
    }

    pub(super) fn release(&self, mut ctx: SrvContext<'_, F, D, S>) -> Result<usize> {
        let ReleaseIn {
            fh,
            flags,
            release_flags,
            lock_owner,
        } = ctx.r.read_obj().map_err(Error::DecodeMessage)?;

        let flush = release_flags & RELEASE_FLUSH != 0;
        let flock_release = release_flags & RELEASE_FLOCK_UNLOCK != 0;
        let lock_owner = if flush || flock_release {
            Some(lock_owner)
        } else {
            None
        };

        match self.fs.release(
            ctx.context(),
            ctx.nodeid(),
            flags,
            fh.into(),
            flush,
            flock_release,
            lock_owner,
        ) {
            Ok(()) => ctx.reply_ok(None::<u8>, None),
            Err(e) => ctx.reply_error(e),
        }
    }

    fn fsync(&self, mut ctx: SrvContext<'_, F, D, S>) -> Result<usize> {
        let FsyncIn {
            fh, fsync_flags, ..
        } = ctx.r.read_obj().map_err(Error::DecodeMessage)?;
        let datasync = fsync_flags & 0x1 != 0;

        match self
            .fs
            .fsync(ctx.context(), ctx.nodeid(), datasync, fh.into())
        {
            Ok(()) => ctx.reply_ok(None::<u8>, None),
            Err(e) => ctx.reply_error(e),
        }
    }

    pub(super) fn setxattr(&self, mut ctx: SrvContext<'_, F, D, S>) -> Result<usize> {
        let SetxattrIn { size, flags } = ctx.r.read_obj().map_err(Error::DecodeMessage)?;
        let buf =
            ServerUtil::get_message_body(&mut ctx.r, &ctx.in_header, size_of::<SetxattrIn>())?;

        // The name and value and encoded one after another and separated by a '\0' character.
        let split_pos = buf
            .iter()
            .position(|c| *c == b'\0')
            .map(|p| p + 1)
            .ok_or(Error::MissingParameter)?;
        let (name, value) = buf.split_at(split_pos);

        if size != value.len() as u32 {
            return Err(Error::InvalidXattrSize((size, value.len())));
        }

        match self.fs.setxattr(
            ctx.context(),
            ctx.nodeid(),
            bytes_to_cstr(name)?,
            value,
            flags,
        ) {
            Ok(()) => ctx.reply_ok(None::<u8>, None),
            Err(e) => ctx.reply_error(e),
        }
    }

    pub(super) fn getxattr(&self, mut ctx: SrvContext<'_, F, D, S>) -> Result<usize> {
        let GetxattrIn { size, .. } = ctx.r.read_obj().map_err(Error::DecodeMessage)?;
        if size > MAX_BUFFER_SIZE {
            return ctx.reply_error_explicit(io::Error::from_raw_os_error(libc::ENOMEM));
        }

        let buf =
            ServerUtil::get_message_body(&mut ctx.r, &ctx.in_header, size_of::<GetxattrIn>())?;
        let name = bytes_to_cstr(buf.as_ref())?;

        match self.fs.getxattr(ctx.context(), ctx.nodeid(), name, size) {
            Ok(GetxattrReply::Value(val)) => ctx.reply_ok(None::<u8>, Some(&val)),
            Ok(GetxattrReply::Count(count)) => {
                let out = GetxattrOut {
                    size: count,
                    ..Default::default()
                };

                ctx.reply_ok(Some(out), None)
            }
            Err(e) => ctx.reply_error(e),
        }
    }

    pub(super) fn listxattr(&self, mut ctx: SrvContext<'_, F, D, S>) -> Result<usize> {
        let GetxattrIn { size, .. } = ctx.r.read_obj().map_err(Error::DecodeMessage)?;

        if size > MAX_BUFFER_SIZE {
            return ctx.reply_error_explicit(io::Error::from_raw_os_error(libc::ENOMEM));
        }

        match self.fs.listxattr(ctx.context(), ctx.nodeid(), size) {
            Ok(ListxattrReply::Names(val)) => ctx.reply_ok(None::<u8>, Some(&val)),
            Ok(ListxattrReply::Count(count)) => {
                let out = GetxattrOut {
                    size: count,
                    ..Default::default()
                };

                ctx.reply_ok(Some(out), None)
            }
            Err(e) => ctx.reply_error(e),
        }
    }

    pub(super) fn removexattr(&self, mut ctx: SrvContext<'_, F, D, S>) -> Result<usize> {
        let buf = ServerUtil::get_message_body(&mut ctx.r, &ctx.in_header, 0)?;
        let name = bytes_to_cstr(&buf)?;

        match self.fs.removexattr(ctx.context(), ctx.nodeid(), name) {
            Ok(()) => ctx.reply_ok(None::<u8>, None),
            Err(e) => ctx.reply_error(e),
        }
    }

    pub(super) fn flush(&self, mut ctx: SrvContext<'_, F, D, S>) -> Result<usize> {
        let FlushIn { fh, lock_owner, .. } = ctx.r.read_obj().map_err(Error::DecodeMessage)?;

        match self
            .fs
            .flush(ctx.context(), ctx.nodeid(), fh.into(), lock_owner)
        {
            Ok(()) => ctx.reply_ok(None::<u8>, None),
            Err(e) => ctx.reply_error(e),
        }
    }

    pub(super) fn init(&self, mut ctx: SrvContext<'_, F, D, S>) -> Result<usize> {
        let InitIn {
            major,
            minor,
            max_readahead,
            flags,
        } = ctx.r.read_obj().map_err(Error::DecodeMessage)?;

        if major < KERNEL_VERSION {
            error!("Unsupported fuse protocol version: {}.{}", major, minor);
            return ctx.reply_error_explicit(io::Error::from_raw_os_error(libc::EPROTO));
        }

        if major > KERNEL_VERSION {
            // Wait for the kernel to reply back with a 7.X version.
            let out = InitOut {
                major: KERNEL_VERSION,
                minor: KERNEL_MINOR_VERSION,
                ..Default::default()
            };

            return ctx.reply_ok(Some(out), None);
        }

        // These fuse features are supported by this server by default.
        let supported = FsOptions::ASYNC_READ
            | FsOptions::PARALLEL_DIROPS
            | FsOptions::BIG_WRITES
            | FsOptions::AUTO_INVAL_DATA
            | FsOptions::ASYNC_DIO
            | FsOptions::HAS_IOCTL_DIR
            | FsOptions::MAX_PAGES
            | FsOptions::EXPLICIT_INVAL_DATA;

        let capable = FsOptions::from_bits_truncate(flags);

        match self.fs.init(capable) {
            Ok(want) => {
                let enabled = capable & (want | supported);
                info!(
                    "FUSE INIT major {} minor {}\n in_opts: {:?}\nout_opts: {:?}",
                    major, minor, capable, enabled
                );

                let out = InitOut {
                    major: KERNEL_VERSION,
                    minor: KERNEL_MINOR_VERSION,
                    max_readahead,
                    flags: enabled.bits(),
                    max_background: ::std::u16::MAX,
                    congestion_threshold: (::std::u16::MAX / 4) * 3,
                    max_write: MAX_BUFFER_SIZE,
                    time_gran: 1,             // nanoseconds
                    max_pages: MAX_REQ_PAGES, // 1MB
                    ..Default::default()
                };
                let vers = ServerVersion { major, minor };
                self.vers.store(Arc::new(vers));
                if minor < KERNEL_MINOR_VERSION_INIT_OUT_SIZE {
                    ctx.reply_ok(
                        Some(
                            *<[u8; FUSE_COMPAT_INIT_OUT_SIZE]>::from_slice(
                                out.as_slice().split_at(FUSE_COMPAT_INIT_OUT_SIZE).0,
                            )
                            .unwrap(),
                        ),
                        None,
                    )
                } else if minor < KERNEL_MINOR_VERSION_INIT_22_OUT_SIZE {
                    ctx.reply_ok(
                        Some(
                            *<[u8; FUSE_COMPAT_22_INIT_OUT_SIZE]>::from_slice(
                                out.as_slice().split_at(FUSE_COMPAT_22_INIT_OUT_SIZE).0,
                            )
                            .unwrap(),
                        ),
                        None,
                    )
                } else {
                    ctx.reply_ok(Some(out), None)
                }
            }
            Err(e) => ctx.reply_error(e),
        }
    }

    pub(super) fn opendir(&self, mut ctx: SrvContext<'_, F, D, S>) -> Result<usize> {
        let OpenIn { flags, .. } = ctx.r.read_obj().map_err(Error::DecodeMessage)?;

        match self.fs.opendir(ctx.context(), ctx.nodeid(), flags) {
            Ok((handle, opts)) => {
                let out = OpenOut {
                    fh: handle.map(Into::into).unwrap_or(0),
                    open_flags: opts.bits(),
                    ..Default::default()
                };

                ctx.reply_ok(Some(out), None)
            }
            Err(e) => ctx.reply_error(e),
        }
    }

    fn do_readdir(&self, mut ctx: SrvContext<'_, F, D, S>, plus: bool) -> Result<usize> {
        let ReadIn {
            fh, offset, size, ..
        } = ctx.r.read_obj().map_err(Error::DecodeMessage)?;

        if size > MAX_BUFFER_SIZE {
            return ctx.reply_error_explicit(io::Error::from_raw_os_error(libc::ENOMEM));
        }

        let available_bytes = ctx.w.available_bytes();
        if available_bytes < size as usize {
            return ctx.reply_error_explicit(io::Error::from_raw_os_error(libc::ENOMEM));
        }

        // Skip over enough bytes for the header.
        let mut cursor = match ctx.w.split_at(size_of::<OutHeader>()) {
            Ok(v) => v,
            Err(_e) => return Err(Error::InvalidHeaderLength),
        };

        let res = if plus {
            self.fs.readdirplus(
                ctx.context(),
                ctx.nodeid(),
                fh.into(),
                size,
                offset,
                &mut |d, e| add_dirent(&mut cursor, size, d, Some(e)),
            )
        } else {
            self.fs.readdir(
                ctx.context(),
                ctx.nodeid(),
                fh.into(),
                size,
                offset,
                &mut |d| add_dirent(&mut cursor, size, d, None),
            )
        };

        if let Err(e) = res {
            ctx.reply_error_explicit(e)
        } else {
            // Don't use `reply_ok` because we need to set a custom size length for the
            // header.
            let out = OutHeader {
                len: (size_of::<OutHeader>() + cursor.bytes_written()) as u32,
                error: 0,
                unique: ctx.unique(),
            };

            ctx.w
                .write_all(out.as_slice())
                .map_err(Error::EncodeMessage)?;
            ctx.w.commit(Some(&cursor)).map_err(Error::EncodeMessage)?;
            Ok(out.len as usize)
        }
    }

    pub(super) fn readdir(&self, ctx: SrvContext<'_, F, D, S>) -> Result<usize> {
        self.do_readdir(ctx, false)
    }

    pub(super) fn readdirplus(&self, ctx: SrvContext<'_, F, D, S>) -> Result<usize> {
        self.do_readdir(ctx, true)
    }

    pub(super) fn releasedir(&self, mut ctx: SrvContext<'_, F, D, S>) -> Result<usize> {
        let ReleaseIn { fh, flags, .. } = ctx.r.read_obj().map_err(Error::DecodeMessage)?;

        match self
            .fs
            .releasedir(ctx.context(), ctx.nodeid(), flags, fh.into())
        {
            Ok(()) => ctx.reply_ok(None::<u8>, None),
            Err(e) => ctx.reply_error(e),
        }
    }

    fn fsyncdir(&self, mut ctx: SrvContext<'_, F, D, S>) -> Result<usize> {
        let FsyncIn {
            fh, fsync_flags, ..
        } = ctx.r.read_obj().map_err(Error::DecodeMessage)?;
        let datasync = fsync_flags & 0x1 != 0;

        match self
            .fs
            .fsyncdir(ctx.context(), ctx.nodeid(), datasync, fh.into())
        {
            Ok(()) => ctx.reply_ok(None::<u8>, None),
            Err(e) => ctx.reply_error(e),
        }
    }

    pub(super) fn getlk(&self, mut ctx: SrvContext<'_, F, D, S>) -> Result<usize> {
        if let Err(e) = self.fs.getlk() {
            ctx.reply_error(e)
        } else {
            Ok(0)
        }
    }

    pub(super) fn setlk(&self, mut ctx: SrvContext<'_, F, D, S>) -> Result<usize> {
        if let Err(e) = self.fs.setlk() {
            ctx.reply_error(e)
        } else {
            Ok(0)
        }
    }

    pub(super) fn setlkw(&self, mut ctx: SrvContext<'_, F, D, S>) -> Result<usize> {
        if let Err(e) = self.fs.setlkw() {
            ctx.reply_error(e)
        } else {
            Ok(0)
        }
    }

    pub(super) fn access(&self, mut ctx: SrvContext<'_, F, D, S>) -> Result<usize> {
        let AccessIn { mask, .. } = ctx.r.read_obj().map_err(Error::DecodeMessage)?;

        match self.fs.access(ctx.context(), ctx.nodeid(), mask) {
            Ok(()) => ctx.reply_ok(None::<u8>, None),
            Err(e) => ctx.reply_error(e),
        }
    }

    fn create(&self, mut ctx: SrvContext<'_, F, D, S>) -> Result<usize> {
        let args: CreateIn = ctx.r.read_obj().map_err(Error::DecodeMessage)?;
        let buf = ServerUtil::get_message_body(&mut ctx.r, &ctx.in_header, size_of::<CreateIn>())?;
        let name = bytes_to_cstr(&buf)?;

        match self.fs.create(ctx.context(), ctx.nodeid(), name, args) {
            Ok((entry, handle, opts)) => {
                let entry_out = EntryOut {
                    nodeid: entry.inode,
                    generation: entry.generation,
                    entry_valid: entry.entry_timeout.as_secs(),
                    attr_valid: entry.attr_timeout.as_secs(),
                    entry_valid_nsec: entry.entry_timeout.subsec_nanos(),
                    attr_valid_nsec: entry.attr_timeout.subsec_nanos(),
                    attr: entry.attr.into(),
                };
                let open_out = OpenOut {
                    fh: handle.map(Into::into).unwrap_or(0),
                    open_flags: opts.bits(),
                    ..Default::default()
                };

                // Kind of a hack to write both structs.
                ctx.reply_ok(Some(entry_out), Some(open_out.as_slice()))
            }
            Err(e) => ctx.reply_error(e),
        }
    }

    pub(super) fn interrupt(&self, _ctx: SrvContext<'_, F, D, S>) {}

    pub(super) fn bmap(&self, mut ctx: SrvContext<'_, F, D, S>) -> Result<usize> {
        if let Err(e) = self.fs.bmap() {
            ctx.reply_error(e)
        } else {
            Ok(0)
        }
    }

    pub(super) fn destroy(&self) {
        // No reply to this function.
        self.fs.destroy();
    }

    pub(super) fn ioctl(&self, mut ctx: SrvContext<'_, F, D, S>) -> Result<usize> {
        if let Err(e) = self.fs.ioctl() {
            ctx.reply_error(e)
        } else {
            Ok(0)
        }
    }

    pub(super) fn poll(&self, mut ctx: SrvContext<'_, F, D, S>) -> Result<usize> {
        if let Err(e) = self.fs.poll() {
            ctx.reply_error(e)
        } else {
            Ok(0)
        }
    }

    pub(super) fn notify_reply(&self, mut ctx: SrvContext<'_, F, D, S>) -> Result<usize> {
        if let Err(e) = self.fs.notify_reply() {
            ctx.reply_error(e)
        } else {
            Ok(0)
        }
    }

    pub(super) fn batch_forget(&self, mut ctx: SrvContext<'_, F, D, S>) -> Result<usize> {
        let BatchForgetIn { count, .. } = ctx.r.read_obj().map_err(Error::DecodeMessage)?;

        if let Some(size) = (count as usize).checked_mul(size_of::<ForgetOne>()) {
            if size > MAX_BUFFER_SIZE as usize {
                return ctx.reply_error_explicit(io::Error::from_raw_os_error(libc::ENOMEM));
            }
        } else {
            return ctx.reply_error_explicit(io::Error::from_raw_os_error(libc::EOVERFLOW));
        }

        let mut requests = Vec::with_capacity(count as usize);
        for _ in 0..count {
            requests.push(
                ctx.r
                    .read_obj::<ForgetOne>()
                    .map(|f| (f.nodeid.into(), f.nlookup))
                    .map_err(Error::DecodeMessage)?,
            );
        }

        self.fs.batch_forget(ctx.context(), requests);

        // No reply for forget messages.
        Ok(0)
    }

    fn fallocate(&self, mut ctx: SrvContext<'_, F, D, S>) -> Result<usize> {
        let FallocateIn {
            fh,
            offset,
            length,
            mode,
            ..
        } = ctx.r.read_obj().map_err(Error::DecodeMessage)?;

        match self
            .fs
            .fallocate(ctx.context(), ctx.nodeid(), fh.into(), mode, offset, length)
        {
            Ok(()) => ctx.reply_ok(None::<u8>, None),
            Err(e) => ctx.reply_error(e),
        }
    }

    pub(super) fn lseek(&self, mut ctx: SrvContext<'_, F, D, S>) -> Result<usize> {
        let LseekIn {
            fh, offset, whence, ..
        } = ctx.r.read_obj().map_err(Error::DecodeMessage)?;

        match self
            .fs
            .lseek(ctx.context(), ctx.nodeid(), fh.into(), offset, whence)
        {
            Ok(offset) => {
                let out = LseekOut { offset };

                ctx.reply_ok(Some(out), None)
            }
            Err(e) => ctx.reply_error(e),
        }
    }
}

#[cfg(feature = "virtiofs")]
impl<F: FileSystem<S> + Sync, D: AsyncDrive, S: BitmapSlice> Server<F, D, S> {
    pub(super) fn setupmapping(
        &self,
        mut ctx: SrvContext<'_, F, D, S>,
        vu_req: Option<&mut dyn FsCacheReqHandler>,
    ) -> Result<usize> {
        if let Some(req) = vu_req {
            let SetupmappingIn {
                fh,
                foffset,
                len,
                flags,
                moffset,
            } = ctx.r.read_obj().map_err(Error::DecodeMessage)?;

            match self.fs.setupmapping(
                ctx.context(),
                ctx.nodeid(),
                fh.into(),
                foffset,
                len,
                flags,
                moffset,
                req,
            ) {
                Ok(()) => ctx.reply_ok(None::<u8>, None),
                Err(e) => ctx.reply_error(e),
            }
        } else {
            ctx.reply_error_explicit(io::Error::from_raw_os_error(libc::EINVAL))
        }
    }

    pub(super) fn removemapping(
        &self,
        mut ctx: SrvContext<'_, F, D, S>,
        vu_req: Option<&mut dyn FsCacheReqHandler>,
    ) -> Result<usize> {
        if let Some(req) = vu_req {
            let RemovemappingIn { count } = ctx.r.read_obj().map_err(Error::DecodeMessage)?;

            if let Some(size) = (count as usize).checked_mul(size_of::<RemovemappingOne>()) {
                if size > MAX_BUFFER_SIZE as usize {
                    return ctx.reply_error_explicit(io::Error::from_raw_os_error(libc::ENOMEM));
                }
            } else {
                return ctx.reply_error_explicit(io::Error::from_raw_os_error(libc::EOVERFLOW));
            }

            let mut requests = Vec::with_capacity(count as usize);
            for _ in 0..count {
                requests.push(
                    ctx.r
                        .read_obj::<RemovemappingOne>()
                        .map_err(Error::DecodeMessage)?,
                );
            }

            match self
                .fs
                .removemapping(ctx.context(), ctx.nodeid(), requests, req)
            {
                Ok(()) => ctx.reply_ok(None::<u8>, None),
                Err(e) => ctx.reply_error(e),
            }
        } else {
            ctx.reply_error_explicit(io::Error::from_raw_os_error(libc::EINVAL))
        }
    }
}

impl<'a, F: FileSystem<S>, D: AsyncDrive, S: BitmapSlice> SrvContext<'a, F, D, S> {
    fn reply_ok<T: ByteValued>(&mut self, out: Option<T>, data: Option<&[u8]>) -> Result<usize> {
        let data2 = out.as_ref().map(|v| v.as_slice()).unwrap_or(&[]);
        let data3 = data.unwrap_or(&[]);
        let len = size_of::<OutHeader>() + data2.len() + data3.len();
        let header = OutHeader {
            len: len as u32,
            error: 0,
            unique: self.unique(),
        };
        trace!("fuse: new reply {:?}", header);

        match (data2.len(), data3.len()) {
            (0, 0) => self
                .w
                .write(header.as_slice())
                .map_err(Error::EncodeMessage)?,
            (0, _) => self
                .w
                .write_vectored(&[IoSlice::new(header.as_slice()), IoSlice::new(data3)])
                .map_err(Error::EncodeMessage)?,
            (_, 0) => self
                .w
                .write_vectored(&[IoSlice::new(header.as_slice()), IoSlice::new(data2)])
                .map_err(Error::EncodeMessage)?,
            (_, _) => self
                .w
                .write_vectored(&[
                    IoSlice::new(header.as_slice()),
                    IoSlice::new(data2),
                    IoSlice::new(data3),
                ])
                .map_err(Error::EncodeMessage)?,
        };

        debug_assert_eq!(len, self.w.bytes_written());
        Ok(self.w.bytes_written())
    }

    fn do_reply_error(&mut self, err: io::Error, explicit: bool) -> Result<usize> {
        let header = OutHeader {
            len: size_of::<OutHeader>() as u32,
            error: -err
                .raw_os_error()
                .unwrap_or_else(|| encode_io_error_kind(err.kind())),
            unique: self.unique(),
        };

        if explicit || err.raw_os_error().is_none() {
            error!("fuse: reply error header {:?}, error {:?}", header, err);
        } else {
            trace!("fuse: reply error header {:?}, error {:?}", header, err);
        }
        self.w
            .write_all(header.as_slice())
            .map_err(Error::EncodeMessage)?;

        // Commit header if it is buffered otherwise kernel gets nothing back.
        self.w
            .commit(None)
            .map(|_| {
                debug_assert_eq!(header.len as usize, self.w.bytes_written());
                self.w.bytes_written()
            })
            .map_err(Error::EncodeMessage)
    }

    // reply operation error back to fuse client, don't print error message, as they are not server's
    // internal error, and client could deal with them.
    fn reply_error(&mut self, err: io::Error) -> Result<usize> {
        self.do_reply_error(err, false)
    }

    fn reply_error_explicit(&mut self, err: io::Error) -> Result<usize> {
        self.do_reply_error(err, true)
    }

    fn handle_attr_result(
        &mut self,
        result: io::Result<(libc::stat64, Duration)>,
    ) -> Result<usize> {
        match result {
            Ok((st, timeout)) => {
                let out = AttrOut {
                    attr_valid: timeout.as_secs(),
                    attr_valid_nsec: timeout.subsec_nanos(),
                    dummy: 0,
                    attr: st.into(),
                };
                self.reply_ok(Some(out), None)
            }
            Err(e) => self.reply_error(e),
        }
    }
}

fn add_dirent<S: BitmapSlice>(
    cursor: &mut Writer<'_, S>,
    max: u32,
    d: DirEntry,
    entry: Option<Entry>,
) -> io::Result<usize> {
    if d.name.len() > ::std::u32::MAX as usize {
        return Err(io::Error::from_raw_os_error(libc::EOVERFLOW));
    }

    let dirent_len = size_of::<Dirent>()
        .checked_add(d.name.len())
        .ok_or_else(|| io::Error::from_raw_os_error(libc::EOVERFLOW))?;

    // Directory entries must be padded to 8-byte alignment.  If adding 7 causes
    // an overflow then this dirent cannot be properly padded.
    let padded_dirent_len = dirent_len
        .checked_add(7)
        .map(|l| l & !7)
        .ok_or_else(|| io::Error::from_raw_os_error(libc::EOVERFLOW))?;

    let total_len = if entry.is_some() {
        padded_dirent_len
            .checked_add(size_of::<EntryOut>())
            .ok_or_else(|| io::Error::from_raw_os_error(libc::EOVERFLOW))?
    } else {
        padded_dirent_len
    };

    // Skip the entry if there's no enough space left.
    if (max as usize).saturating_sub(cursor.bytes_written()) < total_len {
        Ok(0)
    } else {
        if let Some(entry) = entry {
            cursor.write_all(EntryOut::from(entry).as_slice())?;
        }

        let dirent = Dirent {
            ino: d.ino,
            off: d.offset,
            namelen: d.name.len() as u32,
            type_: d.type_,
        };

        cursor.write_all(dirent.as_slice())?;
        cursor.write_all(d.name)?;

        // We know that `dirent_len` <= `padded_dirent_len` due to the check above
        // so there's no need for checked arithmetic.
        let padding = padded_dirent_len - dirent_len;
        if padding > 0 {
            cursor.write_all(&DIRENT_PADDING[..padding])?;
        }

        Ok(total_len)
    }
}
