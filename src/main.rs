use clap::{Parser, ValueEnum};
use color_eyre::{
    Result,
    eyre::{Context, eyre},
};
use flate2::{read::ZlibDecoder, write::ZlibEncoder};
use hex::FromHexError;
use ignore::gitignore::Gitignore;
use is_executable::is_executable;
use sha1::{Digest, Sha1};
use std::{
    cmp::Ordering,
    fs::{self, File},
    io::{self, BufRead, BufReader, Read, Write},
    num::ParseIntError,
    path::{Path, PathBuf},
    string::FromUtf8Error,
};
use thiserror::Error;

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
/// Codecrafters git excercise
enum Command {
    /// Initialize a new repository
    Init,
    /// Print the contents of a blob
    CatFile {
        #[arg(value_parser = parse_key)]
        key: Key,
    },
    /// Hash a blob and optionally store it as an object
    HashObject {
        #[arg()]
        file: PathBuf,
        #[arg(short)]
        /// Whether to store the blob as an object
        write: bool,
    },
    /// List the contents of a tree
    LsTree {
        #[arg(value_parser = parse_key)]
        key: Key,
        #[arg(long, visible_alias = "name-status")]
        /// Only print the names of entries
        name_only: bool,
    },
    /// Save the current repo state as a tree object
    WriteTree,
    /// Commit the given tree
    CommitTree {
        #[arg(value_parser = parse_key)]
        /// The key of the tree to commit
        tree: Key,
        #[arg(short, default_value_t)]
        /// Message for the commit
        message: String,
        #[arg(short, value_parser = parse_key, id = "PARENT")]
        /// The parent commit(s)
        parents: Vec<Key>,
    },
}

#[derive(ValueEnum, Debug, Clone, Copy)]
enum ObjectKind {
    Blob,
    Commit,
    Tree,
}

type Key = [u8; 20];

fn parse_key(key: &str) -> Result<Key, FromHexError> {
    let mut out = [0; 20];
    hex::decode_to_slice(key, &mut out)?;
    Ok(out)
}

fn main() -> Result<()> {
    color_eyre::config::HookBuilder::default()
        .display_location_section(cfg!(debug_assertions))
        .display_env_section(cfg!(debug_assertions))
        .install()?;

    match Command::parse() {
        Command::Init => init()?,
        Command::CatFile { key } => cat_file(key)?,
        Command::HashObject { file, write } => hash_object(file, write)?,
        Command::LsTree { key, name_only } => ls_tree(key, name_only)?,
        Command::WriteTree => write_tree()?,
        Command::CommitTree {
            tree,
            parents,
            message,
        } => commit_tree(tree, parents, message)?,
    }

    Ok(())
}

fn init() -> Result<()> {
    fs::create_dir_all(".jit/objects").context("Failed to create objects directory")?;
    fs::create_dir_all(".jit/refs").context("Failed to create refs directory")?;
    fs::write(".jit/HEAD", "ref: refs/heads/main\n").context("Failed to write HEAD file")?;

    println!("Initialized jit repository");

    Ok(())
}

fn cat_file(key: Key) -> Result<()> {
    let Object { kind, size, reader } = get_object(key).context("Failed to get object")?;

    match kind {
        ObjectKind::Blob | ObjectKind::Commit => {
            if io::copy(&mut reader.take(size), &mut io::stdout())? != size {
                return Err(eyre!("Short read"));
            }
        }
        kind => return Err(eyre!("Cannot cat {kind:?}")),
    }

    Ok(())
}

fn hash_object(file: PathBuf, write: bool) -> Result<()> {
    let data = fs::read(file).context("Failed to read file")?;
    let key = if write {
        add_object(ObjectKind::Blob, data).context("Failed to create blob")?
    } else {
        make_object(ObjectKind::Blob, data).0
    };

    println!("{}", hex::encode(key));

    Ok(())
}

fn ls_tree(key: Key, name_only: bool) -> Result<()> {
    let Object {
        kind,
        size,
        mut reader,
    } = get_object(key).context("Failed to get tree")?;

    match kind {
        ObjectKind::Tree => {
            let mut read = 0;
            loop {
                let mut prefix = Vec::new();
                match reader
                    .read_until(0, &mut prefix)
                    .context("Failed to read tree entry header")?
                {
                    0 => {
                        break if read as u64 == size {
                            Ok(())
                        } else {
                            Err(eyre!("Short read"))
                        };
                    }
                    n => {
                        read += n;
                        if read as u64 >= size {
                            break Ok(());
                        }
                    }
                }
                prefix.pop();

                let [mode, name] = &prefix.splitn(2, |b| *b == b' ').collect::<Vec<_>>()[..] else {
                    return Err(eyre!("Invalid tree entry header"));
                };
                let mode = str::from_utf8(mode).context("Invalid tree entry mode")?;

                let mut hash = [0; 20];
                reader
                    .read_exact(&mut hash)
                    .context("Failed to read tree entry hash")?;
                read += 20;

                let mut stdout = io::stdout();

                if name_only {
                    stdout.write_all(name)?;
                } else {
                    let kind = match mode {
                        "40000" => ObjectKind::Tree,
                        "160000" => ObjectKind::Commit,
                        "100644" | "100755" | "1200000" => ObjectKind::Blob,
                        _ => return Err(eyre!("Unknown file mode {mode}")),
                    };
                    write!(
                        stdout,
                        "{mode:0>6} {} {}\t",
                        format!("{kind:?}").to_lowercase(),
                        hex::encode(hash)
                    )?;
                    stdout.write_all(name)?;
                }
                writeln!(stdout)?;
            }
        }
        kind => Err(eyre!("Expected Tree, got {kind:?}")),
    }
}

fn write_tree() -> Result<()> {
    let root = get_root().context("Failed to find repo root")?;

    let key = write_tree_inner(&root, &Gitignore::new(root.join(".gitignore")).0)
        .context("Failed to create tree")?;

    println!("{}", hex::encode(key));
    Ok(())
}

fn write_tree_inner(path: &Path, ignore: &Gitignore) -> io::Result<Key> {
    let mut contents = Vec::new();
    let mut entries = fs::read_dir(path)?.collect::<io::Result<Vec<_>>>()?;
    entries.sort_by(|entry1, entry2| {
        // https://github.com/git/git/blob/d38352cd43ab9745686d697872408bc3249a153f/tree.c#L99
        // Git is weird
        let name1 = entry1.file_name().into_encoded_bytes();
        let name2 = entry2.file_name().into_encoded_bytes();

        let len = name1.len().min(name2.len());

        match name1[..len].cmp(&name2[..len]) {
            Ordering::Equal => {}
            o => return o,
        }

        let mut c1 = name1.get(len).copied().unwrap_or_default();
        let mut c2 = name2.get(len).copied().unwrap_or_default();

        if c1 == 0 && entry1.file_type().is_ok_and(|t| t.is_dir()) {
            c1 = b'/';
        }
        if c2 == 0 && entry2.file_type().is_ok_and(|t| t.is_dir()) {
            c2 = b'/';
        }

        c1.cmp(&c2)
    });

    for entry in entries {
        let filetype = entry.file_type()?;
        let path = entry.path();
        let file_name = entry.file_name();

        if file_name == ".git"
            || file_name == ".jit"
            || file_name == ".jj"
            || ignore.matched(&path, filetype.is_dir()).is_ignore()
        {
            continue;
        }

        let (mode, key) = if filetype.is_symlink() {
            (
                "120000",
                add_object(
                    ObjectKind::Blob,
                    fs::read_link(path)?.into_os_string().into_encoded_bytes(),
                )?,
            )
        } else if filetype.is_dir() {
            ("40000", write_tree_inner(&path, ignore)?)
        } else {
            let mode = if is_executable(&path) {
                "100755"
            } else {
                "100644"
            };
            (mode, add_object(ObjectKind::Blob, fs::read(path)?)?)
        };
        let name = entry.file_name();

        contents.extend(format!("{mode} ").bytes());
        contents.extend(name.into_encoded_bytes());
        contents.push(0);
        contents.extend(key);
    }

    add_object(ObjectKind::Tree, contents)
}

fn commit_tree(tree: Key, parents: Vec<Key>, message: String) -> Result<()> {
    if !object_exists(tree).context("Failed to check if tree exists")? {
        return Err(eyre!("Given tree does not exist"));
    }

    let mut contents = format!("tree {}\n", hex::encode(tree));
    for parent in parents {
        if !object_exists(parent).context("Failed to check if parent exists")? {
            return Err(eyre!("Given parent does not exist"));
        }
        contents += &format!("parent {}\n", hex::encode(parent));
    }
    contents += &format!("\n{message}");

    let key =
        add_object(ObjectKind::Commit, contents.into_bytes()).context("Failed to create commit")?;

    println!("{}", hex::encode(key));

    Ok(())
}

fn object_exists(key: Key) -> io::Result<bool> {
    let key = hex::encode(key);
    Ok(get_root()?
        .join(".jit/objects")
        .join(&key[..2])
        .join(&key[2..])
        .exists())
}

struct Object {
    kind: ObjectKind,
    size: u64,
    reader: BufReader<ZlibDecoder<File>>,
}

#[derive(Debug, Error)]
enum ReadObjectError {
    #[error("IO error")]
    Io(
        #[from]
        #[source]
        io::Error,
    ),
    #[error("Invalid UTF-8 in header")]
    InvalidHeaderText(
        #[from]
        #[source]
        FromUtf8Error,
    ),
    #[error("Invalid size in header")]
    InvalidHeaderSize(
        #[from]
        #[source]
        ParseIntError,
    ),
    #[error("Unknown object kind {0}")]
    UnknownKind(String),
    #[error("Invalid header")]
    InvalidHeader,
}

fn get_object(key: Key) -> Result<Object, ReadObjectError> {
    let key = hex::encode(key);

    let mut reader = BufReader::new(ZlibDecoder::new(File::open(
        get_root()?
            .join(".jit/objects")
            .join(&key[..2])
            .join(&key[2..]),
    )?));

    let mut prefix = Vec::new();
    reader.read_until(0, &mut prefix)?;
    prefix.pop();
    let prefix_text = String::from_utf8(prefix)?;
    let Some((kind, size)) = prefix_text.split_once(' ') else {
        return Err(ReadObjectError::InvalidHeader);
    };
    let size = size.parse()?;

    Ok(Object {
        kind: ObjectKind::from_str(kind, false).map_err(ReadObjectError::UnknownKind)?,
        size,
        reader,
    })
}

fn make_object(kind: ObjectKind, contents: Vec<u8>) -> (Key, Vec<u8>) {
    let mut out = format!(
        "{} {}\0",
        format!("{kind:?}").to_lowercase(),
        contents.len()
    )
    .into_bytes();
    out.extend(contents);

    (Sha1::digest(&out).0, out)
}

fn add_object(kind: ObjectKind, contents: Vec<u8>) -> io::Result<Key> {
    let (key, data) = make_object(kind, contents);
    let key_hex = hex::encode(key);
    let folder = get_root()?.join(".jit/objects").join(&key_hex[..2]);
    fs::create_dir_all(&folder)?;
    ZlibEncoder::new(
        File::create(folder.join(&key_hex[2..]))?,
        Default::default(),
    )
    .write_all(&data)?;
    Ok(key)
}

fn get_root() -> io::Result<PathBuf> {
    let mut cur = ".".to_string();
    loop {
        if fs::read_dir(&cur)?
            .find(|e| e.as_ref().is_ok_and(|e| e.file_name() == ".jit"))
            .is_some()
        {
            break Ok(cur.into());
        } else {
            cur += "/..";
        }
    }
}
