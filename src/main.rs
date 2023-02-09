#![deny(rust_2018_idioms, unused, unused_crate_dependencies, unused_import_braces, unused_lifetimes, unused_qualifications, warnings)]
#![forbid(unsafe_code)]

use {
    std::{
        collections::BTreeSet,
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
        traits::AsyncCommandOutputExt as _,
    },
    xdg::BaseDirectories,
};

const FILENAME_FORMAT: &str = "%Y-%m-%d_%H-%M-%S.sql";

#[derive(Debug, thiserror::Error)]
enum Error {
    #[error(transparent)] ChronoParse(#[from] chrono::format::ParseError),
    #[error(transparent)] Io(#[from] std::io::Error),
    #[error(transparent)] Wheel(#[from] wheel::Error),
    #[error(transparent)] Xdg(#[from] xdg::BaseDirectoriesError),
    #[error("backup directory not found, create at /usr/local/share/pgbackuproll")]
    BackupDir,
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

fn backup_path() -> Result<PathBuf, Error> {
    BaseDirectories::new()?.find_data_file("pgbackuproll").ok_or(Error::BackupDir)
}

/// Deletes the backup file that's closest to other backup files. In case of a tie, the oldest backup is deleted.
///
/// If only one backup file exists, it's not deleted and `false` is returned.
async fn delete_one(verbose: bool) -> Result<bool, Error> {
    let dir = backup_path()?;
    let mut timestamps = BTreeSet::default();
    pin! {
        let entries = fs::read_dir(&dir);
    }
    while let Some(entry) = entries.try_next().await? {
        timestamps.insert(Utc.datetime_from_str(&entry.file_name().into_string()?, FILENAME_FORMAT)?);
    }
    let timestamp_to_delete = match timestamps.len() {
        0 | 1 => return Ok(false),
        2 => timestamps.into_iter().next().unwrap(),
        _ => *timestamps.iter().tuple_windows().min_by_key(|&(&prev, &curr, &next)| {
            let mut diffs = [curr - prev, next - curr];
            diffs.sort();
            diffs
        }).unwrap().1,
    };
    let filename = timestamp_to_delete.format(FILENAME_FORMAT).to_string();
    if verbose {
        println!("deleting {filename}");
    }
    fs::remove_file(dir.join(filename)).await?;
    Ok(true)
}

async fn make_backup() -> Result<(), Error> {
    Command::new("pg_dumpall")
        .stdout(std::fs::File::create(backup_path()?.join(Utc::now().format(FILENAME_FORMAT).to_string()))?)
        .spawn()? // don't override stdout
        .check("pg_dumpall").await?;
    Ok(())
}

/// `amount` should be a number between 0 and 100. Backups will be deleted until:
///
/// * at least `amount` gibibytes are free _and_ at least `amount` % of the disk is free (returns `Ok(true)`),
/// * only one backup file is remaining (returns `Ok(false)`), or
/// * an error occurs (returns `Err(_)`).
async fn make_room(amount: u64, verbose: bool) -> Result<bool, Error> {
    loop {
        let fs = backup_path()?.ancestors().map(|ancestor| System::new().mount_at(ancestor)).find_map(Result::ok).ok_or(Error::NoMount)?;
        if fs.avail < ByteSize::gib(amount as u64) || (fs.avail.as_u64() as f64 / fs.total.as_u64() as f64) < (amount as f64 / 100.0) {
            if !delete_one(verbose).await? { return Ok(false) }
        } else {
            return Ok(true)
        }
    }
}

#[derive(clap::Parser)]
#[clap(version)]
struct Args {
    #[clap(short, long)]
    verbose: bool,
}

#[wheel::main(debug)]
async fn main(Args { verbose }: Args) -> Result<(), Error> {
    if make_room(10, verbose).await? {
        make_backup().await?;
        make_room(10, verbose).await?;
    }
    Ok(())
}
