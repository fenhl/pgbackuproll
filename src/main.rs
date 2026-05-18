#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

use {
    std::{
        collections::BTreeMap,
        ffi::OsString,
        path::PathBuf,
    },
    bytesize::ByteSize,
    chrono::prelude::*,
    futures::stream::TryStreamExt as _,
    itertools::Itertools as _,
    systemstat::{
        Platform as _,
        System,
    },
    tokio::{
        pin,
        process::Command,
    },
    wheel::{
        fs,
        traits::{
            AsyncCommandOutputExt as _,
            IoResultExt as _,
        },
    },
    xdg::BaseDirectories,
};

const UNCOMPRESSED_FILENAME_FORMAT: &str = "%Y-%m-%d_%H-%M-%S.sql";
const COMPRESSED_FILENAME_FORMAT: &str = "%Y-%m-%d_%H-%M-%S.sql.gz";

#[derive(Debug, thiserror::Error)]
enum Error {
    #[error(transparent)] ChronoParse(#[from] chrono::format::ParseError),
    #[error(transparent)] Io(#[from] std::io::Error),
    #[error(transparent)] Wheel(#[from] wheel::Error),
    #[error("failed to check file system stats at backup directory")]
    NoMount,
    #[error("non-UTF-8 filename")]
    OsString(OsString),
}

impl From<OsString> for Error {
    fn from(value: OsString) -> Self {
        Self::OsString(value)
    }
}

fn backup_path(dir_name: &str) -> Result<PathBuf, Error> {
    let base_dirs = BaseDirectories::new();
    if let Some(path) = base_dirs.find_data_file(dir_name) { return Ok(path) } // prefer existing dir even if it's not in data home
    Ok(base_dirs.create_data_directory(dir_name).at_unknown()?)
}

/// Deletes the backup file that's closest to other backup files. In case of a tie, the oldest backup is deleted.
///
/// If only one backup file exists, it's not deleted and `false` is returned.
async fn delete_one(dir_name: &str, verbose: bool) -> Result<bool, Error> {
    let dir = backup_path(dir_name)?;
    let mut timestamps = BTreeMap::default();
    pin! {
        let entries = fs::read_dir(&dir);
    }
    while let Some(entry) = entries.try_next().await? {
        let filename = entry.file_name().into_string()?;
        timestamps.insert(
            NaiveDateTime::parse_from_str(&filename, UNCOMPRESSED_FILENAME_FORMAT)
                .or_else(|_| NaiveDateTime::parse_from_str(&filename, COMPRESSED_FILENAME_FORMAT))?
                .and_utc(),
            filename,
        );
    }
    let filename = match timestamps.len() {
        0 | 1 => return Ok(false),
        2 => timestamps.into_values().next().unwrap(),
        _ => timestamps.iter().tuple_windows().min_by_key(|&((&prev, _), (&curr, _), (&next, _))| {
            let mut diffs = [curr - prev, next - curr];
            diffs.sort();
            diffs
        }).unwrap().1.1.clone(),
    };
    if verbose {
        println!("deleting {filename}");
    }
    fs::remove_file(dir.join(filename)).await?;
    Ok(true)
}

async fn make_backup(remote: Option<&str>, dir_name: &str) -> Result<(), Error> {
    let mut cmd;
    if let Some(remote) = remote {
        cmd = Command::new("ssh");
        cmd.arg(remote);
        cmd.arg("sudo");
        cmd.arg("-u");
        cmd.arg("postgres");
        cmd.arg("pg_dumpall");
    } else {
        cmd = Command::new("pg_dumpall");
    }
    cmd
        .stdout(std::fs::File::create(backup_path(dir_name)?.join(Utc::now().format(UNCOMPRESSED_FILENAME_FORMAT).to_string()))?)
        .spawn()? // don't override stdout
        .check(if remote.is_some() { "ssh" } else { "pg_dumpall" }).await?;
    Ok(())
}

/// `amount` should be a number between 0 and 100. Backups will be deleted until:
///
/// * at least `amount` gibibytes are free _and_ at least `amount` % of the disk is free (returns `Ok(true)`),
/// * only one backup file is remaining (returns `Ok(false)`), or
/// * an error occurs (returns `Err(_)`).
async fn make_room(dir_name: &str, amount: u64, verbose: bool) -> Result<bool, Error> {
    let dir = backup_path(dir_name)?;
    loop {
        let fs = dir.ancestors().map(|ancestor| System::new().mount_at(ancestor)).find_map(Result::ok).ok_or(Error::NoMount)?;
        if fs.avail < ByteSize::gib(amount as u64) || (fs.avail.as_u64() as f64 / fs.total.as_u64() as f64) < (amount as f64 / 100.0) {
            pin! {
                let entries = fs::read_dir(&dir);
            }
            let mut smallest_uncompressed = None;
            while let Some(entry) = entries.try_next().await? {
                let path = entry.path();
                if path.extension().and_then(|ext| ext.to_str()) != Some("gz") {
                    // this works because the backups are regular files, not directories
                    let size = entry.metadata().await?.len();
                    if smallest_uncompressed.as_ref().map_or(true, |&(_, smallest_size)| size < smallest_size) {
                        smallest_uncompressed = Some((path, size));
                    }
                }
            }
            if let Some((path, size)) = smallest_uncompressed {
                if ByteSize::b(size) < fs.avail {
                    Command::new("gzip")
                        .arg(path)
                        .check("gzip").await?;
                    continue
                }
            }
            // not enough room to compress anything or no uncompressed backups left, delete backups to make room
            if !delete_one(dir_name, verbose).await? { return Ok(false) }
        } else {
            return Ok(true)
        }
    }
}

#[derive(clap::Parser)]
#[clap(version)]
struct Args {
    #[clap(long, default_value = "pgbackuproll")]
    dir_name: String,
    #[clap(long)]
    remote: Option<String>,
    #[clap(short, long)]
    verbose: bool,
}

#[wheel::main]
async fn main(Args { remote, dir_name, verbose }: Args) -> Result<(), Error> {
    if make_room(&dir_name, 10, verbose).await? {
        make_backup(remote.as_deref(), &dir_name).await?;
        make_room(&dir_name, 10, verbose).await?;
    }
    Ok(())
}
