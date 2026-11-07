//! Minimal NBT reader/writer used to maintain `servers.dat`.
//!
//! Modern Minecraft (1.20.2+) stores the multiplayer list as an *uncompressed*
//! NBT compound. Older versions used gzip, so the reader accepts both formats
//! while the writer always emits plain NBT. We only need compounds, lists,
//! strings and bytes for `servers.dat`, but the parser understands every
//! vanilla tag so unknown fields written by the game survive an update.

use std::collections::BTreeMap;
use std::fs;
use std::io::Read;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use flate2::read::GzDecoder;

const MAX_DEPTH: u8 = 64;
const MAX_LIST: usize = 1_000_000;
const MAX_BYTE_ARRAY: usize = 64 * 1024 * 1024;
/// Upper bound for the decompressed NBT document. A hostile `overrides/` entry
/// could otherwise turn a tiny gzip file into gigabytes of RAM.
const MAX_NBT_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq)]
enum Nbt {
    Byte(i8),
    Short(i16),
    Int(i32),
    Long(i64),
    Float(f32),
    Double(f64),
    ByteArray(Vec<u8>),
    String(String),
    List(Vec<Nbt>),
    Compound(BTreeMap<String, Nbt>),
    IntArray(Vec<i32>),
    LongArray(Vec<i64>),
}

impl Nbt {
    fn type_id(&self) -> u8 {
        match self {
            Self::Byte(_) => 1,
            Self::Short(_) => 2,
            Self::Int(_) => 3,
            Self::Long(_) => 4,
            Self::Float(_) => 5,
            Self::Double(_) => 6,
            Self::ByteArray(_) => 7,
            Self::String(_) => 8,
            Self::List(_) => 9,
            Self::Compound(_) => 10,
            Self::IntArray(_) => 11,
            Self::LongArray(_) => 12,
        }
    }
}

/// Adds (or updates) one entry in `<instance>/servers.dat` without discarding
/// servers the player added manually.
pub fn ensure_server_entry(instance_dir: &Path, name: &str, ip: &str) -> Result<(), String> {
    let path = instance_dir.join("servers.dat");
    let mut root = match fs::read(&path) {
        Ok(bytes) => match decode(&bytes) {
            Ok(root) => root,
            Err(error) => {
                // The player's manual server list is user data: keep a copy
                // before the corrupt file is replaced. Minecraft itself would
                // silently reset the list, which is exactly what we avoid here.
                let backup = corrupt_backup_path(&path);
                match fs::rename(&path, &backup) {
                    Ok(()) => tracing::warn!(
                        %error,
                        path = %path.display(),
                        backup = %backup.display(),
                        "servers.dat is corrupt; backup saved and file recreated"
                    ),
                    Err(rename_error) => tracing::warn!(
                        %error,
                        %rename_error,
                        path = %path.display(),
                        "servers.dat is corrupt and could not be backed up; recreating"
                    ),
                }
                new_root()
            }
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => new_root(),
        Err(error) => return Err(format!("не удалось прочитать servers.dat: {error}")),
    };

    merge_server(&mut root, name, ip)?;
    let bytes = encode(&root)?;
    let temporary = path.with_extension("dat.tmp");
    fs::write(&temporary, bytes)
        .map_err(|error| format!("не удалось записать {}: {error}", temporary.display()))?;
    fs::rename(&temporary, &path)
        .map_err(|error| format!("не удалось заменить {}: {error}", path.display()))?;
    Ok(())
}

fn new_root() -> Nbt {
    let mut root = BTreeMap::new();
    root.insert("servers".to_owned(), Nbt::List(Vec::new()));
    Nbt::Compound(root)
}

/// `servers.dat.broken-<unix-seconds>.bak`: a timestamp keeps an earlier
/// backup intact when the game corrupts the file more than once.
fn corrupt_backup_path(path: &Path) -> std::path::PathBuf {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0);
    path.with_extension(format!("dat.broken-{stamp}.bak"))
}

fn server_entry(name: &str, ip: &str) -> Nbt {
    let mut entry = BTreeMap::new();
    entry.insert("name".to_owned(), Nbt::String(name.to_owned()));
    entry.insert("ip".to_owned(), Nbt::String(ip.to_owned()));
    // `icon` is intentionally omitted: vanilla only writes it when the server
    // sent a favicon, and an empty TAG_String here makes the entry invalid.
    entry.insert("acceptTextures".to_owned(), Nbt::Byte(1));
    entry.insert("hidden".to_owned(), Nbt::Byte(0));
    Nbt::Compound(entry)
}

fn merge_server(root: &mut Nbt, name: &str, ip: &str) -> Result<(), String> {
    let Nbt::Compound(map) = root else {
        return Err("корень servers.dat не является compound".to_owned());
    };
    if !matches!(map.get("servers"), Some(Nbt::List(_))) {
        map.insert("servers".to_owned(), Nbt::List(Vec::new()));
    }
    let Some(Nbt::List(servers)) = map.get_mut("servers") else {
        return Err("поле servers не является списком".to_owned());
    };

    let mut found = false;
    for server in servers.iter_mut() {
        let Nbt::Compound(fields) = server else {
            continue;
        };
        let matches_ip = matches!(fields.get("ip"), Some(Nbt::String(value)) if value == ip);
        if matches_ip {
            fields.insert("name".to_owned(), Nbt::String(name.to_owned()));
            // Older launcher versions wrote `icon` as an empty TAG_String,
            // which the game cannot read. Drop that invalid value; a real
            // TAG_Byte_Array favicon is preserved.
            if matches!(fields.get("icon"), Some(Nbt::String(_))) {
                fields.remove("icon");
            }
            fields
                .entry("acceptTextures".to_owned())
                .or_insert(Nbt::Byte(1));
            fields.entry("hidden".to_owned()).or_insert(Nbt::Byte(0));
            found = true;
            break;
        }
    }
    if !found {
        servers.push(server_entry(name, ip));
    }
    Ok(())
}

fn encode(root: &Nbt) -> Result<Vec<u8>, String> {
    let mut raw = Vec::new();
    raw.push(10);
    write_string(&mut raw, "")?;
    write_payload(&mut raw, root)?;
    Ok(raw)
}

fn decode(bytes: &[u8]) -> Result<Nbt, String> {
    decode_limited(bytes, MAX_NBT_BYTES)
}

fn decode_limited(bytes: &[u8], limit: u64) -> Result<Nbt, String> {
    // 1.20.2+ writes plain NBT. Keep accepting gzip so old instances created
    // by this launcher (or by an older game) still load their server list.
    let raw = if bytes.starts_with(&[0x1f, 0x8b]) {
        let decoder = GzDecoder::new(bytes);
        let mut limited = decoder.take(limit + 1);
        let mut raw = Vec::new();
        limited
            .read_to_end(&mut raw)
            .map_err(|error| format!("gzip: {error}"))?;
        if raw.len() as u64 > limit {
            return Err("servers.dat: распакованный NBT превышает лимит".to_owned());
        }
        raw
    } else {
        if bytes.len() as u64 > limit {
            return Err("servers.dat: файл NBT превышает лимит".to_owned());
        }
        bytes.to_vec()
    };

    let mut reader = Reader {
        data: &raw,
        position: 0,
    };
    let tag = reader.u8()?;
    if tag != 10 {
        return Err(format!("ожидался TAG_Compound, получен {tag}"));
    }
    let _root_name = reader.string()?;
    read_payload(&mut reader, tag, 0)
}

fn write_payload(out: &mut Vec<u8>, value: &Nbt) -> Result<(), String> {
    match value {
        Nbt::Byte(value) => out.push(*value as u8),
        Nbt::Short(value) => out.extend_from_slice(&value.to_be_bytes()),
        Nbt::Int(value) => out.extend_from_slice(&value.to_be_bytes()),
        Nbt::Long(value) => out.extend_from_slice(&value.to_be_bytes()),
        Nbt::Float(value) => out.extend_from_slice(&value.to_be_bytes()),
        Nbt::Double(value) => out.extend_from_slice(&value.to_be_bytes()),
        Nbt::ByteArray(bytes) => {
            write_len(out, bytes.len())?;
            out.extend_from_slice(bytes);
        }
        Nbt::String(text) => write_string(out, text)?,
        Nbt::List(items) => {
            let element_type = items.first().map(Nbt::type_id).unwrap_or(0);
            out.push(element_type);
            write_len(out, items.len())?;
            for item in items {
                if item.type_id() != element_type {
                    return Err("список NBT содержит разные типы".to_owned());
                }
                write_payload(out, item)?;
            }
        }
        Nbt::Compound(fields) => {
            for (name, field) in fields {
                out.push(field.type_id());
                write_string(out, name)?;
                write_payload(out, field)?;
            }
            out.push(0);
        }
        Nbt::IntArray(values) => {
            write_len(out, values.len())?;
            for value in values {
                out.extend_from_slice(&value.to_be_bytes());
            }
        }
        Nbt::LongArray(values) => {
            write_len(out, values.len())?;
            for value in values {
                out.extend_from_slice(&value.to_be_bytes());
            }
        }
    }
    Ok(())
}

fn write_len(out: &mut Vec<u8>, length: usize) -> Result<(), String> {
    let length = i32::try_from(length).map_err(|_| "слишком длинный массив NBT".to_owned())?;
    out.extend_from_slice(&length.to_be_bytes());
    Ok(())
}

fn write_string(out: &mut Vec<u8>, text: &str) -> Result<(), String> {
    let length = u16::try_from(text.len()).map_err(|_| "слишком длинная строка NBT".to_owned())?;
    out.extend_from_slice(&length.to_be_bytes());
    out.extend_from_slice(text.as_bytes());
    Ok(())
}

struct Reader<'a> {
    data: &'a [u8],
    position: usize,
}

impl Reader<'_> {
    fn take(&mut self, length: usize) -> Result<&[u8], String> {
        let end = self
            .position
            .checked_add(length)
            .filter(|end| *end <= self.data.len())
            .ok_or_else(|| "неожиданный конец NBT".to_owned())?;
        let slice = &self.data[self.position..end];
        self.position = end;
        Ok(slice)
    }

    fn u8(&mut self) -> Result<u8, String> {
        Ok(self.take(1)?[0])
    }

    fn i8(&mut self) -> Result<i8, String> {
        Ok(self.u8()? as i8)
    }

    fn u16(&mut self) -> Result<u16, String> {
        let bytes = self.take(2)?;
        Ok(u16::from_be_bytes([bytes[0], bytes[1]]))
    }

    fn i16(&mut self) -> Result<i16, String> {
        let bytes = self.take(2)?;
        Ok(i16::from_be_bytes([bytes[0], bytes[1]]))
    }

    fn i32(&mut self) -> Result<i32, String> {
        let bytes = self.take(4)?;
        Ok(i32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    fn i64(&mut self) -> Result<i64, String> {
        let bytes = self.take(8)?;
        Ok(i64::from_be_bytes([
            bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
        ]))
    }

    fn f32(&mut self) -> Result<f32, String> {
        Ok(f32::from_bits(self.i32()? as u32))
    }

    fn f64(&mut self) -> Result<f64, String> {
        Ok(f64::from_bits(self.i64()? as u64))
    }

    fn string(&mut self) -> Result<String, String> {
        let length = self.u16()? as usize;
        let bytes = self.take(length)?;
        String::from_utf8(bytes.to_vec())
            .map_err(|error| format!("некорректная строка NBT: {error}"))
    }
}

fn read_payload(reader: &mut Reader<'_>, tag: u8, depth: u8) -> Result<Nbt, String> {
    if depth > MAX_DEPTH {
        return Err("слишком глубокая вложенность NBT".to_owned());
    }
    match tag {
        1 => Ok(Nbt::Byte(reader.i8()?)),
        2 => Ok(Nbt::Short(reader.i16()?)),
        3 => Ok(Nbt::Int(reader.i32()?)),
        4 => Ok(Nbt::Long(reader.i64()?)),
        5 => Ok(Nbt::Float(reader.f32()?)),
        6 => Ok(Nbt::Double(reader.f64()?)),
        7 => {
            let length = reader.i32()?;
            if length < 0 || length as usize > MAX_BYTE_ARRAY {
                return Err("некорректная длина byte array".to_owned());
            }
            Ok(Nbt::ByteArray(reader.take(length as usize)?.to_vec()))
        }
        8 => Ok(Nbt::String(reader.string()?)),
        9 => {
            let element_type = reader.u8()?;
            let length = reader.i32()?;
            if length < 0 || length as usize > MAX_LIST {
                return Err("некорректная длина списка NBT".to_owned());
            }
            let mut items = Vec::with_capacity(length as usize);
            for _ in 0..length {
                items.push(read_payload(reader, element_type, depth + 1)?);
            }
            Ok(Nbt::List(items))
        }
        10 => {
            let mut fields = BTreeMap::new();
            loop {
                let field_type = reader.u8()?;
                if field_type == 0 {
                    break;
                }
                let name = reader.string()?;
                let value = read_payload(reader, field_type, depth + 1)?;
                fields.insert(name, value);
            }
            Ok(Nbt::Compound(fields))
        }
        11 => {
            let length = reader.i32()?;
            if length < 0 || length as usize > MAX_LIST {
                return Err("некорректная длина int array".to_owned());
            }
            let mut values = Vec::with_capacity(length as usize);
            for _ in 0..length {
                values.push(reader.i32()?);
            }
            Ok(Nbt::IntArray(values))
        }
        12 => {
            let length = reader.i32()?;
            if length < 0 || length as usize > MAX_LIST {
                return Err("некорректная длина long array".to_owned());
            }
            let mut values = Vec::with_capacity(length as usize);
            for _ in 0..length {
                values.push(reader.i64()?);
            }
            Ok(Nbt::LongArray(values))
        }
        0 => Err("неожиданный TAG_End".to_owned()),
        other => Err(format!("неизвестный NBT tag {other}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn servers(root: &Nbt) -> &Vec<Nbt> {
        let Nbt::Compound(map) = root else {
            panic!("root compound");
        };
        let Some(Nbt::List(list)) = map.get("servers") else {
            panic!("servers list");
        };
        list
    }

    #[test]
    fn creates_and_updates_server_entry() {
        let directory = tempfile::tempdir().expect("tempdir");
        ensure_server_entry(directory.path(), "Odium", "45.85.60.39:25592").expect("create");
        ensure_server_entry(directory.path(), "Odium", "45.85.60.39:25592").expect("update");

        let bytes = fs::read(directory.path().join("servers.dat")).expect("read");
        let root = decode(&bytes).expect("decode");
        let list = servers(&root);
        assert_eq!(list.len(), 1);
        let Nbt::Compound(fields) = &list[0] else {
            panic!("entry compound");
        };
        assert_eq!(
            fields.get("ip"),
            Some(&Nbt::String("45.85.60.39:25592".to_owned()))
        );
        assert_eq!(fields.get("name"), Some(&Nbt::String("Odium".to_owned())));
        assert_eq!(fields.get("acceptTextures"), Some(&Nbt::Byte(1)));
        assert_eq!(fields.get("hidden"), Some(&Nbt::Byte(0)));
        assert!(
            !fields.contains_key("icon"),
            "an empty icon tag makes Minecraft ignore the entry"
        );
        assert_eq!(
            bytes.first(),
            Some(&10),
            "modern Minecraft expects uncompressed NBT"
        );
    }

    #[test]
    fn reads_legacy_gzipped_servers_dat() {
        use flate2::write::GzEncoder;
        use flate2::Compression;
        use std::io::Write;

        let directory = tempfile::tempdir().expect("tempdir");
        let mut manual = BTreeMap::new();
        manual.insert("name".to_owned(), Nbt::String("Old".to_owned()));
        manual.insert("ip".to_owned(), Nbt::String("1.2.3.4:25565".to_owned()));
        let mut root = BTreeMap::new();
        root.insert("servers".to_owned(), Nbt::List(vec![Nbt::Compound(manual)]));
        let mut raw = Vec::new();
        raw.push(10);
        write_string(&mut raw, "").expect("name");
        write_payload(&mut raw, &Nbt::Compound(root)).expect("payload");
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(&raw).expect("gzip");
        let compressed = encoder.finish().expect("gzip finish");
        fs::write(directory.path().join("servers.dat"), compressed).expect("write");

        ensure_server_entry(directory.path(), "Odium", "45.85.60.39:25592").expect("add");
        let bytes = fs::read(directory.path().join("servers.dat")).expect("read");
        assert_eq!(
            bytes.first(),
            Some(&10),
            "legacy file is upgraded to plain NBT"
        );
        let root = decode(&bytes).expect("decode");
        assert_eq!(servers(&root).len(), 2);
    }

    #[test]
    fn preserves_manual_servers() {
        let directory = tempfile::tempdir().expect("tempdir");
        let mut manual = BTreeMap::new();
        manual.insert("name".to_owned(), Nbt::String("Manual".to_owned()));
        manual.insert("ip".to_owned(), Nbt::String("1.2.3.4:25565".to_owned()));
        let mut root = BTreeMap::new();
        root.insert("servers".to_owned(), Nbt::List(vec![Nbt::Compound(manual)]));
        fs::write(
            directory.path().join("servers.dat"),
            encode(&Nbt::Compound(root)).expect("encode"),
        )
        .expect("write");

        ensure_server_entry(directory.path(), "Odium", "45.85.60.39:25592").expect("add");
        let bytes = fs::read(directory.path().join("servers.dat")).expect("read");
        let root = decode(&bytes).expect("decode");
        assert_eq!(servers(&root).len(), 2);
    }

    #[test]
    fn corrupt_servers_dat_is_backed_up() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("servers.dat");
        fs::write(&path, b"not an nbt file at all").expect("write");

        ensure_server_entry(directory.path(), "Odium", "45.85.60.39:25592").expect("recreate");

        let backups: Vec<_> = fs::read_dir(directory.path())
            .expect("read dir")
            .filter_map(Result::ok)
            .filter(|entry| {
                let name = entry.file_name().to_string_lossy().into_owned();
                name.starts_with("servers.dat.broken-") && name.ends_with(".bak")
            })
            .collect();
        assert_eq!(backups.len(), 1, "the corrupt file must be preserved");
        assert_eq!(
            fs::read(backups[0].path()).expect("read backup"),
            b"not an nbt file at all"
        );
        let root = decode(&fs::read(&path).expect("read new")).expect("decode");
        assert_eq!(servers(&root).len(), 1);
    }

    #[test]
    fn gzip_decompression_is_bounded() {
        use flate2::write::GzEncoder;
        use flate2::Compression;
        use std::io::Write;

        let mut encoder = GzEncoder::new(Vec::new(), Compression::fast());
        encoder.write_all(&vec![0u8; 4096]).expect("gzip");
        let compressed = encoder.finish().expect("finish");
        let error = decode_limited(&compressed, 64).expect_err("must reject");
        assert!(error.contains("лимит"), "unexpected error: {error}");
    }

    #[test]
    fn nbt_roundtrip_keeps_all_scalar_types() {
        let mut fields = BTreeMap::new();
        fields.insert("byte".to_owned(), Nbt::Byte(-5));
        fields.insert("short".to_owned(), Nbt::Short(-1234));
        fields.insert("int".to_owned(), Nbt::Int(42));
        fields.insert("long".to_owned(), Nbt::Long(123456789));
        fields.insert("float".to_owned(), Nbt::Float(1.5));
        fields.insert("double".to_owned(), Nbt::Double(2.5));
        fields.insert("text".to_owned(), Nbt::String("Привет".to_owned()));
        fields.insert("bytes".to_owned(), Nbt::ByteArray(vec![1, 2, 3]));
        fields.insert("ints".to_owned(), Nbt::IntArray(vec![4, 5]));
        fields.insert("longs".to_owned(), Nbt::LongArray(vec![6, 7]));
        let root = Nbt::Compound(fields);

        let bytes = encode(&root).expect("encode");
        let decoded = decode(&bytes).expect("decode");
        assert_eq!(decoded, root);
    }
}
