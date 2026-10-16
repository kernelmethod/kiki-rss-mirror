//! Handing the kiki executable's code back to the kernel once a process is
//! done running it.
//!
//! Every Kiki process is the same executable, but each runs only part of
//! it. The script host is the extreme case: compiling plugins runs
//! Cranelift and the rest of Wasmtime's compiler, several MiB of code that
//! no other process touches, and that the script host itself runs again
//! only when a plugin changes. Those pages are clean and file-backed, so
//! the kernel would reclaim them under memory pressure, but until then they
//! count towards the process's resident and proportional memory (and its
//! cgroup's), and in a quiet deployment they made up about a third of the
//! script host's.
//!
//! [`page_out_executable`] asks the kernel to reclaim them now, with
//! `madvise(MADV_PAGEOUT)` over the executable's read-only segments. The
//! kernel only reclaims pages no other process maps, so the code that
//! Kiki's processes share stays where it is, and a page this process runs
//! again is read back in from the page cache or the executable on its next
//! use.

use std::io;

/// Ask the kernel to reclaim the pages of the kiki executable's code and
/// read-only data that only this process maps; see the module
/// documentation. Returns how many bytes were advised.
///
/// Its writable segment is left alone: its pages are this process's own
/// copies, which reclaiming would push to swap.
///
/// # Errors
///
/// Returns the error `madvise` gave for the first segment it refused, such
/// as `EINVAL` from kernels before 5.4, which lack `MADV_PAGEOUT`. The
/// advice only ever reclaims memory early, so callers can ignore it.
pub fn page_out_executable() -> io::Result<usize> {
    // SAFETY: `sysconf` only reads a constant of the system.
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    let page = usize::try_from(page)
        .ok()
        .filter(|p| p.is_power_of_two())
        .ok_or_else(|| io::Error::other("the page size is unknown"))?;
    let mut advised = 0;
    for (start, len) in read_only_segments() {
        // Whole pages of the segment only: a page it shares with the
        // writable segment beside it is left alone.
        let first = start.next_multiple_of(page);
        let end = start.saturating_add(len) & !(page - 1);
        if end <= first {
            continue;
        }
        // SAFETY: the range lies within a segment of the executable that is
        // mapped for as long as the process runs. `MADV_PAGEOUT` changes
        // nothing about its contents: a page reclaimed from a file-backed
        // mapping is read back from the file when it is next used.
        let ret =
            unsafe { libc::madvise(first as *mut libc::c_void, end - first, libc::MADV_PAGEOUT) };
        if ret != 0 {
            return Err(io::Error::last_os_error());
        }
        advised += end - first;
    }
    Ok(advised)
}

/// The start addresses and lengths of the executable's loadable segments
/// that are not writable.
fn read_only_segments() -> Vec<(usize, usize)> {
    unsafe extern "C" fn first_object(
        info: *mut libc::dl_phdr_info,
        _size: libc::size_t,
        data: *mut libc::c_void,
    ) -> libc::c_int {
        // SAFETY: `dl_iterate_phdr` passes a valid `info` for the duration of
        // the call, and `data` is the vector `read_only_segments` passed it,
        // which nothing else uses meanwhile.
        let (info, segments) = unsafe { (&*info, &mut *data.cast::<Vec<(usize, usize)>>()) };
        if !info.dlpi_phdr.is_null() {
            for i in 0..usize::from(info.dlpi_phnum) {
                // SAFETY: `dlpi_phdr` points at `dlpi_phnum` program headers.
                let header = unsafe { &*info.dlpi_phdr.add(i) };
                if header.p_type == libc::PT_LOAD && header.p_flags & libc::PF_W == 0 {
                    // Addresses that do not fit in a `usize` cannot be mapped.
                    if let (Ok(vaddr), Ok(len)) = (
                        usize::try_from(header.p_vaddr),
                        usize::try_from(header.p_memsz),
                    ) {
                        let base = info.dlpi_addr as usize;
                        segments.push((base.wrapping_add(vaddr), len));
                    }
                }
            }
        }
        // The first object is the executable itself: stop there, leaving
        // the shared libraries of a dynamically linked build alone.
        1
    }

    let mut segments: Vec<(usize, usize)> = Vec::new();
    // SAFETY: `first_object` only reads the headers it is given, and writes
    // only to `segments`, which outlives the call.
    unsafe {
        libc::dl_iterate_phdr(
            Some(first_object),
            (&raw mut segments).cast::<libc::c_void>(),
        );
    }
    segments
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The executable's code is among the segments found: the address of
    /// a function in it lies in one of them.
    #[test]
    fn the_executables_code_is_found() {
        let here = the_executables_code_is_found as *const () as usize;
        let segments = read_only_segments();
        assert!(
            segments
                .iter()
                .any(|(start, len)| (*start..start + len).contains(&here)),
            "{here:#x} is in none of {segments:x?}"
        );
    }
}
