//! Unpacking the server archive inside the extension.
//!
//! `zed::download_file` can extract while downloading, but then the archive is
//! never seen whole and its checksum can't be checked. So the archive is saved
//! as is, verified, and unpacked here. The `zip` crate doesn't build for
//! `wasm32-wasip2`, so zip archives are read directly; they only need the
//! stored and deflate methods.
//!
//! The sandbox can't set file modes or create symlinks: executables are
//! returned for the caller to mark with `zed::make_file_executable`, and
//! symlinks (the JetBrains Runtime links license files) become copies.

use std::{
    fs::{self, File},
    io::{self, BufReader, Read, Seek, SeekFrom},
    path::{Component, Path, PathBuf},
};

use flate2::read::{DeflateDecoder, GzDecoder};
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArchiveKind {
    TarGz,
    Zip,
}

/// Hex SHA-256 of a file.
pub fn sha256_file(path: &Path) -> io::Result<String> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0; 1 << 20];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex::encode(hasher.finalize()))
}

/// Unpack `archive` into `destination` and return the paths, relative to
/// `destination`, of the files the archive marks executable.
pub fn extract(archive: &Path, kind: ArchiveKind, destination: &Path) -> io::Result<Vec<PathBuf>> {
    fs::create_dir_all(destination)?;
    let mut unpacker = Unpacker {
        destination,
        executables: Vec::new(),
        links: Vec::new(),
    };
    match kind {
        ArchiveKind::TarGz => extract_tar_gz(archive, &mut unpacker)?,
        ArchiveKind::Zip => extract_zip(archive, &mut unpacker)?,
    }
    unpacker.finish()
}

struct Unpacker<'a> {
    destination: &'a Path,
    executables: Vec<PathBuf>,
    /// `(link, target)`, both relative to the destination.
    links: Vec<(PathBuf, PathBuf)>,
}

impl Unpacker<'_> {
    fn directory(&self, name: &Path) -> io::Result<()> {
        fs::create_dir_all(self.destination.join(name))
    }

    fn file(&mut self, name: &Path, contents: &mut dyn Read, executable: bool) -> io::Result<u64> {
        let path = self.destination.join(name);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let written = io::copy(contents, &mut File::create(&path)?)?;
        if executable {
            self.executables.push(name.to_path_buf());
        }
        Ok(written)
    }

    /// A symlink, with `target` relative to the link's directory.
    fn symlink(&mut self, name: &Path, target: &str) -> io::Result<()> {
        let base = name.parent().unwrap_or(Path::new(""));
        let target = normalize(&base.join(target)).ok_or_else(|| {
            invalid(format!(
                "symlink {} points outside the archive",
                name.display()
            ))
        })?;
        self.links.push((name.to_path_buf(), target));
        Ok(())
    }

    /// A hard link, with `target` relative to the archive root.
    fn hard_link(&mut self, name: &Path, target: &str) -> io::Result<()> {
        let target = entry_path(target)?;
        self.links.push((name.to_path_buf(), target));
        Ok(())
    }

    /// Copy link targets into place. Links to links resolve over several passes.
    fn finish(mut self) -> io::Result<Vec<PathBuf>> {
        while !self.links.is_empty() {
            let pending = self.links.len();
            let mut unresolved = Vec::new();
            for (link, target) in std::mem::take(&mut self.links) {
                let source = self.destination.join(&target);
                if source.is_file() {
                    let path = self.destination.join(&link);
                    if let Some(parent) = path.parent() {
                        fs::create_dir_all(parent)?;
                    }
                    fs::copy(&source, &path)?;
                    if self.executables.contains(&target) {
                        self.executables.push(link);
                    }
                } else {
                    unresolved.push((link, target));
                }
            }
            if unresolved.len() == pending {
                let (link, target) = &unresolved[0];
                return Err(invalid(format!(
                    "link {} points to {}, which is not a file in the archive",
                    link.display(),
                    target.display()
                )));
            }
            self.links = unresolved;
        }
        Ok(self.executables)
    }
}

fn extract_tar_gz(archive: &Path, unpacker: &mut Unpacker) -> io::Result<()> {
    let decoder = GzDecoder::new(BufReader::with_capacity(1 << 20, File::open(archive)?));
    let mut archive = tar::Archive::new(decoder);
    for entry in archive.entries()? {
        let mut entry = entry?;
        let name = entry.path()?.to_string_lossy().into_owned();
        let path = entry_path(&name)?;
        if path.as_os_str().is_empty() {
            continue;
        }
        let executable = entry.header().mode()? & 0o111 != 0;
        let entry_type = entry.header().entry_type();
        // `Entry::link_name` includes targets too long for the header, which
        // GNU and PAX archives keep in extension records.
        let link_target = |entry: &tar::Entry<_>| -> io::Result<String> {
            Ok(entry
                .link_name()?
                .ok_or_else(|| invalid(format!("link {name} has no target")))?
                .to_string_lossy()
                .into_owned())
        };
        match entry_type {
            tar::EntryType::Directory => unpacker.directory(&path)?,
            tar::EntryType::Regular | tar::EntryType::Continuous => {
                unpacker.file(&path, &mut entry, executable)?;
            }
            tar::EntryType::Symlink => {
                let target = link_target(&entry)?;
                unpacker.symlink(&path, &target)?;
            }
            tar::EntryType::Link => {
                let target = link_target(&entry)?;
                unpacker.hard_link(&path, &target)?;
            }
            // Extended headers are consumed by `tar`; nothing else belongs in
            // a server archive.
            _ => {}
        }
    }
    Ok(())
}

const ZIP_END: u32 = 0x0605_4b50;
const ZIP64_END_LOCATOR: u32 = 0x0706_4b50;
const ZIP64_END: u32 = 0x0606_4b50;
const ZIP_CENTRAL_ENTRY: u32 = 0x0201_4b50;
const ZIP_LOCAL_ENTRY: u32 = 0x0403_4b50;
const ZIP64_EXTRA: u16 = 0x0001;
const ZIP_HOST_UNIX: u8 = 3;
const UNIX_TYPE_MASK: u32 = 0o170_000;
const UNIX_SYMLINK: u32 = 0o120_000;

struct ZipEntry {
    name: String,
    method: u16,
    flags: u16,
    compressed_size: u64,
    size: u64,
    local_header_offset: u64,
    /// Unix mode, when the archive was made on Unix.
    mode: Option<u32>,
}

fn extract_zip(archive: &Path, unpacker: &mut Unpacker) -> io::Result<()> {
    let mut file = File::open(archive)?;
    for entry in zip_entries(&mut file)? {
        let path = entry_path(&entry.name)?;
        if path.as_os_str().is_empty() {
            continue;
        }
        if entry.flags & 1 != 0 {
            return Err(invalid(format!("{} is encrypted", entry.name)));
        }
        if entry.name.ends_with('/') {
            unpacker.directory(&path)?;
            continue;
        }

        file.seek(SeekFrom::Start(entry.local_header_offset))?;
        let mut header = [0; 30];
        file.read_exact(&mut header)?;
        if u32_at(&header, 0) != ZIP_LOCAL_ENTRY {
            return Err(invalid(format!("bad local header for {}", entry.name)));
        }
        let skip = u64::from(u16_at(&header, 26)) + u64::from(u16_at(&header, 28));
        file.seek(SeekFrom::Current(skip as i64))?;

        let compressed = BufReader::new((&mut file).take(entry.compressed_size));
        let mut contents: Box<dyn Read + '_> = match entry.method {
            0 => Box::new(compressed),
            8 => Box::new(DeflateDecoder::new(compressed)),
            method => {
                return Err(invalid(format!(
                    "{} uses unsupported compression method {method}",
                    entry.name
                )));
            }
        };

        let mode = entry.mode.unwrap_or(0);
        if mode & UNIX_TYPE_MASK == UNIX_SYMLINK {
            let mut target = String::new();
            contents.read_to_string(&mut target)?;
            unpacker.symlink(&path, &target)?;
            continue;
        }
        let written = unpacker.file(&path, &mut contents, mode & 0o111 != 0)?;
        if written != entry.size {
            return Err(invalid(format!(
                "{} is truncated: {written} of {} bytes",
                entry.name, entry.size
            )));
        }
    }
    Ok(())
}

/// Read the central directory, which lists every entry with its full sizes.
fn zip_entries(file: &mut File) -> io::Result<Vec<ZipEntry>> {
    let length = file.seek(SeekFrom::End(0))?;
    // The end record is 22 bytes plus a comment of up to 64 KiB.
    let tail_length = length.min(22 + 0xffff);
    let mut tail = vec![0; tail_length as usize];
    file.seek(SeekFrom::Start(length - tail_length))?;
    file.read_exact(&mut tail)?;
    let end = (0..tail.len().saturating_sub(21))
        .rev()
        .find(|&index| u32_at(&tail, index) == ZIP_END)
        .ok_or_else(|| invalid("not a zip archive".to_string()))?;

    let mut count = u64::from(u16_at(&tail, end + 10));
    let mut directory_size = u64::from(u32_at(&tail, end + 12));
    let mut directory_offset = u64::from(u32_at(&tail, end + 16));
    // The zip64 locator sits just before the end record, which a long comment
    // can push past the start of `tail`; read it from the file.
    let end_offset = length - tail_length + end as u64;
    let mut locator = [0; 20];
    if end_offset >= 20 {
        file.seek(SeekFrom::Start(end_offset - 20))?;
        file.read_exact(&mut locator)?;
    }
    if u32_at(&locator, 0) == ZIP64_END_LOCATOR {
        let mut record = [0; 56];
        file.seek(SeekFrom::Start(u64_at(&locator, 8)))?;
        file.read_exact(&mut record)?;
        if u32_at(&record, 0) != ZIP64_END {
            return Err(invalid("bad zip64 end record".to_string()));
        }
        count = u64_at(&record, 32);
        directory_size = u64_at(&record, 40);
        directory_offset = u64_at(&record, 48);
    }

    let mut directory = vec![0; directory_size as usize];
    file.seek(SeekFrom::Start(directory_offset))?;
    file.read_exact(&mut directory)?;

    let mut entries = Vec::with_capacity(count as usize);
    let mut position = 0;
    while entries.len() < count as usize {
        let record = directory
            .get(position..position + 46)
            .filter(|record| u32_at(record, 0) == ZIP_CENTRAL_ENTRY)
            .ok_or_else(|| invalid("truncated zip central directory".to_string()))?;
        let made_by = u16_at(record, 4);
        let name_length = usize::from(u16_at(record, 28));
        let extra_length = usize::from(u16_at(record, 30));
        let comment_length = usize::from(u16_at(record, 32));
        let external_attributes = u32_at(record, 38);
        let name_start = position + 46;
        let extra_start = name_start + name_length;
        let next = extra_start + extra_length + comment_length;
        if next > directory.len() {
            return Err(invalid("truncated zip central directory".to_string()));
        }

        let mut entry = ZipEntry {
            name: String::from_utf8_lossy(&directory[name_start..extra_start]).into_owned(),
            flags: u16_at(record, 8),
            method: u16_at(record, 10),
            compressed_size: u64::from(u32_at(record, 20)),
            size: u64::from(u32_at(record, 24)),
            local_header_offset: u64::from(u32_at(record, 42)),
            mode: ((made_by >> 8) as u8 == ZIP_HOST_UNIX).then_some(external_attributes >> 16),
        };
        apply_zip64_extra(
            &mut entry,
            &directory[extra_start..extra_start + extra_length],
        );
        entries.push(entry);
        position = next;
    }
    Ok(entries)
}

/// Sizes and offsets that don't fit 32 bits are `0xffffffff` in the entry and
/// follow, in this order, in the zip64 extra field.
fn apply_zip64_extra(entry: &mut ZipEntry, mut extra: &[u8]) {
    while extra.len() >= 4 {
        let id = u16_at(extra, 0);
        let size = usize::from(u16_at(extra, 2));
        let Some(data) = extra.get(4..4 + size) else {
            return;
        };
        if id == ZIP64_EXTRA {
            let mut values = data.chunks_exact(8).map(|value| u64_at(value, 0));
            for field in [
                &mut entry.size,
                &mut entry.compressed_size,
                &mut entry.local_header_offset,
            ] {
                if *field == u64::from(u32::MAX) {
                    match values.next() {
                        Some(value) => *field = value,
                        None => return,
                    }
                }
            }
            return;
        }
        extra = &extra[4 + size..];
    }
}

/// An archive entry name as a relative path, refusing anything that would
/// land outside the destination.
fn entry_path(name: &str) -> io::Result<PathBuf> {
    let name = name.replace('\\', "/");
    if name.starts_with('/') || name.contains(':') {
        return Err(invalid(format!("archive entry {name} is not relative")));
    }
    normalize(Path::new(&name))
        .filter(|path| path.components().all(|c| matches!(c, Component::Normal(_))))
        .ok_or_else(|| invalid(format!("archive entry {name} leaves the destination")))
}

/// Resolve `.` and `..` lexically; `None` when the path climbs above its root.
fn normalize(path: &Path) -> Option<PathBuf> {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => normalized.push(part),
            Component::CurDir => {}
            Component::ParentDir => {
                if !normalized.pop() {
                    return None;
                }
            }
            Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    Some(normalized)
}

fn invalid(message: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn u16_at(bytes: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes([bytes[offset], bytes[offset + 1]])
}

fn u32_at(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
}

fn u64_at(bytes: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap())
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "zed-java-intellij-archive-{name}-{}",
                std::process::id()
            ));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn tar_gz(path: &Path) {
        let encoder =
            flate2::write::GzEncoder::new(File::create(path).unwrap(), flate2::Compression::fast());
        let mut builder = tar::Builder::new(encoder);
        let mut add =
            |name: &str, mode: u32, entry_type: tar::EntryType, data: &[u8], link: Option<&str>| {
                let mut header = tar::Header::new_gnu();
                header.set_entry_type(entry_type);
                header.set_mode(mode);
                header.set_size(data.len() as u64);
                if let Some(link) = link {
                    header.set_link_name(link).unwrap();
                }
                builder.append_data(&mut header, name, data).unwrap();
            };
        add("server/", 0o755, tar::EntryType::Directory, b"", None);
        add(
            "server/bin/launcher",
            0o755,
            tar::EntryType::Regular,
            b"#!/bin/sh\n",
            None,
        );
        add(
            "server/legal/base/LICENSE",
            0o644,
            tar::EntryType::Regular,
            b"license",
            None,
        );
        add(
            "server/legal/other/LICENSE",
            0o777,
            tar::EntryType::Symlink,
            b"",
            Some("../base/LICENSE"),
        );
        add(
            "server/bin/alias",
            0o755,
            tar::EntryType::Link,
            b"",
            Some("server/bin/launcher"),
        );
        builder.into_inner().unwrap().finish().unwrap();
    }

    fn zip(path: &Path) {
        use zip::write::SimpleFileOptions;

        let mut writer = zip::ZipWriter::new(File::create(path).unwrap());
        let deflated =
            SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
        writer.add_directory("server/", deflated).unwrap();
        writer
            .start_file("server/bin/launcher", deflated.unix_permissions(0o755))
            .unwrap();
        writer.write_all(b"#!/bin/sh\n").unwrap();
        writer
            .start_file(
                "server/legal/base/LICENSE",
                SimpleFileOptions::default()
                    .compression_method(zip::CompressionMethod::Stored)
                    .unix_permissions(0o644),
            )
            .unwrap();
        writer.write_all(b"license").unwrap();
        writer
            .add_symlink("server/legal/other/LICENSE", "../base/LICENSE", deflated)
            .unwrap();
        writer.finish().unwrap();
    }

    fn check(destination: &Path, mut executables: Vec<PathBuf>) {
        executables.sort();
        assert!(executables.contains(&PathBuf::from("server/bin/launcher")));
        assert!(!executables.contains(&PathBuf::from("server/legal/base/LICENSE")));
        assert_eq!(
            fs::read_to_string(destination.join("server/bin/launcher")).unwrap(),
            "#!/bin/sh\n"
        );
        assert_eq!(
            fs::read_to_string(destination.join("server/legal/other/LICENSE")).unwrap(),
            "license"
        );
    }

    #[test]
    fn extracts_tar_gz_with_links_and_modes() {
        let temp = TempDir::new("tar");
        let archive = temp.0.join("server.tar.gz");
        tar_gz(&archive);
        let destination = temp.0.join("out");
        let executables = extract(&archive, ArchiveKind::TarGz, &destination).unwrap();
        assert!(executables.contains(&PathBuf::from("server/bin/alias")));
        check(&destination, executables);
    }

    #[test]
    fn extracts_tar_gz_links_with_long_targets() {
        let temp = TempDir::new("tar-long-link");
        let archive = temp.0.join("server.tar.gz");
        let deep = format!("server/{}/LICENSE", "d".repeat(120));
        let encoder = flate2::write::GzEncoder::new(
            File::create(&archive).unwrap(),
            flate2::Compression::fast(),
        );
        let mut builder = tar::Builder::new(encoder);
        let mut header = tar::Header::new_gnu();
        header.set_mode(0o644);
        header.set_size(7);
        builder
            .append_data(&mut header, &deep, &b"license"[..])
            .unwrap();
        let mut link = tar::Header::new_gnu();
        link.set_entry_type(tar::EntryType::Symlink);
        link.set_mode(0o777);
        link.set_size(0);
        builder
            .append_link(
                &mut link,
                "server/LICENSE",
                deep.trim_start_matches("server/"),
            )
            .unwrap();
        builder.into_inner().unwrap().finish().unwrap();

        let destination = temp.0.join("out");
        extract(&archive, ArchiveKind::TarGz, &destination).unwrap();
        assert_eq!(
            fs::read_to_string(destination.join("server/LICENSE")).unwrap(),
            "license"
        );
    }

    #[test]
    fn extracts_zip_with_links_and_modes() {
        let temp = TempDir::new("zip");
        let archive = temp.0.join("server.zip");
        zip(&archive);
        let destination = temp.0.join("out");
        let executables = extract(&archive, ArchiveKind::Zip, &destination).unwrap();
        check(&destination, executables);
    }

    #[test]
    fn extracts_zip64_with_a_long_comment() {
        use zip::write::SimpleFileOptions;

        let temp = TempDir::new("zip64");
        let archive = temp.0.join("server.zip");
        let mut writer = zip::ZipWriter::new(File::create(&archive).unwrap());
        // Forces the zip64 end record, so the classic one holds placeholders.
        writer.set_raw_zip64_extensible_data_sector(Box::new([]));
        writer.set_comment("x".repeat(65_530)).unwrap();
        writer
            .start_file(
                "server/bin/launcher",
                SimpleFileOptions::default()
                    .large_file(true)
                    .unix_permissions(0o755),
            )
            .unwrap();
        writer.write_all(b"#!/bin/sh\n").unwrap();
        writer.finish().unwrap();

        let destination = temp.0.join("out");
        let executables = extract(&archive, ArchiveKind::Zip, &destination).unwrap();
        assert_eq!(executables, vec![PathBuf::from("server/bin/launcher")]);
    }

    #[test]
    fn hashes_files() {
        let temp = TempDir::new("hash");
        let path = temp.0.join("file");
        fs::write(&path, b"abc").unwrap();
        assert_eq!(
            sha256_file(&path).unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn entry_paths_stay_inside_the_destination() {
        assert_eq!(entry_path("a/./b/../c").unwrap(), PathBuf::from("a/c"));
        assert_eq!(entry_path("a\\b").unwrap(), PathBuf::from("a/b"));
        assert!(entry_path("../evil").is_err());
        assert!(entry_path("a/../../evil").is_err());
        assert!(entry_path("/etc/passwd").is_err());
        assert!(entry_path("C:/evil").is_err());
    }
}
