//! Moving a carried Mix (Elixir) build to the new worktree instead of letting Mix
//! recompile it.
//!
//! Since Elixir 1.15, Mix recompiles a whole project when the compile manifest's `cwd`
//! is not the project's directory (elixir-lang/elixir#12462), because every .beam names
//! its source by absolute path. A cloned `_build` therefore costs a full compile in
//! every new worktree. Instead, the source worktree's path is rewritten to the new one
//! wherever Mix and the BEAM keep it for the project's own (local) apps:
//!
//! - `.mix/compile.elixir` (the manifest: `cwd`, local deps config) and
//!   `.mix/cached_dot_formatter`
//! - `.beam` chunks `CInf` (compile info: `:source`), `Dbgi` (debug info), `Attr`
//!   (attributes such as `@external_resource`) and `LitT` (literals such as `__DIR__`)
//! - `ebin/*.app` (compile-time config computed from paths)
//!
//! Stack traces, `module_info(:compile)[:source]` and tools reading them then point into
//! the new worktree. Hex and git deps (`deps/`) are left alone: Mix does not recompile
//! them on a move, and there are thousands of their .beam files. Every file keeps its
//! mtime, which Mix compares with its manifests.

use anyhow::{bail, Context, Result};
use eetf::{Binary, ByteList, FixInteger, ImproperList, List, Map, Term, Tuple};
use flate2::{read::ZlibDecoder, write::ZlibEncoder, Compression};
use md5::{Digest, Md5};
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

/// Relocates the local apps of every Mix build carried into `dest` from `source`.
/// Returns the number of files rewritten.
pub fn relocate(source: &Path, dest: &Path) -> Result<usize> {
    let old = path_str(source)?;
    let new = path_str(dest)?;
    let files = local_app_files(dest, &old)?;
    let count = files.len();

    // All or nothing: every file is rewritten in memory first, and only written once
    // all of them succeeded. A half-moved build would be a mix of both paths.
    let workers = std::thread::available_parallelism().map_or(4, |n| n.get());
    let chunk = count.div_ceil(workers).max(1);
    let rewritten: Vec<(PathBuf, Vec<u8>)> = std::thread::scope(|scope| {
        let handles: Vec<_> = files
            .chunks(chunk)
            .map(|paths| {
                scope.spawn(|| {
                    paths
                        .iter()
                        .map(|p| Ok((p.clone(), rewrite(p, &old, &new)?)))
                        .collect::<Result<Vec<_>>>()
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().expect("relocation thread panicked"))
            .collect::<Result<Vec<Vec<_>>>>()
    })?
    .into_iter()
    .flatten()
    .collect();
    for (path, bytes) in rewritten {
        write_keeping_mtime(&path, &bytes)?;
    }
    Ok(count)
}

fn path_str(path: &Path) -> Result<String> {
    path.to_str()
        .map(str::to_owned)
        .with_context(|| format!("{} is not UTF-8", path.display()))
}

/// Files of the apps whose manifest `cwd` is the source worktree or a directory in it,
/// except dependencies under `deps/`.
fn local_app_files(dest: &Path, old: &str) -> Result<Vec<PathBuf>> {
    let deps = format!("{old}/deps/");
    let mut files = Vec::new();
    for env in read_dirs(&dest.join("_build"))? {
        for app in read_dirs(&env.join("lib"))? {
            let manifest = app.join(".mix/compile.elixir");
            let Ok(bytes) = fs::read(&manifest) else {
                continue;
            };
            let Some(cwd) = manifest_cwd(&bytes) else {
                continue;
            };
            let inside = cwd == old || cwd.starts_with(&format!("{old}/"));
            if !inside || cwd.starts_with(&deps) {
                continue;
            }
            files.push(manifest);
            let formatter = app.join(".mix/cached_dot_formatter");
            if formatter.is_file() {
                files.push(formatter);
            }
            for dir in ["ebin", "consolidated"] {
                for entry in read_files(&app.join(dir))? {
                    if matches!(
                        entry.extension().and_then(|e| e.to_str()),
                        Some("beam" | "app")
                    ) {
                        files.push(entry);
                    }
                }
            }
        }
    }
    Ok(files)
}

fn read_dirs(dir: &Path) -> Result<Vec<PathBuf>> {
    read_entries(dir, true)
}

fn read_files(dir: &Path) -> Result<Vec<PathBuf>> {
    read_entries(dir, false)
}

fn read_entries(dir: &Path, dirs: bool) -> Result<Vec<PathBuf>> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Ok(Vec::new());
    };
    let mut paths = Vec::new();
    for entry in entries {
        let entry = entry?;
        if entry.file_type()?.is_dir() == dirs {
            paths.push(entry.path());
        }
    }
    Ok(paths)
}

/// The manifest is `{vsn, modules, sources, exports, parents, cache_key, cwd, ...}`.
fn manifest_cwd(bytes: &[u8]) -> Option<String> {
    match Term::decode(bytes).ok()? {
        Term::Tuple(Tuple { elements }) => match elements.get(6)? {
            Term::Binary(Binary { bytes }) => String::from_utf8(bytes.clone()).ok(),
            _ => None,
        },
        _ => None,
    }
}

fn rewrite(path: &Path, old: &str, new: &str) -> Result<Vec<u8>> {
    let bytes = fs::read(path).with_context(|| format!("read {}", path.display()))?;
    match path.extension().and_then(|e| e.to_str()) {
        Some("beam") => beam(&bytes, old, new),
        Some("app") => Ok(replace_bytes(&bytes, old.as_bytes(), new.as_bytes())),
        _ => etf(&bytes, old, new, true),
    }
    .with_context(|| format!("relocate {}", path.display()))
}

fn write_keeping_mtime(path: &Path, bytes: &[u8]) -> Result<()> {
    let mtime = fs::metadata(path)?.modified()?;
    fs::write(path, bytes)?;
    fs::File::options()
        .write(true)
        .open(path)?
        .set_modified(mtime)?;
    Ok(())
}

/// Rebuilds a .beam (`FOR1` <size> `BEAM` (<id> <size> <data, padded to 4>)*) with its
/// path-holding chunks rewritten.
fn beam(bytes: &[u8], old: &str, new: &str) -> Result<Vec<u8>> {
    if bytes.len() < 12 || &bytes[0..4] != b"FOR1" || &bytes[8..12] != b"BEAM" {
        bail!("not a BEAM file");
    }
    let mut body = b"BEAM".to_vec();
    let mut at = 12;
    while at + 8 <= bytes.len() {
        let id = &bytes[at..at + 4];
        let len = u32::from_be_bytes(bytes[at + 4..at + 8].try_into()?) as usize;
        let data = bytes.get(at + 8..at + 8 + len).context("truncated chunk")?;
        let data = match id {
            b"LitT" => literals(data, old, new)?,
            b"CInf" | b"Dbgi" | b"Attr" => etf(data, old, new, false)?,
            _ => data.to_vec(),
        };
        body.extend_from_slice(id);
        body.extend_from_slice(&u32::try_from(data.len())?.to_be_bytes());
        body.extend_from_slice(&data);
        body.resize(body.len().next_multiple_of(4), 0);
        at += 8 + len.next_multiple_of(4);
    }
    let mut out = b"FOR1".to_vec();
    out.extend_from_slice(&u32::try_from(body.len())?.to_be_bytes());
    out.extend_from_slice(&body);
    Ok(out)
}

/// `LitT`: <<UncompressedSize:32, Zlib>> (size 0: stored as is) of
/// <<Count:32, (<<Size:32, ETF>>)*>>, each ETF a full `term_to_binary` result.
fn literals(data: &[u8], old: &str, new: &str) -> Result<Vec<u8>> {
    let size = u32::from_be_bytes(data.get(0..4).context("empty LitT")?.try_into()?);
    let table = if size == 0 {
        data[4..].to_vec()
    } else {
        let mut plain = Vec::with_capacity(size as usize);
        ZlibDecoder::new(&data[4..]).read_to_end(&mut plain)?;
        plain
    };
    if !contains(&table, old.as_bytes()) {
        return Ok(data.to_vec());
    }
    let count = u32::from_be_bytes(table.get(0..4).context("empty literal table")?.try_into()?);
    let mut out = count.to_be_bytes().to_vec();
    let mut literals = Vec::with_capacity(count as usize);
    let mut at = 4;
    for _ in 0..count {
        let len = u32::from_be_bytes(
            table
                .get(at..at + 4)
                .context("truncated literal")?
                .try_into()?,
        ) as usize;
        let literal = table
            .get(at + 4..at + 4 + len)
            .context("truncated literal")?;
        literals.push(etf(literal, old, new, false)?);
        at += 4 + len;
    }
    phoenix_template_hashes(&mut literals, old, new)?;
    for literal in &literals {
        out.extend_from_slice(&u32::try_from(literal.len())?.to_be_bytes());
        out.extend_from_slice(literal);
    }
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(&out)?;
    let mut chunk = u32::try_from(out.len())?.to_be_bytes().to_vec();
    chunk.extend_from_slice(&encoder.finish()?);
    Ok(chunk)
}

/// Phoenix's `embed_templates` compiles `__mix_recompile__?/0` to compare a literal
/// `md5(sorted(Path.wildcard("#{dir}/#{pattern}.{#{extensions}}")))` of absolute
/// template paths with the current one, so a moved module would still recompile
/// (phoenixframework/phoenix_template#15). For each directory and glob literal of the
/// module, the old hash is recomputed from the new worktree's files under their old
/// paths; only a 16-byte literal it matches exactly is replaced, by the hash of the
/// same files under their new paths. Nothing is replaced on a guess.
fn phoenix_template_hashes(literals: &mut [Vec<u8>], old: &str, new: &str) -> Result<()> {
    let decoded: Vec<Option<Term>> = literals
        .iter()
        .map(|l| Term::decode(l.as_slice()).ok())
        .collect();
    let hashes: Vec<[u8; 16]> = decoded
        .iter()
        .flatten()
        .filter_map(|t| match t {
            Term::Binary(Binary { bytes }) => bytes.as_slice().try_into().ok(),
            _ => None,
        })
        .collect();
    if hashes.is_empty() {
        return Ok(());
    }
    let strings: Vec<String> = decoded
        .iter()
        .flatten()
        .filter_map(|t| match t {
            Term::Binary(Binary { bytes }) => String::from_utf8(bytes.clone()).ok(),
            _ => None,
        })
        .collect();
    let inside = format!("{new}/");
    let dirs = strings
        .iter()
        .filter(|s| (s.as_str() == new || s.starts_with(&inside)) && Path::new(s).is_dir());
    let patterns: Vec<&String> = strings
        .iter()
        .filter(|s| s.contains('*') && !s.starts_with('/'))
        .collect();
    // Phoenix's default engines, with and without LiveView's leex.
    let extension_sets: [&[&str]; 3] = [
        &["eex", "exs", "heex", "leex"],
        &["eex", "exs", "heex"],
        &["eex", "exs"],
    ];

    let mut replacements: Vec<([u8; 16], [u8; 16])> = Vec::new();
    for dir in dirs {
        for pattern in &patterns {
            for extensions in extension_sets {
                let files = template_files(dir, pattern, extensions);
                if files.is_empty() {
                    continue;
                }
                let digest = |root: &str| {
                    let mut paths: Vec<String> = files
                        .iter()
                        .map(|f| format!("{root}{}", &f[new.len()..]))
                        .collect();
                    paths.sort();
                    let mut md5 = Md5::new();
                    paths.iter().for_each(|p| md5.update(p.as_bytes()));
                    <[u8; 16]>::from(md5.finalize())
                };
                let before = digest(old);
                if hashes.contains(&before) {
                    replacements.push((before, digest(new)));
                    break;
                }
            }
        }
    }
    for (literal, term) in literals.iter_mut().zip(&decoded) {
        let Some(Term::Binary(Binary { bytes })) = term else {
            continue;
        };
        if let Some((_, after)) = replacements
            .iter()
            .find(|(before, _)| before.as_slice() == bytes.as_slice())
        {
            literal.clear();
            Term::Binary(Binary::from(after.as_slice()))
                .encode(&mut *literal)
                .map_err(|e| anyhow::anyhow!("encode: {e:?}"))?;
        }
    }
    Ok(())
}

/// `Path.wildcard("#{dir}/#{pattern}.{#{extensions}}")` for the patterns
/// `embed_templates` produces: an optional directory and a file-name glob with `*`.
fn template_files(dir: &str, pattern: &str, extensions: &[&str]) -> Vec<String> {
    let (sub, name_glob) = pattern.rsplit_once('/').unwrap_or(("", pattern));
    if sub.contains('*') {
        return Vec::new();
    }
    let base = if sub.is_empty() {
        dir.to_owned()
    } else {
        format!("{dir}/{sub}")
    };
    let Ok(entries) = fs::read_dir(&base) else {
        return Vec::new();
    };
    let mut files = Vec::new();
    for entry in entries.flatten() {
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let matches = !name.starts_with('.')
            && extensions.iter().any(|ext| {
                name.strip_suffix(&format!(".{ext}"))
                    .is_some_and(|stem| glob_match(name_glob, stem))
            });
        if matches {
            files.push(format!("{base}/{name}"));
        }
    }
    files
}

/// `*` matches any run of characters (file names have no `/`).
fn glob_match(glob: &str, text: &str) -> bool {
    match glob.split_once('*') {
        None => glob == text,
        Some((head, tail)) => {
            text.starts_with(head)
                && (head.len()..=text.len()).any(|i| glob_match(tail, &text[i..]))
        }
    }
}

/// Decodes an external term, rewrites it and encodes it again (zlib-compressed as
/// `term_to_binary(term, [:compressed])` when `compress`). Untouched terms are returned
/// byte for byte.
fn etf(bytes: &[u8], old: &str, new: &str, compress: bool) -> Result<Vec<u8>> {
    let decoded = Term::decode(bytes).map_err(|e| anyhow::anyhow!("decode: {e:?}"))?;
    let rewritten = term(decoded.clone(), old, new);
    if rewritten == decoded {
        return Ok(bytes.to_vec());
    }
    let mut plain = Vec::new();
    rewritten
        .encode(&mut plain)
        .map_err(|e| anyhow::anyhow!("encode: {e:?}"))?;
    if !compress {
        return Ok(plain);
    }
    // <<131, 80, UncompressedSize:32, Zlib(term without its version byte)>>
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(&plain[1..])?;
    let mut out = vec![131, 80];
    out.extend_from_slice(&u32::try_from(plain.len() - 1)?.to_be_bytes());
    out.extend_from_slice(&encoder.finish()?);
    Ok(out)
}

fn term(t: Term, old: &str, new: &str) -> Term {
    match t {
        Term::Binary(Binary { bytes }) => Term::Binary(Binary {
            bytes: replace_bytes(&bytes, old.as_bytes(), new.as_bytes()),
        }),
        // STRING_EXT: a charlist of code points below 256
        Term::ByteList(ByteList { bytes }) => {
            let text: String = bytes.iter().map(|&b| char::from(b)).collect();
            charlist(&text, old, new).unwrap_or(Term::ByteList(ByteList { bytes }))
        }
        Term::List(List { elements }) => {
            let text: Option<String> = elements
                .iter()
                .map(|e| match e {
                    Term::FixInteger(FixInteger { value }) => {
                        u32::try_from(*value).ok().and_then(char::from_u32)
                    }
                    _ => None,
                })
                .collect();
            match text.and_then(|text| charlist(&text, old, new)) {
                Some(relocated) => relocated,
                None => Term::List(List {
                    elements: elements.into_iter().map(|e| term(e, old, new)).collect(),
                }),
            }
        }
        Term::ImproperList(ImproperList { elements, last }) => Term::ImproperList(ImproperList {
            elements: elements.into_iter().map(|e| term(e, old, new)).collect(),
            last: Box::new(term(*last, old, new)),
        }),
        Term::Tuple(Tuple { elements }) => Term::Tuple(Tuple {
            elements: elements.into_iter().map(|e| term(e, old, new)).collect(),
        }),
        Term::Map(Map { map }) => Term::Map(Map {
            map: map
                .into_iter()
                .map(|(k, v)| (term(k, old, new), term(v, old, new)))
                .collect(),
        }),
        other => other,
    }
}

/// A charlist naming the old root, as the same charlist naming the new one.
fn charlist(text: &str, old: &str, new: &str) -> Option<Term> {
    let replaced = String::from_utf8(replace_bytes(
        text.as_bytes(),
        old.as_bytes(),
        new.as_bytes(),
    ))
    .ok()?;
    if text.is_empty() || replaced == text {
        return None;
    }
    if replaced.chars().all(|c| (c as u32) < 256) {
        Some(Term::ByteList(ByteList {
            bytes: replaced.chars().map(|c| c as u8).collect(),
        }))
    } else {
        Some(Term::List(List {
            elements: replaced
                .chars()
                .map(|c| Term::FixInteger(FixInteger { value: c as i32 }))
                .collect(),
        }))
    }
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty() && haystack.windows(needle.len()).any(|w| w == needle)
}

/// Replaces `old` where it is a whole path prefix: followed by the end, `/` or anything
/// that cannot continue a file name, so `/src/app` never matches `/src/application`.
fn replace_bytes(haystack: &[u8], old: &[u8], new: &[u8]) -> Vec<u8> {
    if !contains(haystack, old) {
        return haystack.to_vec();
    }
    let continues_name =
        |b: &u8| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.') || *b >= 0x80;
    let mut out = Vec::with_capacity(haystack.len() + new.len());
    let mut at = 0;
    while at < haystack.len() {
        if haystack[at..].starts_with(old)
            && !haystack.get(at + old.len()).is_some_and(continues_name)
        {
            out.extend_from_slice(new);
            at += old.len();
        } else {
            out.push(haystack[at]);
            at += 1;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encode(t: &Term) -> Vec<u8> {
        let mut out = Vec::new();
        t.encode(&mut out).unwrap();
        out
    }

    #[test]
    fn rewrites_binaries_and_charlists_everywhere() {
        let t = Term::Tuple(Tuple {
            elements: vec![
                Term::Binary(Binary::from(b"/src/app/lib/a.ex".as_slice())),
                Term::from(String::from("/src/app/lib/b.ex")),
                Term::Binary(Binary::from(b"/src/application".as_slice())),
                Term::Atom(eetf::Atom::from("/src/app")),
            ],
        });
        let Term::Tuple(Tuple { elements }) = term(t, "/src/app", "/wt/x") else {
            panic!()
        };
        assert_eq!(
            elements[0],
            Term::Binary(Binary::from(b"/wt/x/lib/a.ex".as_slice()))
        );
        assert_eq!(elements[1], Term::from(String::from("/wt/x/lib/b.ex")));
        // a sibling directory sharing the prefix is not the root
        assert_eq!(
            elements[2],
            Term::Binary(Binary::from(b"/src/application".as_slice()))
        );
        // atoms are names, not paths
        assert_eq!(elements[3], Term::Atom(eetf::Atom::from("/src/app")));
    }

    #[test]
    fn untouched_terms_keep_their_bytes() {
        let bytes = encode(&Term::Binary(Binary::from(b"elsewhere".as_slice())));
        assert_eq!(etf(&bytes, "/src/app", "/wt/x", true).unwrap(), bytes);
    }

    #[test]
    fn literal_tables_round_trip() {
        let literal = encode(&Term::Binary(Binary::from(b"/src/app/lib".as_slice())));
        let other = encode(&Term::Atom(eetf::Atom::from("ok")));
        let mut table = 2u32.to_be_bytes().to_vec();
        for l in [&literal, &other] {
            table.extend_from_slice(&u32::try_from(l.len()).unwrap().to_be_bytes());
            table.extend_from_slice(l);
        }
        let mut chunk = 0u32.to_be_bytes().to_vec();
        chunk.extend_from_slice(&table);

        let out = literals(&chunk, "/src/app", "/wt/x").unwrap();
        let size = u32::from_be_bytes(out[0..4].try_into().unwrap()) as usize;
        let mut plain = Vec::new();
        ZlibDecoder::new(&out[4..]).read_to_end(&mut plain).unwrap();
        assert_eq!(plain.len(), size);
        assert_eq!(&plain[0..4], &2u32.to_be_bytes());
        let len = u32::from_be_bytes(plain[4..8].try_into().unwrap()) as usize;
        assert_eq!(
            Term::decode(&plain[8..8 + len]).unwrap(),
            Term::Binary(Binary::from(b"/wt/x/lib".as_slice()))
        );
        assert_eq!(&plain[8 + len + 4..], other.as_slice());
    }

    #[test]
    fn glob_matches_like_path_wildcard() {
        assert!(glob_match("*", "home.html"));
        assert!(glob_match("*.html", "home.html"));
        assert!(!glob_match("*.html", "home.text"));
        assert!(glob_match("a*c", "abbc"));
    }

    #[test]
    fn phoenix_template_hash_is_recomputed_only_on_an_exact_match() {
        let tmp = tempfile::tempdir().unwrap();
        let new = tmp.path().join("wt").to_str().unwrap().to_owned();
        let old = "/src/app";
        let dir = format!("{new}/lib/web/page_html");
        fs::create_dir_all(&dir).unwrap();
        fs::write(format!("{dir}/home.html.heex"), "").unwrap();
        fs::write(format!("{dir}/notes.txt"), "").unwrap();
        let md5 = |s: &str| <[u8; 16]>::from(Md5::digest(s.as_bytes()));
        let old_hash = md5("/src/app/lib/web/page_html/home.html.heex");
        let new_hash = md5(&format!("{dir}/home.html.heex"));
        let unrelated = *b"Email (optional)";
        let binary = |b: &[u8]| encode(&Term::Binary(Binary::from(b)));
        let mut literals = vec![
            binary(&old_hash),
            binary(&unrelated),
            binary(format!("{new}/lib/web").as_bytes()),
            binary(b"page_html/*"),
        ];
        phoenix_template_hashes(&mut literals, old, &new).unwrap();
        assert_eq!(literals[0], binary(&new_hash));
        assert_eq!(literals[1], binary(&unrelated));
    }

    #[test]
    fn compressed_output_decodes() {
        let bytes = encode(&Term::Binary(Binary::from(b"/src/app/mix.exs".as_slice())));
        let out = etf(&bytes, "/src/app", "/wt/x", true).unwrap();
        assert_eq!(&out[0..2], &[131, 80]);
        assert_eq!(
            Term::decode(out.as_slice()).unwrap(),
            Term::Binary(Binary::from(b"/wt/x/mix.exs".as_slice()))
        );
    }
}
