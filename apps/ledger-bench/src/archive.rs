//! Safe reading of untrusted dataset archives (Plan 0011; DATASETS.md "Archive handling").
//!
//! - gzip streams are decompressed with a hard cap on output bytes, so a decompression bomb
//!   fails instead of exhausting memory or disk;
//! - tar archives are read by a minimal ustar reader. It yields only *regular-file* entries
//!   whose names pass a caller-supplied validator, as in-memory byte vectors. Nothing is ever
//!   written to disk using an archive-provided path, so path traversal cannot occur. Links,
//!   directories, devices and PAX/GNU extension records are skipped and counted, and
//!   duplicate names fail.
//!
//! No archive content is ever executed.

use flate2::read::GzDecoder;
use std::{collections::BTreeMap, io::Read};

/// Decompress a gzip member, refusing more than `cap` output bytes.
pub fn gunzip_capped(compressed: &[u8], cap: u64) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    GzDecoder::new(compressed)
        .take(cap + 1)
        .read_to_end(&mut out)
        .map_err(|e| format!("gzip: {e}"))?;
    if out.len() as u64 > cap {
        return Err(format!("gzip output exceeds the {cap}-byte cap"));
    }
    Ok(out)
}

/// What a tar scan found besides the selected files.
#[derive(Debug, Default, Eq, PartialEq)]
pub struct TarReport {
    pub regular_selected: usize,
    /// Regular files whose names the validator rejected.
    pub regular_ignored: usize,
    /// Links, directories, devices, FIFOs, PAX and GNU long-name records.
    pub non_regular_skipped: usize,
}

fn octal(field: &[u8]) -> Result<u64, String> {
    let s: String = field
        .iter()
        .take_while(|b| **b != 0)
        .map(|b| *b as char)
        .collect();
    let s = s.trim();
    if s.is_empty() {
        return Ok(0);
    }
    u64::from_str_radix(s, 8).map_err(|_| format!("bad octal field {s:?}"))
}

/// Read a (decompressed) tar stream and return the regular files `select` accepts, keyed by
/// name. Fails on a malformed header, a checksum mismatch, a duplicate selected name, an
/// entry larger than `entry_cap`, or more than `total_cap` selected bytes.
pub fn tar_regular_files(
    tar: &[u8],
    select: impl Fn(&str) -> bool,
    entry_cap: u64,
    total_cap: u64,
) -> Result<(BTreeMap<String, Vec<u8>>, TarReport), String> {
    let mut out = BTreeMap::new();
    let mut report = TarReport::default();
    let mut total = 0u64;
    let mut at = 0usize;
    while at + 512 <= tar.len() {
        let h = &tar[at..at + 512];
        if h.iter().all(|b| *b == 0) {
            break; // end-of-archive marker
        }
        let stored = octal(&h[148..156])?;
        let sum: u64 = h
            .iter()
            .enumerate()
            .map(|(i, b)| {
                if (148..156).contains(&i) {
                    32
                } else {
                    u64::from(*b)
                }
            })
            .sum();
        if sum != stored {
            return Err(format!("tar header checksum mismatch at offset {at}"));
        }
        let name_field: Vec<u8> = h[0..100].iter().take_while(|b| **b != 0).copied().collect();
        let prefix: Vec<u8> = h[345..500]
            .iter()
            .take_while(|b| **b != 0)
            .copied()
            .collect();
        let mut name = String::from_utf8(name_field).map_err(|_| "non-UTF-8 tar name")?;
        if !prefix.is_empty() {
            name = format!(
                "{}/{name}",
                String::from_utf8(prefix).map_err(|_| "non-UTF-8 tar prefix")?
            );
        }
        let size = octal(&h[124..136])?;
        let typeflag = h[156];
        let data_start = at + 512;
        let padded = size
            .checked_add(511)
            .map(|s| s / 512 * 512)
            .ok_or("tar size overflow")?;
        let data_end = data_start
            .checked_add(usize::try_from(size).map_err(|_| "tar entry too large")?)
            .ok_or("tar size overflow")?;
        if data_end > tar.len() {
            return Err(format!("truncated tar entry {name:?}"));
        }
        if typeflag == b'0' || typeflag == 0 {
            if select(&name) {
                if size > entry_cap {
                    return Err(format!(
                        "tar entry {name:?} exceeds the {entry_cap}-byte cap"
                    ));
                }
                total += size;
                if total > total_cap {
                    return Err(format!(
                        "selected tar entries exceed the {total_cap}-byte cap"
                    ));
                }
                if out
                    .insert(name.clone(), tar[data_start..data_end].to_vec())
                    .is_some()
                {
                    return Err(format!("duplicate tar entry {name:?}"));
                }
                report.regular_selected += 1;
            } else {
                report.regular_ignored += 1;
            }
        } else {
            report.non_regular_skipped += 1;
        }
        at = data_start + usize::try_from(padded).map_err(|_| "tar entry too large")?;
    }
    Ok((out, report))
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::{Compression, write::GzEncoder};
    use std::io::Write;

    fn header(name: &str, size: usize, typeflag: u8) -> Vec<u8> {
        let mut h = vec![0u8; 512];
        h[..name.len()].copy_from_slice(name.as_bytes());
        h[100..107].copy_from_slice(b"0000644");
        h[124..135].copy_from_slice(format!("{size:011o}").as_bytes());
        h[156] = typeflag;
        h[257..262].copy_from_slice(b"ustar");
        h[148..156].copy_from_slice(b"        ");
        let sum: u64 = h.iter().map(|b| u64::from(*b)).sum();
        h[148..155].copy_from_slice(format!("{sum:06o}\0").as_bytes());
        h
    }

    fn tar(entries: &[(&str, &[u8], u8)]) -> Vec<u8> {
        let mut out = Vec::new();
        for (name, data, t) in entries {
            out.extend(header(name, data.len(), *t));
            out.extend_from_slice(data);
            out.resize(out.len().div_ceil(512) * 512, 0);
        }
        out.extend(vec![0u8; 1024]);
        out
    }

    #[test]
    fn only_selected_regular_files_are_returned_never_links_or_paths() {
        let t = tar(&[
            ("000001.nt.gz", b"a", b'0'),
            ("../../etc/passwd", b"x", b'0'),
            ("link", b"", b'2'),
            ("dir/", b"", b'5'),
            ("000002.nt.gz", b"bb", b'0'),
        ]);
        let (files, report) = tar_regular_files(&t, |n| n.ends_with(".nt.gz"), 10, 100).unwrap();
        assert_eq!(
            files.keys().collect::<Vec<_>>(),
            ["000001.nt.gz", "000002.nt.gz"]
        );
        assert_eq!(files["000002.nt.gz"], b"bb");
        assert_eq!(
            report,
            TarReport {
                regular_selected: 2,
                regular_ignored: 1,
                non_regular_skipped: 2
            }
        );
    }

    #[test]
    fn caps_duplicates_truncation_and_bad_checksums_fail() {
        let t = tar(&[("a.nt.gz", b"0123456789A", b'0')]);
        assert!(
            tar_regular_files(&t, |_| true, 10, 100)
                .unwrap_err()
                .contains("cap")
        );
        let t = tar(&[("a", b"1", b'0'), ("b", b"2", b'0')]);
        assert!(
            tar_regular_files(&t, |_| true, 10, 1)
                .unwrap_err()
                .contains("cap")
        );
        let t = tar(&[("a", b"1", b'0'), ("a", b"2", b'0')]);
        assert!(
            tar_regular_files(&t, |_| true, 10, 100)
                .unwrap_err()
                .contains("duplicate")
        );
        let mut t = tar(&[("a", b"1", b'0')]);
        t[0] = b'b';
        assert!(
            tar_regular_files(&t, |_| true, 10, 100)
                .unwrap_err()
                .contains("checksum")
        );
        let t = tar(&[("a", &[7u8; 600], b'0')]);
        assert!(
            tar_regular_files(&t[..700], |_| true, 1000, 1000)
                .unwrap_err()
                .contains("truncated")
        );
    }

    #[test]
    fn gzip_output_is_capped() {
        let mut e = GzEncoder::new(Vec::new(), Compression::default());
        e.write_all(&[b'x'; 10_000]).unwrap();
        let gz = e.finish().unwrap();
        assert_eq!(gunzip_capped(&gz, 10_000).unwrap().len(), 10_000);
        assert!(gunzip_capped(&gz, 9_999).unwrap_err().contains("cap"));
    }
}
