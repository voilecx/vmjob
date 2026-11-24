//! The disks a job's virtual machine sees: a tar of its input going in, a tar
//! of its output coming back.
//!
//! The output tar is written by the guest, which runs code the runner does not
//! trust, so unpacking it accepts only regular files and directories at
//! relative paths below the destination, resolves every name beneath it,
//! never makes a link or writes over a file, and stops at the bounds in
//! [`Limits`]; a refused output leaves nothing behind.

use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};

use anyhow::{Context as _, bail};
use cap_std::ambient_authority;
use cap_std::fs::Dir;
use vmjob_api::{GUEST_EXIT, GUEST_JOB, GUEST_OVERFLOW as OVERFLOW, GuestJob};

/// Write the input disk: `job` as [`GUEST_JOB`], then the tree under `input`
/// (links stay links; nothing is followed).
pub fn write_input(dest: &Path, input: Option<&Path>, job: &GuestJob) -> anyhow::Result<()> {
    let file = File::create(dest).with_context(|| format!("creating {}", dest.display()))?;
    let mut tar = tar::Builder::new(io::BufWriter::new(file));
    tar.follow_symlinks(false);
    tar.mode(tar::HeaderMode::Deterministic);
    let json = serde_json::to_vec(job)?;
    let mut header = tar::Header::new_ustar();
    header.set_size(json.len() as u64);
    header.set_mode(0o644);
    header.set_entry_type(tar::EntryType::Regular);
    tar.append_data(&mut header, GUEST_JOB, json.as_slice())?;
    if let Some(input) = input {
        let meta = fs::symlink_metadata(input).with_context(|| format!("reading {}", input.display()))?;
        if !meta.is_dir() {
            bail!("the input {} is not a directory", input.display());
        }
        for entry in fs::read_dir(input)? {
            let entry = entry?;
            let name = entry.file_name();
            if name == ".vmjob" {
                bail!("the input may not hold a .vmjob entry");
            }
            let path = entry.path();
            if entry.file_type()?.is_dir() {
                tar.append_dir_all(&name, &path)?;
            } else {
                tar.append_path_with_name(&path, &name)?;
            }
        }
    }
    let mut writer = tar.into_inner()?;
    writer.flush()?;
    let file = writer.into_inner().map_err(io::IntoInnerError::into_error)?;
    file.sync_all()?;
    Ok(())
}

/// Create the output disk: `bytes` of zeroes, sparse where the filesystem allows.
pub fn create_output(dest: &Path, bytes: u64) -> anyhow::Result<()> {
    let file = File::create(dest).with_context(|| format!("creating {}", dest.display()))?;
    file.set_len(bytes)?;
    Ok(())
}

/// Bounds on what an output tar may unpack to.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// The most entries.
    pub entries: usize,
    /// The most bytes of file content.
    pub bytes: u64,
    /// The deepest path.
    pub depth: usize,
}

/// What came back from the guest.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Unpacked {
    /// The command's exit status, from [`GUEST_EXIT`].
    pub exit_code: Option<i32>,
    /// Regular files written.
    pub files: usize,
    /// Their bytes.
    pub bytes: u64,
    /// The guest's output did not fit the output disk, so it wrote the exit
    /// status alone.
    pub overflow: bool,
}

/// Why an output tar was refused.
#[derive(Debug)]
pub struct Refused(pub String);

impl std::error::Error for Refused {}

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// The staging directory, below the destination, that an output is unpacked
/// into before it is moved into place.
const STAGING: &str = ".vmjob-partial";

/// The largest GNU long-name or PAX extension body: the tar reader holds one
/// whole in memory.
const MAX_EXTENSION: u64 = 64 * 1024;

/// Read the output tar from `disk`. With a destination, its files and
/// directories are unpacked into a staging directory below it and moved into
/// it only once the whole tar was accepted, so a refused output leaves
/// nothing. Every name is resolved beneath the destination ([`cap_std`]): no
/// link, however it got there, leads outside it. [`GUEST_EXIT`] is read, not
/// written.
pub fn read_output<R: Read + Seek>(mut disk: R, dest: Option<&Path>, limits: Limits) -> Result<Unpacked, Refused> {
    prescan(&mut disk, limits)?;
    disk.seek(SeekFrom::Start(0)).map_err(|e| Refused(format!("rereading the output disk: {e}")))?;
    let Some(dest) = dest else { return unpack(disk, None, limits) };
    let root = Dir::open_ambient_dir(dest, ambient_authority())
        .map_err(|e| Refused(format!("opening {}: {e}", dest.display())))?;
    let _ = root.remove_dir_all(STAGING);
    root.create_dir(STAGING).map_err(|e| Refused(format!("making {STAGING}: {e}")))?;
    let unpacked = root.open_dir(STAGING).map_err(|e| Refused(format!("opening {STAGING}: {e}"))).and_then(|stage| {
        unpack(disk, Some(&stage), limits).and_then(|unpacked| move_into_place(&stage, &root).map(|()| unpacked))
    });
    // The staging handle is closed by now: some systems keep an open directory.
    let _ = root.remove_dir_all(STAGING);
    unpacked
}

/// Walk the raw headers once before unpacking. An extension body larger than
/// [`MAX_EXTENSION`], or more headers than twice the entry limit, refuses the
/// output before the tar reader buffers anything.
fn prescan(disk: &mut (impl Read + Seek), limits: Limits) -> Result<(), Refused> {
    let mut block = [0u8; 512];
    let mut headers = 0usize;
    loop {
        match disk.read_exact(&mut block) {
            Ok(()) => {}
            // A disk that ends without the closing zero blocks ends the tar too.
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(()),
            Err(e) => return Err(Refused(format!("reading the output disk: {e}"))),
        }
        if block.iter().all(|&b| b == 0) {
            return Ok(());
        }
        headers += 1;
        if headers > limits.entries.saturating_mul(2) {
            return Err(Refused(format!("more than {} entries", limits.entries)));
        }
        let header = tar::Header::from_byte_slice(&block);
        let size = header.entry_size().map_err(|e| Refused(format!("a bad size: {e}")))?;
        if matches!(block[156], b'L' | b'K' | b'x' | b'g') && size > MAX_EXTENSION {
            return Err(Refused(format!("a {size}-byte extension header")));
        }
        let skip = i64::try_from(size.div_ceil(512).saturating_mul(512))
            .map_err(|_| Refused("an entry larger than any disk".into()))?;
        disk.seek(SeekFrom::Current(skip)).map_err(|e| Refused(format!("reading the output disk: {e}")))?;
    }
}

fn unpack(disk: impl Read, stage: Option<&Dir>, limits: Limits) -> Result<Unpacked, Refused> {
    let refuse = |what: String| Refused(what);
    let mut archive = tar::Archive::new(disk);
    let mut unpacked = Unpacked::default();
    let entries = archive.entries().map_err(|e| refuse(format!("not a tar: {e}")))?;
    for (index, entry) in entries.enumerate() {
        if index >= limits.entries {
            return Err(refuse(format!("more than {} entries", limits.entries)));
        }
        let mut entry = entry.map_err(|e| refuse(format!("a bad entry: {e}")))?;
        let raw = entry.path().map_err(|e| refuse(format!("a bad path: {e}")))?.into_owned();
        let relative = clean(&raw, limits.depth)
            .ok_or_else(|| refuse(format!("a path that leaves the output: {}", raw.display())))?;
        if relative.as_os_str().is_empty() {
            continue;
        }
        if relative == Path::new(GUEST_EXIT) {
            let mut text = String::new();
            entry.by_ref().take(32).read_to_string(&mut text).map_err(|e| refuse(format!("{GUEST_EXIT}: {e}")))?;
            unpacked.exit_code =
                Some(text.trim().parse().map_err(|_| refuse(format!("{GUEST_EXIT} is not a number")))?);
            continue;
        }
        if relative == Path::new(OVERFLOW) {
            unpacked.overflow = true;
            continue;
        }
        if relative.starts_with(".vmjob") {
            continue;
        }
        let Some(stage) = stage else { continue };
        let at = |e: io::Error| refuse(format!("{}: {e}", relative.display()));
        match entry.header().entry_type() {
            tar::EntryType::Directory => make_dirs(stage, &relative).map_err(at)?,
            tar::EntryType::Regular | tar::EntryType::Continuous => {
                let size = entry.header().size().map_err(|e| refuse(format!("a bad size: {e}")))?;
                unpacked.bytes = unpacked.bytes.saturating_add(size);
                if unpacked.bytes > limits.bytes {
                    return Err(refuse(format!("more than {} bytes", limits.bytes)));
                }
                if let Some(parent) = relative.parent() {
                    make_dirs(stage, parent).map_err(at)?;
                }
                let executable = entry.header().mode().is_ok_and(|mode| mode & 0o111 != 0);
                write_new(stage, &relative, &mut entry.by_ref().take(size), executable).map_err(at)?;
                unpacked.files += 1;
            }
            other => {
                return Err(refuse(format!("{}: a {other:?} entry is not a file or a directory", relative.display())));
            }
        }
    }
    Ok(unpacked)
}

/// Move what the staging directory holds into the destination; a name that is
/// already there refuses the output.
fn move_into_place(stage: &Dir, root: &Dir) -> Result<(), Refused> {
    let listing = |e: io::Error| Refused(format!("listing {STAGING}: {e}"));
    let mut names = Vec::new();
    for entry in stage.entries().map_err(listing)? {
        names.push(entry.map_err(listing)?.file_name());
    }
    for name in &names {
        if root.symlink_metadata(name).is_ok() {
            return Err(Refused(format!("{} is already in the destination", Path::new(name).display())));
        }
    }
    for name in &names {
        stage.rename(name, root, name).map_err(|e| Refused(format!("{}: {e}", Path::new(name).display())))?;
    }
    Ok(())
}

/// The normal components of `path`, if it stays below its root and is at
/// most `depth` deep.
fn clean(path: &Path, depth: usize) -> Option<PathBuf> {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => out.push(part),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    (out.components().count() <= depth).then_some(out)
}

/// Make each directory of `relative` below `dir`, refusing one that exists as
/// anything but a directory.
fn make_dirs(dir: &Dir, relative: &Path) -> io::Result<()> {
    let mut at = PathBuf::new();
    for part in relative.components() {
        at.push(part);
        match dir.symlink_metadata(&at) {
            Ok(meta) if meta.is_dir() => {}
            Ok(_) => return Err(io::Error::new(io::ErrorKind::AlreadyExists, "exists and is not a directory")),
            Err(e) if e.kind() == io::ErrorKind::NotFound => dir.create_dir(&at)?,
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// Write a file below `dir` that must not exist yet: `create_new` neither
/// follows nor replaces whatever is there.
fn write_new(dir: &Dir, path: &Path, content: &mut impl Read, executable: bool) -> io::Result<()> {
    let mut options = cap_std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    let mut file = dir.open_with(path, &options)?;
    io::copy(content, &mut file)?;
    file.flush()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = if executable { 0o755 } else { 0o644 };
        file.set_permissions(cap_std::fs::Permissions::from_std(std::fs::Permissions::from_mode(mode)))?;
    }
    #[cfg(not(unix))]
    let _ = executable;
    Ok(())
}

#[cfg(test)]
mod tests;
