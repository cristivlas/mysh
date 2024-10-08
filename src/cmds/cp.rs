use super::{flags::CommandFlags, register_command, Exec, Flag, ShellCommand};
use crate::{
    eval::Value,
    prompt::{confirm, Answer},
    scope::Scope,
    symlnk::SymLink,
    utils::format_error,
};
use filetime::FileTime;
use indicatif::{ProgressBar, ProgressDrawTarget, ProgressStyle};
use std::collections::{BTreeMap, HashSet};
use std::fs::{self, File};
use std::io::{self, ErrorKind::Other, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

#[derive(Debug, PartialEq)]
enum Action {
    Copy,
    CreateDir,
    Link,
}

#[derive(Debug)]
struct WorkItem<'a> {
    top: &'a str, // Top source path as given in the command line
    act: Action,
    src: PathBuf,
}

impl<'a> WorkItem<'a> {
    fn new(top: &'a str, act: Action, src: PathBuf) -> Self {
        Self { top, act, src }
    }
}

trait WrapErr<T> {
    fn wrap_err(self, fc: &FileCopier, top: &str, path: &Path) -> T;
    fn wrap_err_with_msg(self, fc: &FileCopier, top: &str, path: &Path, msg: Option<&str>) -> T;
}

impl<T> WrapErr<Result<T, io::Error>> for Result<T, io::Error> {
    /// `top` is the path name as specified in the command line,
    /// `path` is the path the error is related to -- in most cases the resolved, or
    /// canonicalized version of `top`. `top` is looked up in the original command
    /// args, so that when the error is reported to the user, the error location
    /// that is shown is as close as possible to the argument that caused the error.
    fn wrap_err_with_msg(
        self,
        fc: &FileCopier,
        top: &str,
        path: &Path,
        msg: Option<&str>,
    ) -> Result<T, io::Error> {
        match self {
            Ok(v) => Ok(v),
            Err(e) => {
                // Map the top source path to its position in the command line arguments.
                let pos = fc.args.iter().position(|a| a == top).unwrap_or(0);
                // Store the position of the argument that originated the error.
                fc.scope.set_err_arg(pos);

                // Format error message to include path.
                let message = if let Some(msg) = msg {
                    format!("{} {}: {}", msg, fc.scope.err_path(path), e)
                } else {
                    format!("{}: {}", fc.scope.err_path(path), e)
                };
                Err(io::Error::new(io::ErrorKind::Other, message))
            }
        }
    }

    fn wrap_err(self, fc: &FileCopier, top: &str, path: &Path) -> Result<T, io::Error> {
        self.wrap_err_with_msg(fc, top, path, None)
    }
}

struct FileCopier<'a> {
    dest: PathBuf, // Destination
    debug: bool,
    ignore_links: bool,      // Skip symbolic links
    confirm_overwrite: bool, // Ask for overwrite confirmation?
    no_hidden: bool,         // Ignore entries starting with '.'
    preserve_metadata: bool,
    progress: Option<ProgressBar>,
    recursive: bool,
    scope: &'a Arc<Scope>,
    srcs: &'a [String], // Source paths from the command line
    args: &'a [String], // All the original command line args
    visited: HashSet<PathBuf>,
    work: BTreeMap<PathBuf, WorkItem<'a>>, // Use BTreeMap to keep work items sorted
    total_size: u64,                       // Total size of files to be copied
}

impl<'a> FileCopier<'a> {
    fn new(
        paths: &'a [String],
        flags: &CommandFlags,
        scope: &'a Arc<Scope>,
        args: &'a [String],
    ) -> Self {
        Self {
            dest: PathBuf::from(paths.last().unwrap()),
            // Command line flags
            debug: flags.is_present("debug"),
            ignore_links: flags.is_present("no-dereference"),
            confirm_overwrite: flags.is_present("interactive"),
            no_hidden: flags.is_present("no-hidden"),
            preserve_metadata: !flags.is_present("no-preserve"),
            recursive: flags.is_present("recursive"),
            // Progress indicator
            progress: if flags.is_present("progress") {
                let template = if scope.use_colors(&std::io::stdout()) {
                    "{spinner:.green} [{elapsed_precise}] {msg:>30.cyan.bright} {total_bytes}"
                } else {
                    "{spinner} [{elapsed_precise}] {msg:>30} {total_bytes}"
                };
                let pb = ProgressBar::with_draw_target(None, ProgressDrawTarget::stdout());
                pb.set_style(ProgressStyle::default_spinner().template(template).unwrap());
                pb.enable_steady_tick(Duration::from_millis(100));
                Some(pb)
            } else {
                None
            },
            scope,
            srcs: &paths[..paths.len() - 1],
            args,
            visited: HashSet::new(),
            work: BTreeMap::new(),
            total_size: 0,
        }
    }

    fn resolve_dest(&self, _top: &'a str, parent: &Path, src: &Path) -> io::Result<PathBuf> {
        if self.dest.is_dir() {
            if src == parent {
                Ok(self.dest.join(src.file_name().unwrap()))
            } else {
                match src.strip_prefix(parent) {
                    Ok(path) => Ok(self.dest.join(path)),
                    Err(_) => Ok(src.to_path_buf()), // absolute src path / link?
                }
            }
        } else {
            Ok(self.dest.to_path_buf())
        }
    }

    /// Add a work item for creating a directory.
    fn add_create_dir(&mut self, top: &'a str, parent: &Path, src: &Path) -> io::Result<()> {
        let actual_dest = self.resolve_dest(top, parent, src)?;
        let work_item = WorkItem::new(top, Action::CreateDir, src.to_path_buf());
        self.work.insert(actual_dest, work_item);

        Ok(())
    }

    fn check_dir_dest(&mut self) -> io::Result<()> {
        if self.dest.exists() {
            // Copying multiple files over a regular file?
            if !self.dest.is_dir() && !self.work.is_empty() {
                return Err(self.dest_error("Copying multiple sources into single destination"));
            }
        } else if !self.work.is_empty() {
            return Err(self.dest_error("Copying multiple sources to non-existing directory"));
        }
        Ok(())
    }

    /// Add a work item for copying the contents of a regular file (i.e. not symlink, not dir).
    fn add_copy(&mut self, top: &'a str, parent: &Path, src: &Path) -> io::Result<()> {
        assert!(!src.is_dir());

        self.check_dir_dest()?;
        let dest = self.resolve_dest(top, parent, src)?;

        if dest.exists() && dest.canonicalize()? == src.canonicalize()? {
            return Err(self.error(top, &dest, "Source and destination are the same"));
        }

        let work_item = WorkItem::new(top, Action::Copy, src.to_path_buf());
        self.work.insert(dest, work_item);

        Ok(())
    }

    /// Add a work item for making a symbolic link.
    fn add_link(&mut self, top: &'a str, parent: &Path, src: &Path) -> io::Result<()> {
        // Resolve the link target
        let target = src.dereference().wrap_err_with_msg(
            &self,
            top,
            src,
            Some("Could not get link target"),
        )?;

        let target_dest = self.resolve_dest(top, parent, &target)?;
        let dest = self.resolve_dest(top, parent, src)?;

        // if self.debug {
        //     eprintln!(
        //         "Link: {} -> {}:\n\t{} -> {}",
        //         src.display(),
        //         target.display(),
        //         dest.display(),
        //         target_dest.display()
        //     );
        // }

        // Store work item by dest, target_dest cannot be used as unique key.
        let work_item = WorkItem::new(top, Action::Link, target_dest.to_path_buf());
        self.work.insert(dest, work_item);

        Ok(())
    }

    /// Collect info about one path and its size, recurse if directory.
    /// Return Ok(false) if interrupted by Ctrl+C.
    /// Update progress indicator in verbose mode.
    fn collect_path_info(&mut self, top: &'a str, parent: &Path, path: &Path) -> io::Result<bool> {
        // Check for Ctrl+C
        if Scope::is_interrupted() {
            return Ok(false);
        }
        if self.ignore_links && path.is_symlink() {
            return Ok(true);
        }
        // Ignore files and dirs starting with '.'? Useful for
        // copying project directories without .git, .vscode, etc.
        if self.no_hidden
            && path
                .file_name()
                .is_some_and(|f| f.to_string_lossy().starts_with("."))
        {
            if self.debug {
                eprintln!("{}: skip hidden", path.display());
            }
            return Ok(true);
        }

        if path.is_symlink() {
            assert!(!self.ignore_links);
            self.add_link(top, parent, path)?;
        } else if path.is_dir() {
            if !self.recursive {
                my_warning!(self.scope, "{}: Is a directory", self.scope.err_path(path));
                return Ok(true);
            }
            // Bail if the path has been seen before
            let canonical = path.canonicalize().wrap_err(&self, top, path)?;
            if !self.visited.insert(canonical) {
                if self.debug {
                    eprintln!("{}: already seen", path.display());
                }
                return Ok(true);
            }

            // Replicate dirs from the source into the destination, even if empty.
            self.add_create_dir(top, parent, path)?;

            // Collect info recursively
            for entry in fs::read_dir(path).wrap_err(&self, top, path)? {
                let entry = entry.wrap_err(&self, top, path)?;
                let child = entry.path();

                if !self.collect_path_info(top, parent, &child)? {
                    return Ok(false); // User interrupted
                }
            }
        } else {
            let size = fs::metadata(&path).wrap_err(&self, top, path)?.len();

            self.total_size += size;
            self.add_copy(top, parent, path)?;

            // Update progress indicator, if set up (-v flag specified)
            if let Some(pb) = &self.progress {
                pb.set_message(format!("{}", Self::truncate_path(path)));
                pb.set_position(self.total_size);
            }
        }
        Ok(true)
    }

    /// Collect the list of files to copy and their sizes.
    /// Create work items. Return Ok(false) on Ctrl+C.
    fn collect_src_info(&mut self) -> io::Result<bool> {
        assert!(!self.srcs.is_empty());

        // Always resolve symbolic links in the destination.
        self.dest = self
            .dest
            .dereference()
            .wrap_err(
                &self,
                self.dest.as_os_str().to_str().unwrap_or(""),
                &self.dest,
            )?
            .into();

        for src in self.srcs {
            // Always resolve symbolic links for the source paths given in the command line.
            let path = Path::new(src).dereference()?;
            let parent = path.parent().unwrap_or(&path);

            if self.debug {
                eprintln!("Collect: {} (resolved: {})", src, path.display());
            }

            // Collect source info for the top paths, checking for cancellation.
            if !self.collect_path_info(src, &parent, &path)? {
                if let Some(pb) = self.progress.as_mut() {
                    pb.abandon_with_message("Aborted");
                }
                return Ok(false);
            }
        }
        if let Some(pb) = self.progress.as_mut() {
            pb.finish_with_message("Collected source file(s)");
        }
        Ok(true)
    }

    fn dest_error(&self, msg: &str) -> io::Error {
        let dest = self
            .dest
            .file_name()
            .unwrap_or(self.dest.as_os_str())
            .to_str()
            .unwrap_or_default();

        io::Error::new(
            io::ErrorKind::Other,
            format_error(&self.scope, dest, self.args, msg),
        )
    }

    /// Construct io::Error with given path and message.
    fn error(&self, top: &str, path: &Path, msg: &str) -> io::Error {
        // Map the top source path to its position in the command line arguments.
        let pos = self.args.iter().position(|a| a == top).unwrap_or(0);
        // Store the position of the argument that originated the error.
        self.scope.set_err_arg(pos);

        io::Error::new(Other, format!("{}: {}", self.scope.err_path(path), msg))
    }

    /// Truncate path for display in progress indicator.
    fn truncate_path(path: &Path) -> String {
        const MAX_LENGTH: usize = 30;
        let filename = path.to_str().unwrap_or("");
        if filename.len() <= MAX_LENGTH {
            filename.to_uppercase()
        } else {
            let start_index = filename.len() - (MAX_LENGTH - 3);
            format!("...{}", &filename[start_index..])
        }
    }

    fn reset_progress_indicator(&mut self, size: u64) {
        let template = if self.scope.use_colors(&std::io::stdout()) {
            "{spinner:.green} [{elapsed_precise}] {msg:>30.cyan.bright} [{bar:45.green/}] {bytes}/{total_bytes} ({eta})"
        } else {
            "{spinner:} [{elapsed_precise}] {msg:>30} [{bar:45}] {bytes}/{total_bytes} ({eta})"
        };

        let pb = ProgressBar::with_draw_target(Some(size), ProgressDrawTarget::stdout());
        pb.set_style(
            ProgressStyle::default_bar()
                .template(&template)
                .unwrap()
                .progress_chars("=> "),
        );

        self.progress = Some(pb);
    }

    /// Collect all source files, their total size, re-create all dirs in the
    /// source(s) and copy the files; symlinks require Admin privilege on Windows.
    fn copy(&mut self) -> io::Result<()> {
        if !self.collect_src_info()? {
            return Ok(());
        }

        if self.progress.is_some() {
            self.reset_progress_indicator(self.total_size);
        }

        self.do_work()
    }

    fn do_work_actions(
        &mut self,
        actions: &[Action],
        work: &BTreeMap<PathBuf, WorkItem<'a>>,
    ) -> io::Result<bool> {
        for (dest, w) in work {
            if let Some(pb) = self.progress.as_mut() {
                pb.set_message(Self::truncate_path(&w.src));
            }

            if !actions.contains(&w.act) {
                continue;
            }

            if !self.do_work_item(work.len(), &dest, &w)? {
                return Ok(false);
            }
        }

        Ok(true)
    }

    fn do_work(&mut self) -> io::Result<()> {
        let work = std::mem::take(&mut self.work);

        if self.debug {
            dbg!(&work); // Dump the work plan
        }

        // 1st pass: create dirs and copy files
        let mut done = self.do_work_actions(&[Action::CreateDir, Action::Copy], &work)?;
        if done {
            // 2nd pass: symlinks
            done = self.do_work_actions(&[Action::Link], &work).map_err(|e| {
                io::Error::new(
                    Other,
                    format!("{}. Try again with -P, --no-dereference, or sudo", e),
                )
            })?;
        }

        if let Some(pb) = self.progress.as_mut() {
            if done {
                pb.finish_with_message("Ok");
            } else {
                pb.abandon_with_message("Aborted");
            }
            println!();
        }

        Ok(())
    }

    fn do_work_item(&mut self, count: usize, dest: &PathBuf, w: &WorkItem) -> io::Result<bool> {
        match w.act {
            Action::Copy => {
                if self.debug {
                    eprintln!("COPY: {} -> {}", w.src.display(), dest.display());
                }
                assert!(!dest.is_dir());

                if self.confirm_overwrite && dest.exists() {
                    match confirm(
                        format!("Overwrite {}", dest.display()),
                        self.scope,
                        count > 1,
                    )? {
                        Answer::Yes => {}
                        Answer::No => return Ok(true), // Continue
                        Answer::All => {
                            self.confirm_overwrite = false;
                        }
                        Answer::Quit => return Ok(false), // Cancel all
                    }
                }
                if !self.copy_file(w.top, &w.src, dest)? {
                    return Ok(false);
                }
            }
            Action::CreateDir => {
                if self.debug {
                    eprintln!("CREATE: {} ({})", dest.display(), w.src.display());
                }
                if !dest.exists() {
                    fs::create_dir(dest).wrap_err(&self, w.top, &w.src)?;
                }
            }
            Action::Link => {
                if self.debug {
                    eprintln!("LINK: {} -> {}", dest.display(), w.src.display());
                }
                self.symlink(&w.src, &dest).wrap_err(&self, w.top, &w.src)?;
            }
        }
        Ok(true)
    }

    /// Copy the contents of a regular file.
    /// Update progress indicator in verbose mode.
    fn copy_file(&mut self, top: &str, src: &Path, dest: &PathBuf) -> io::Result<bool> {
        #[cfg(unix)]
        self.handle_unix_special_file(src, dest)?;

        let mut src_file = File::open(src).wrap_err(&self, top, src)?;
        let mut dst_file = File::create(&dest).wrap_err(&self, top, dest)?;

        let mut buffer = [0; 8192]; // TODO: allow user to specify buffer size?
        loop {
            if Scope::is_interrupted() {
                return Ok(false);
            }
            let n = src_file.read(&mut buffer).wrap_err(&self, top, src)?;
            if n == 0 {
                break;
            }
            dst_file
                .write_all(&buffer[..n])
                .wrap_err(&self, top, dest)?;

            if let Some(pb) = self.progress.as_mut() {
                pb.inc(n as u64);
            }
        }

        if self.preserve_metadata {
            self.preserve_metadata(top, src, dest)?;
        }

        Ok(true)
    }

    #[cfg(unix)]
    fn handle_unix_special_file(&self, src: &Path, dest: &PathBuf) -> io::Result<()> {
        use std::os::unix::fs::FileTypeExt;
        let file_type = fs::symlink_metadata(src)?.file_type();

        if file_type.is_fifo() {
            // Recreate the FIFO rather than copying contents
            nix::unistd::mkfifo(dest, nix::sys::stat::Mode::S_IRWXU)?;
        } else if file_type.is_socket() {
            my_warning!(self.scope, "Skipping socket: {}", self.scope.err_path(src));
        } else if file_type.is_block_device() || file_type.is_char_device() {
            my_warning!(
                self.scope,
                "Skipping device file: {}",
                self.scope.err_path(src)
            );
        }
        Ok(())
    }

    fn preserve_metadata(&self, top: &str, src: &Path, dest: &PathBuf) -> io::Result<()> {
        // Get metadata of source file
        let metadata = fs::metadata(src).wrap_err_with_msg(
            &self,
            top,
            src,
            Some("Could not read metadata"),
        )?;

        // Set timestamps on destination file
        filetime::set_file_times(
            dest,
            FileTime::from_last_access_time(&metadata),
            FileTime::from_last_modification_time(&metadata),
        )
        .wrap_err_with_msg(&self, top, dest, Some("Could not set file time"))?;

        // Set permissions on the destination
        fs::set_permissions(dest, metadata.permissions()).wrap_err(&self, top, dest)?;

        #[cfg(unix)]
        {
            use nix::unistd::{chown, Gid, Uid};
            use std::os::unix::fs::MetadataExt;

            let uid = metadata.uid();
            let gid = metadata.gid();

            chown(dest, Some(Uid::from_raw(uid)), Some(Gid::from_raw(gid)))?;
        }

        Ok(())
    }

    fn symlink(&self, src: &Path, dst: &Path) -> io::Result<()> {
        #[cfg(unix)]
        {
            use std::os::unix::fs as unix_fs;

            unix_fs::symlink(src, dst)
        }

        #[cfg(windows)]
        {
            use std::os::windows::fs as windows_fs;

            if src.is_dir() {
                windows_fs::symlink_dir(src, dst)
            } else {
                windows_fs::symlink_file(src, dst)
            }
        }
    }
}

struct Cp {
    flags: CommandFlags,
}

impl Cp {
    fn new() -> Self {
        let mut flags = CommandFlags::new();
        flags.add_flag('?', "help", "Display this help message");
        flags.add_flag('d', "debug", "Show debugging details");
        flags.add_flag('v', "progress", "Show progress bar");
        flags.add_flag('r', "recursive", "Copy directories recursively");
        flags.add_flag_enabled('i', "interactive", "Prompt to overwrite");
        flags.add_alias(Some('f'), "force", "no-interactive");
        flags.add_flag('P', "no-dereference", "Ignore symbolic links in SOURCE");
        flags.add(None, "no-hidden", None, "Ignore hidden files");
        flags.add(
            None,
            "no-preserve",
            None,
            "Do not preserve permissions and time stamps",
        );
        Cp { flags }
    }
}

impl Exec for Cp {
    fn cli_flags(&self) -> Box<dyn Iterator<Item = &Flag> + '_> {
        Box::new(self.flags.iter())
    }

    fn exec(&self, _name: &str, args: &Vec<String>, scope: &Arc<Scope>) -> Result<Value, String> {
        let mut flags = self.flags.clone();
        let paths = flags.parse(scope, args)?;

        if flags.is_present("help") {
            println!("Usage: cp [OPTIONS] SOURCE... DEST");
            println!("Copy SOURCE(s) to DESTination.");
            println!("\nOptions:");
            print!("{}", flags.help());
            return Ok(Value::success());
        }

        if paths.is_empty() {
            return Err("Missing source and destination".to_string());
        }
        if paths.len() < 2 {
            return Err("Missing destination".to_string());
        }

        let mut copier = FileCopier::new(&paths, &flags, scope, &args);
        copier.copy().map_err(|e| e.to_string())?;

        Ok(Value::success())
    }
}

#[ctor::ctor]
fn register() {
    register_command(ShellCommand {
        name: "cp".to_string(),
        inner: Arc::new(Cp::new()),
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::TempDir;

    fn create_temp_file(dir: &Path, name: &str, content: &str) -> io::Result<PathBuf> {
        let file_path = dir.join(name);
        let mut file = File::create(&file_path)?;
        write!(file, "{}", content)?;
        Ok(file_path)
    }

    #[test]
    fn test_new_file_copier() {
        let scope = Scope::new();
        let paths = vec!["src1".to_string(), "src2".to_string(), "dest".to_string()];
        let flags = CommandFlags::new();
        let args = vec![
            "cp".to_string(),
            "src1".to_string(),
            "src2".to_string(),
            "dest".to_string(),
        ];

        let copier = FileCopier::new(&paths, &flags, &scope, &args);

        assert_eq!(copier.dest, PathBuf::from("dest"));
        assert_eq!(copier.srcs, &["src1", "src2"]);
        assert_eq!(copier.args, &args);
    }

    #[test]
    fn test_add_copy() -> io::Result<()> {
        let temp_dir = TempDir::new()?;
        let src_file = create_temp_file(temp_dir.path(), "source.txt", "Hello, world!")?;

        let scope = Scope::new();
        let paths = vec![src_file.to_str().unwrap().to_string(), "dest".to_string()];
        let flags = CommandFlags::new();
        let args = vec![
            "cp".to_string(),
            src_file.to_str().unwrap().to_string(),
            "dest".to_string(),
        ];

        let mut copier = FileCopier::new(&paths, &flags, &scope, &args);

        copier.add_copy(src_file.to_str().unwrap(), temp_dir.path(), &src_file)?;

        assert_eq!(copier.work.len(), 1);
        assert!(copier.work.values().next().unwrap().act == Action::Copy);

        Ok(())
    }

    #[test]
    fn test_add_create_dir() -> io::Result<()> {
        let temp_dir = TempDir::new()?;
        let src_dir = temp_dir.path().join("source_dir");
        fs::create_dir(&src_dir)?;

        let scope = Scope::new();
        let paths = vec![src_dir.to_str().unwrap().to_string(), "dest".to_string()];
        let flags = CommandFlags::new();
        let args = vec![
            "cp".to_string(),
            src_dir.to_str().unwrap().to_string(),
            "dest".to_string(),
        ];

        let mut copier = FileCopier::new(&paths, &flags, &scope, &args);

        copier.add_create_dir(src_dir.to_str().unwrap(), temp_dir.path(), &src_dir)?;

        assert_eq!(copier.work.len(), 1);
        assert!(copier.work.values().next().unwrap().act == Action::CreateDir);

        Ok(())
    }

    #[test]
    fn test_collect_path_info() -> io::Result<()> {
        let temp_dir = TempDir::new()?;
        let src_file = create_temp_file(temp_dir.path(), "source.txt", "Hello, world!")?;

        let scope = Scope::new();
        let paths = vec![src_file.to_str().unwrap().to_string(), "dest".to_string()];
        let flags = CommandFlags::new();
        let args = vec![
            "cp".to_string(),
            src_file.to_str().unwrap().to_string(),
            "dest".to_string(),
        ];

        let mut copier = FileCopier::new(&paths, &flags, &scope, &args);

        let result =
            copier.collect_path_info(src_file.to_str().unwrap(), temp_dir.path(), &src_file)?;

        assert!(result);
        assert_eq!(copier.work.len(), 1);
        assert_eq!(copier.total_size, 13); // "Hello, world!" is 13 bytes

        Ok(())
    }

    #[test]
    fn test_copy_file() -> io::Result<()> {
        let temp_dir = TempDir::new()?;
        let src_file = create_temp_file(temp_dir.path(), "source.txt", "Hello, world!")?;
        let dest_file = temp_dir.path().join("dest.txt");

        let scope = Scope::new();
        let paths = vec![
            src_file.to_str().unwrap().to_string(),
            dest_file.to_str().unwrap().to_string(),
        ];
        let flags = CommandFlags::new();
        let args = vec![
            "cp".to_string(),
            src_file.to_str().unwrap().to_string(),
            dest_file.to_str().unwrap().to_string(),
        ];

        let mut copier = FileCopier::new(&paths, &flags, &scope, &args);

        let result = copier.copy_file(src_file.to_str().unwrap(), &src_file, &dest_file)?;

        assert!(result);
        assert!(dest_file.exists());

        let contents = fs::read_to_string(dest_file)?;
        assert_eq!(contents, "Hello, world!");

        Ok(())
    }

    #[test]
    fn test_error_handling() {
        let temp_dir = TempDir::new().unwrap();
        let nonexistent = temp_dir.path().join("nonexistent");
        let dest = temp_dir.path().join("dest");

        let scope = Scope::new();
        let paths = vec![
            nonexistent.to_str().unwrap().to_string(),
            dest.to_str().unwrap().to_string(),
        ];
        let flags = CommandFlags::new();
        let args = vec![
            "cp".to_string(),
            nonexistent.to_str().unwrap().to_string(),
            dest.to_str().unwrap().to_string(),
        ];

        let mut copier = FileCopier::new(&paths, &flags, &scope, &args);

        let result = copier.collect_src_info();
        assert!(result.is_err());
    }
}
