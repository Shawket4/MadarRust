//! Deterministic ustar (§11.10): `index.json` first, then entries sorted by
//! content hash; mtime 0, uid/gid 0, empty uname/gname, mode 0644; canonical
//! JSON index (sorted keys, no whitespace). Same inputs → byte-identical tar.

use std::path::PathBuf;

use bytes::Bytes;
use futures::Stream;
use tokio::io::AsyncReadExt;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TarFile {
    pub hash: String,
    pub ext: String,
    pub bytes: u64,
    pub path: PathBuf,
}

impl TarFile {
    pub fn name(&self) -> String {
        format!("{}.{}", self.hash, self.ext)
    }
}

fn json_str(s: &str) -> String {
    serde_json::to_string(s).expect("string serializes")
}

/// Canonical index: `{"files":[{"bytes":N,"ext":"…","hash":"…"}…],"missing":[…]}`
/// (`missing` only when `Some`). Keys are emitted in sorted order by hand so
/// the bytes never depend on a serde_json feature flag.
pub fn index_json(files: &[TarFile], missing: Option<&[String]>) -> Vec<u8> {
    let mut s = String::from("{\"files\":[");
    for (i, f) in files.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        s.push_str(&format!(
            "{{\"bytes\":{},\"ext\":{},\"hash\":{}}}",
            f.bytes,
            json_str(&f.ext),
            json_str(&f.hash)
        ));
    }
    s.push(']');
    if let Some(m) = missing {
        s.push_str(",\"missing\":[");
        for (i, h) in m.iter().enumerate() {
            if i > 0 {
                s.push(',');
            }
            s.push_str(&json_str(h));
        }
        s.push(']');
    }
    s.push('}');
    s.into_bytes()
}

pub fn header(name: &str, size: u64) -> [u8; 512] {
    let mut h = tar::Header::new_ustar();
    h.set_path(name).expect("short ascii names");
    h.set_size(size);
    h.set_mode(0o644);
    h.set_mtime(0);
    h.set_uid(0);
    h.set_gid(0);
    h.set_entry_type(tar::EntryType::Regular);
    h.set_cksum();
    let mut out = [0u8; 512];
    out.copy_from_slice(h.as_bytes());
    out
}

pub fn padding(size: u64) -> usize {
    ((512 - (size % 512)) % 512) as usize
}

/// Sort in place: by hash, then ext.
pub fn sort_files(files: &mut [TarFile]) {
    files.sort_by(|a, b| (&a.hash, &a.ext).cmp(&(&b.hash, &b.ext)));
}

/// Write a complete tar synchronously (bundle builder, on the blocking pool).
pub fn write_tar<W: std::io::Write>(
    out: &mut W,
    files: &[TarFile],
    missing: Option<&[String]>,
) -> std::io::Result<()> {
    let index = index_json(files, missing);
    out.write_all(&header("index.json", index.len() as u64))?;
    out.write_all(&index)?;
    out.write_all(&vec![0u8; padding(index.len() as u64)])?;
    for f in files {
        let data = std::fs::read(&f.path)?;
        if data.len() as u64 != f.bytes {
            return Err(std::io::Error::other(format!(
                "{} changed size under the builder",
                f.name()
            )));
        }
        out.write_all(&header(&f.name(), f.bytes))?;
        out.write_all(&data)?;
        out.write_all(&vec![0u8; padding(f.bytes)])?;
    }
    out.write_all(&[0u8; 1024])?;
    Ok(())
}

/// Stream a tar of already-stored files: no temp archive, no transcoding.
/// Files must exist with the listed sizes (checked before the index is built).
pub fn stream_tar(
    files: Vec<TarFile>,
    missing: Vec<String>,
) -> impl Stream<Item = Result<Bytes, std::io::Error>> {
    async_stream(files, missing)
}

fn async_stream(
    files: Vec<TarFile>,
    missing: Vec<String>,
) -> impl Stream<Item = Result<Bytes, std::io::Error>> {
    enum St {
        Index,
        File(usize),
        End,
        Done,
    }
    futures::stream::unfold(
        (St::Index, files, missing),
        |(st, files, missing)| async move {
            match st {
                St::Index => {
                    let index = index_json(&files, Some(&missing));
                    let mut buf = Vec::with_capacity(1024 + index.len());
                    buf.extend_from_slice(&header("index.json", index.len() as u64));
                    buf.extend_from_slice(&index);
                    buf.extend(std::iter::repeat_n(0u8, padding(index.len() as u64)));
                    let next = if files.is_empty() {
                        St::End
                    } else {
                        St::File(0)
                    };
                    Some((Ok(Bytes::from(buf)), (next, files, missing)))
                }
                St::File(i) => {
                    let f = &files[i];
                    let res = async {
                        let mut fh = tokio::fs::File::open(&f.path).await?;
                        let mut data = Vec::with_capacity(f.bytes as usize + 1024);
                        data.extend_from_slice(&header(&f.name(), f.bytes));
                        let mut body = Vec::with_capacity(f.bytes as usize);
                        (&mut fh).take(f.bytes).read_to_end(&mut body).await?;
                        if body.len() as u64 != f.bytes {
                            return Err(std::io::Error::other("asset file shrank"));
                        }
                        data.extend_from_slice(&body);
                        data.extend(std::iter::repeat_n(0u8, padding(f.bytes)));
                        Ok::<_, std::io::Error>(Bytes::from(data))
                    }
                    .await;
                    let next = if res.is_err() {
                        St::Done
                    } else if i + 1 < files.len() {
                        St::File(i + 1)
                    } else {
                        St::End
                    };
                    Some((res, (next, files, missing)))
                }
                St::End => Some((
                    Ok(Bytes::from_static(&[0u8; 1024])),
                    (St::Done, files, missing),
                )),
                St::Done => None,
            }
        },
    )
}
