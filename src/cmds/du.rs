use super::{flags::CommandFlags, register_command, Exec, ShellCommand, Flag};
use crate::{eval::Value, scope::Scope, symlnk::SymLink, utils::format_error, utils::format_size};
use std::collections::HashSet;
use std::fs;
use std::io::Error;
use std::path::Path;
use std::sync::Arc;

struct DiskUtilization {
    flags: CommandFlags,
}

impl Exec for DiskUtilization {
    fn cli_flags(&self) -> Box<dyn Iterator<Item = &Flag> + '_> {
        Box::new(self.flags.iter())
    }
    
    fn exec(&self, _name: &str, args: &Vec<String>, scope: &Arc<Scope>) -> Result<Value, String> {
        let mut flags = self.flags.clone();
        let mut paths: Vec<String> = flags.parse(scope, args)?;

        if flags.is_present("help") {
            println!("Usage: du [OPTIONS] [PATH...]");
            println!("Estimate file space usage.");
            println!("\nOptions:");
            println!("{}", flags.help());
            println!("Symbolic links are skipped except at top level (i.e. the paths specified in the command)");
            println!("unless -P / --no-dereference option is present -- in which case no symlinks are resolved.");
            return Ok(Value::success());
        }

        if paths.is_empty() {
            paths.push(".".to_string());
        }

        let max_depth = flags
            .value("max-depth")
            .map(|s| {
                s.parse::<usize>()
                    .map_err(|e| format_error(&scope, s, args, e))
            })
            .transpose()?; // Propagate error if present

        let opts = Options {
            all: flags.is_present("all"),
            apparent: flags.is_present("apparent"),
            summarize: flags.is_present("summarize"),
            human: flags.is_present("human-readable"),
            block_size: 1024,
            max_depth,
            unique_ids: flags.is_present("unique"),
        };

        let follow = flags.is_present("follow-links");

        for p in &paths {
            // Set the argument index in case there's an error
            scope.err_path_arg(p, args);

            let mut file_ids: HashSet<(u64, u64)> = HashSet::new();

            let path = Path::new(p)
                .resolve(follow)
                .map_err(|e| format_error(&scope, p, args, e))?;

            let size = du_size(&path, &opts, scope, 0, &mut file_ids)?;

            if opts.summarize {
                print_size(&path, size, &opts)?;
            }
        }
        Ok(Value::success())
    }
}

impl DiskUtilization {
    fn new() -> Self {
        let mut flags = CommandFlags::with_follow_links();
        flags.add_flag(
            'a',
            "all",
            "Include counts for all files, not just directories",
        );
        flags.add_flag('s', "summarize", "Display only a total for each argument");
        flags.add_flag(
            'h',
            "human-readable",
            "Print sizes in human readable format (e.g., 1.1K, 234M, 2.7G)",
        );
        flags.add_value(
            'd',
            "max-depth",
            "depth",
            "Print the total for a directory only if below the specified depth",
        );
        flags.add(
            None,
            "apparent",
            None,
            "Print apparent sizes, rather than disk usage",
        );

        flags.add_flag('u', "unique", "Avoid double-counting hard links");

        Self { flags }
    }
}

struct Options {
    all: bool,
    apparent: bool, // show apparent size on disk
    human: bool,    // show size in human-readble format
    summarize: bool,
    block_size: u64,
    max_depth: Option<usize>,
    unique_ids: bool, // use unique ids to avoid double-counting
}

fn du_size(
    path: &Path,
    opts: &Options,
    scope: &Scope,
    depth: usize,
    file_ids: &mut HashSet<(u64, u64)>,
) -> Result<u64, String> {
    // Skip symbolic links
    if path.is_symlink() {
        return Ok(0);
    }
    let mut size = estimate_disk_size(&opts, file_ids, path)
        .map_err(|e| format!("{}: {}", scope.err_path(path), e))?;

    if path.is_dir() {
        match fs::read_dir(path) {
            Err(e) => {
                my_warning!(scope, "{}: {}", scope.err_path(path), e);
            }
            Ok(dir) => {
                for entry in dir {
                    if Scope::is_interrupted() {
                        return Ok(size);
                    }

                    let entry = &entry.map_err(|e| format!("{}: {}", scope.err_path(path), e))?;
                    size += du_size(&entry.path(), &opts, scope, depth + 1, file_ids)?;
                }
            }
        }
    }

    if !opts.summarize && depth <= opts.max_depth.unwrap_or(usize::MAX) {
        if opts.all || path.is_dir() {
            print_size(path, size, opts)?;
        }
    }

    Ok(size)
}

fn estimate_disk_size(
    opts: &Options,
    file_ids: &mut HashSet<(u64, u64)>,
    path: &Path,
) -> Result<u64, Error> {
    #[cfg(unix)]
    {
        unix_disk_size(opts, file_ids, path)
    }
    #[cfg(windows)]
    {
        let mut blk_sz = std::collections::HashMap::new();
        win::disk_size(&mut blk_sz, opts, file_ids, path)
    }
}

#[cfg(unix)]
fn unix_disk_size(
    opts: &Options,
    file_ids: &mut HashSet<(u64, u64)>,
    path: &Path,
) -> Result<u64, Error> {
    use std::os::unix::fs::MetadataExt;

    let metadata = fs::metadata(path)?;

    if opts.unique_ids {
        // Avoid double-counting hard links
        let inode = (metadata.dev(), metadata.ino());
        if !file_ids.insert(inode) {
            return Ok(0);
        }
    }

    if opts.apparent {
        Ok(metadata.len())
    } else {
        // https://doc.rust-lang.org/std/os/unix/fs/trait.MetadataExt.html#tymethod.blocks
        // num_blocks is in 512 units, NOT metadata.blksize()
        let num_blocks = metadata.blocks();

        Ok(num_blocks * 512)
    }
}

#[cfg(windows)]
mod win {
    use super::*;
    use crate::utils::win::root_path;
    use std::collections::{HashMap, HashSet};
    use std::ffi::OsStr;
    use std::fs::{self, OpenOptions};
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
    use std::os::windows::io::AsRawHandle;
    use std::path::PathBuf;
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::Storage::FileSystem::{
        GetDiskFreeSpaceW, GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
        FILE_FLAG_BACKUP_SEMANTICS,
    };

    #[inline]
    pub fn disk_size(
        blk_sz: &mut HashMap<PathBuf, u64>,
        opts: &Options,
        file_ids: &mut HashSet<(u64, u64)>,
        path: &Path,
    ) -> Result<u64, Error> {
        let metadata = fs::metadata(path)?;

        if opts.unique_ids {
            let id: (u64, u64) = unique_file_id(path)?;

            // Check if we've seen this file before, avoid double-counting hard links
            if !file_ids.insert(id) {
                return Ok(0);
            }
        }

        if opts.apparent {
            Ok(metadata.len())
        } else {
            let block_size = block_size(blk_sz, &root_path(&path))?;
            Ok(((metadata.file_size() + block_size - 1) / block_size) * block_size)
        }
    }

    fn block_size(blk_sz: &mut HashMap<PathBuf, u64>, root_path: &Path) -> Result<u64, Error> {
        if let Some(sz) = blk_sz.get(root_path) {
            return Ok(*sz);
        }

        let path_wide: Vec<u16> = OsStr::new(root_path)
            .encode_wide()
            .chain(Some(0).into_iter())
            .collect();

        let mut sectors_per_cluster = 0;
        let mut bytes_per_sector = 0;
        let mut _free_clusters = 0;
        let mut _total_clusters = 0;

        unsafe {
            GetDiskFreeSpaceW(
                PCWSTR(path_wide.as_ptr()),
                Some(&mut sectors_per_cluster),
                Some(&mut bytes_per_sector),
                Some(&mut _free_clusters),
                Some(&mut _total_clusters),
            )?;
        }

        // Calculate block size
        let block_size = sectors_per_cluster as u64 * bytes_per_sector as u64;

        // Cache it
        blk_sz.insert(root_path.to_path_buf(), block_size);

        Ok(block_size)
    }

    /// Build a unique id from the volume serial number and the file index.
    /// Used with a hash set to avoid double counting of links.
    fn unique_file_id(path: &Path) -> Result<(u64, u64), Error> {
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS.0)
            .open(path)?;

        let handle = HANDLE(file.as_raw_handle());
        let mut file_info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };

        unsafe { GetFileInformationByHandle(handle, &mut file_info) }?;

        let volume_serial_number = file_info.dwVolumeSerialNumber as u64;
        let file_index =
            ((file_info.nFileIndexHigh as u64) << 32) | (file_info.nFileIndexLow as u64);

        Ok((volume_serial_number, file_index))
    }
}

fn print_size(path: &Path, size: u64, opts: &Options) -> Result<(), String> {
    my_println!(
        "{}\t{}",
        format_size(size, opts.block_size, opts.human),
        path.display()
    )
}

#[ctor::ctor]
fn register() {
    register_command(ShellCommand {
        name: "du".to_string(),
        inner: Arc::new(DiskUtilization::new()),
    });
}
