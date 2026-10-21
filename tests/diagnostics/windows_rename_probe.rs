use std::{fs, io, path::{Path, PathBuf}};
use std::os::windows::{ffi::OsStrExt, fs::OpenOptionsExt, io::AsRawHandle};

#[link(name = "kernel32")]
unsafe extern "system" {
    fn SetFileInformationByHandle(handle: *mut std::ffi::c_void, class: i32,
        info: *const std::ffi::c_void, size: u32) -> i32;
}

#[repr(C)]
struct RenameInfo {
    flags: u32,
    root: *mut std::ffi::c_void,
    name_bytes: u32,
    name: [u16; 1],
}

fn directory_handle(path: &Path, access: u32) -> io::Result<fs::File> {
    fs::OpenOptions::new().access_mode(access).share_mode(7)
        .custom_flags(0x0200_0000).open(path)
}

fn setup(root: &Path, label: &str, destination_exists: bool) -> io::Result<(PathBuf, PathBuf, PathBuf)> {
    let case = root.join(label);
    let source = case.join("source");
    let destination = case.join("destination");
    fs::create_dir_all(source.join("child"))?;
    fs::write(source.join("child/payload.txt"), b"original payload")?;
    if destination_exists { fs::create_dir(&destination)?; }
    Ok((case, source, destination))
}

fn report(label: &str, result: io::Result<()>, source: &Path, destination: &Path) {
    match result {
        Ok(()) => println!("{label}: OK"),
        Err(e) => println!("{label}: ERROR kind={:?} raw_os_error={:?} message={e}", e.kind(), e.raw_os_error()),
    }
    println!("  source_exists={} destination_exists={} source_payload={} destination_payload={}",
        source.exists(), destination.exists(), source.join("child/payload.txt").exists(),
        destination.join("child/payload.txt").exists());
}

fn direct_rename(source: &Path, parent: &Path, class: i32) -> io::Result<()> {
    // DELETE | FILE_READ_ATTRIBUTES; metadata/rename handle only.
    let source_handle = directory_handle(source, 0x0001_0080)?;
    // Use a known absolute temporary path and NULL RootDirectory, matching the
    // documented Win32 API form. This probe does not change production I/O.
    let name: Vec<u16> = parent.join("destination").as_os_str().encode_wide().collect();
    let bytes = std::mem::size_of::<RenameInfo>() + name.len() * 2;
    let mut storage = vec![0_u64; bytes.div_ceil(8)];
    let info = storage.as_mut_ptr().cast::<RenameInfo>();
    unsafe {
        (*info).flags = if class == 22 { 3 } else { 1 };
        (*info).root = std::ptr::null_mut();
        (*info).name_bytes = (name.len() * 2) as u32;
        std::ptr::copy_nonoverlapping(name.as_ptr(), std::ptr::addr_of_mut!((*info).name).cast(), name.len());
        if SetFileInformationByHandle(source_handle.as_raw_handle(), class, info.cast(), bytes as u32) == 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

fn main() -> io::Result<()> {
    let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    let root = PathBuf::from(format!(r"Z:\tmp\rustnfs-rename-probe-data-{stamp}"));
    println!("Rust std/Win32 directory rename probe; root={}", root.display());
    for (label, exists, pins) in [
        ("std_new_destination", false, false),
        ("std_replace_empty_destination", true, false),
        ("std_new_destination_with_pins", false, true),
        ("std_replace_empty_destination_with_pins", true, true),
    ] {
        let (_case, source, destination) = setup(&root, label, exists)?;
        let mut held = Vec::new();
        if pins {
            held.push(directory_handle(&source, 0x80)?);
            held.push(directory_handle(&source.join("child"), 0x80)?);
            held.push(fs::OpenOptions::new().read(true).share_mode(7).open(source.join("child/payload.txt"))?);
        }
        report(label, fs::rename(&source, &destination), &source, &destination);
        drop(held);
    }
    for (label, exists, class) in [
        ("FileRenameInfo_class3_new_destination", false, 3),
        ("FileRenameInfoEx_class22_new_destination", false, 22),
        ("FileRenameInfoEx_class22_replace_empty_destination", true, 22),
    ] {
        let (case, source, destination) = setup(&root, label, exists)?;
        report(label, direct_rename(&source, &case, class), &source, &destination);
    }
    Ok(())
}
